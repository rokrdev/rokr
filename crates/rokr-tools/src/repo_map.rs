//! Gitignore-aware, sorted, deterministic file-tree generation for a
//! session's orientation context (ticket 18: repo-map-generation).
//!
//! Deliberately a plain `pub fn`, not a `Tool` impl: this is orientation
//! infrastructure the agent never chooses to invoke (generated once per
//! session by `crates/rokr/src/main.rs` and threaded through
//! `rokr_core::run_tool_loop` into `rokr_core::context::assemble` as its own
//! context segment), so it stays entirely outside the tool/permission
//! machinery (`ToolError`, `Tool`, `PreviewableTool`).

use std::collections::BTreeMap;
use std::path::Path;

/// Default token budget for the rendered tree (~1-2k tokens per the
/// ticket). Converted to a char budget via the `chars/4` heuristic used
/// throughout this crate for cheap token estimation.
pub const DEFAULT_TOKEN_BUDGET: usize = 1500;

/// Number of characters the `chars/4` heuristic treats as one token.
const CHARS_PER_TOKEN: usize = 4;

/// A file-tree entry, already sorted (via the `BTreeMap` it's built from)
/// and gitignore-filtered. Kept as an in-memory tree (rather than a flat
/// sorted list of paths) so rendering can collapse an oversized directory's
/// remaining children into a single marker without needing to re-derive
/// nesting from indentation after the fact.
enum Node {
    File { name: String },
    Dir { name: String, children: Vec<Node> },
}

/// Intermediate builder shape: a directory's children keyed by name so
/// repeated inserts for the same path prefix land on the same node,
/// regardless of the order `ignore::Walk` yields entries in.
enum Building {
    File,
    Dir(BTreeMap<String, Building>),
}

/// Generates a gitignore-aware, sorted, deterministic file tree rooted at
/// `root`, held to [`DEFAULT_TOKEN_BUDGET`].
pub fn generate(root: &Path) -> String {
    generate_with_budget(root, DEFAULT_TOKEN_BUDGET)
}

/// Same as [`generate`], but with an explicit token budget — kept `pub` for
/// testability (a real repo's tree can be forced over budget deterministically
/// without needing hundreds of files) rather than hardcoding the constant.
pub fn generate_with_budget(root: &Path, budget_tokens: usize) -> String {
    let budget_chars = budget_tokens.saturating_mul(CHARS_PER_TOKEN) as i64;
    let children = build_tree(root);

    let mut remaining = budget_chars;
    let mut lines = Vec::new();
    render_children(&children, 0, &mut remaining, &mut lines);
    lines.join("\n")
}

/// Walks `root` with the `ignore` crate (respecting `.gitignore`, `.ignore`,
/// and standard VCS-ignore rules — `require_git(false)` so a plain temp dir
/// with a `.gitignore` but no `.git` directory still honors it) and folds
/// every entry into a nested, name-sorted tree.
fn build_tree(root: &Path) -> Vec<Node> {
    let mut root_map: BTreeMap<String, Building> = BTreeMap::new();

    let walker = ignore::WalkBuilder::new(root).require_git(false).build();
    for result in walker {
        let entry = match result {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        if path == root {
            continue;
        }
        let relative = match path.strip_prefix(root) {
            Ok(relative) => relative,
            Err(_) => continue,
        };
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        let components: Vec<String> = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect();
        insert_components(&mut root_map, &components, is_dir);
    }

    map_to_nodes(root_map)
}

/// Inserts one entry's path `components` into the nested builder map,
/// creating intermediate `Dir` entries as needed. Idempotent: re-inserting a
/// path already present (e.g. a directory visited again as an ancestor of a
/// later entry) is a no-op via `entry().or_insert_with(..)`.
fn insert_components(map: &mut BTreeMap<String, Building>, components: &[String], is_dir: bool) {
    let (head, rest) = match components.split_first() {
        Some(split) => split,
        None => return,
    };

    if rest.is_empty() {
        map.entry(head.clone()).or_insert_with(|| {
            if is_dir {
                Building::Dir(BTreeMap::new())
            } else {
                Building::File
            }
        });
        return;
    }

    let entry = map
        .entry(head.clone())
        .or_insert_with(|| Building::Dir(BTreeMap::new()));
    if let Building::Dir(sub) = entry {
        insert_components(sub, rest, is_dir);
    }
}

/// Converts the intermediate builder map into the final `Node` tree.
/// `BTreeMap` iteration is already key-sorted, so this is where the
/// "sorted, deterministic" guarantee comes from.
fn map_to_nodes(map: BTreeMap<String, Building>) -> Vec<Node> {
    map.into_iter()
        .map(|(name, building)| match building {
            Building::File => Node::File { name },
            Building::Dir(sub) => Node::Dir {
                name,
                children: map_to_nodes(sub),
            },
        })
        .collect()
}

/// Renders `children` (already sorted) at indentation level `depth`,
/// consuming `remaining` budget as it goes. As soon as budget runs out
/// partway through a directory's children, the rest of that directory's
/// children collapse into a single `"... (N more)"` marker (N = the count of
/// immediate children not rendered) instead of being truncated mid-listing —
/// the ticket's "no arbitrary truncation" requirement.
fn render_children(children: &[Node], depth: usize, remaining: &mut i64, out: &mut Vec<String>) {
    let indent = "  ".repeat(depth);

    for (index, child) in children.iter().enumerate() {
        if *remaining <= 0 {
            out.push(format!("{indent}... ({} more)", children.len() - index));
            return;
        }

        match child {
            Node::File { name } => {
                let line = format!("{indent}{name}");
                *remaining -= line.len() as i64 + 1; // +1 for the joining newline
                out.push(line);
            }
            Node::Dir {
                name,
                children: sub,
            } => {
                let line = format!("{indent}{name}/");
                *remaining -= line.len() as i64 + 1;
                out.push(line);

                if sub.is_empty() {
                    continue;
                }
                if *remaining <= 0 {
                    out.push(format!("{indent}  ... ({} more)", sub.len()));
                    continue;
                }
                render_children(sub, depth + 1, remaining, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_respects_gitignore_and_is_deterministic_across_runs() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let root = temp_dir.path();

        std::fs::create_dir_all(root.join("src")).expect("create src dir");
        std::fs::write(root.join("src/main.rs"), "fn main() {}").expect("write tracked file");
        std::fs::write(root.join(".gitignore"), "secret.txt\n").expect("write .gitignore");
        std::fs::write(root.join("secret.txt"), "top secret").expect("write gitignored file");

        let first = generate(root);
        let second = generate(root);

        assert!(
            !first.contains("secret.txt"),
            "gitignored file must not appear in first run's output, got: {first}"
        );
        assert!(
            !second.contains("secret.txt"),
            "gitignored file must not appear in second run's output, got: {second}"
        );
        assert!(
            first.contains("main.rs"),
            "tracked file must appear in first run's output, got: {first}"
        );
        assert!(
            second.contains("main.rs"),
            "tracked file must appear in second run's output, got: {second}"
        );
        assert_eq!(
            first, second,
            "generate() must produce byte-identical output across separate runs"
        );
    }

    /// Uses `generate_with_budget` with a small, explicit budget rather than
    /// scaling up to hundreds of files against `DEFAULT_TOKEN_BUDGET` — keeps
    /// the test fast and its trigger condition obvious, while still
    /// exercising the same collapse code path `generate()` calls into.
    #[test]
    fn generate_collapses_oversized_subtree_into_more_marker() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let root = temp_dir.path();

        let big_dir = root.join("big_subdir");
        std::fs::create_dir_all(&big_dir).expect("create big subdir");
        let file_count = 50;
        for i in 0..file_count {
            std::fs::write(
                big_dir.join(format!("generated_file_number_{i:04}.txt")),
                "contents",
            )
            .expect("write generated file");
        }

        // Small budget (tokens -> 200 chars), far below what 50 ~35-char
        // lines would take (~1750 chars), so the collapse path is
        // deterministically triggered without a huge fixture.
        let budget_tokens = 50;
        let output = generate_with_budget(root, budget_tokens);

        assert!(
            output.contains("more)"),
            "expected an oversized subtree to collapse into a \"... (N more)\" marker, got: {output}"
        );
        assert!(
            !output.contains("generated_file_number_0049.txt"),
            "the last generated file should have been collapsed away, not individually listed, \
             got: {output}"
        );

        let budget_chars = budget_tokens * CHARS_PER_TOKEN;
        let untruncated_len: usize = (0..file_count)
            .map(|i| format!("  generated_file_number_{i:04}.txt").len() + 1)
            .sum();
        assert!(
            output.len() < untruncated_len,
            "collapsed output ({} chars) should be far shorter than an untruncated listing \
             ({untruncated_len} chars)",
            output.len()
        );
        assert!(
            output.len() < budget_chars * 3,
            "collapsed output ({} chars) should stay within a sane multiple of the {budget_chars}-char \
             budget, not grow unbounded",
            output.len()
        );
    }
}
