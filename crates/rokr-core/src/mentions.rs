//! `@path` mention expansion (pure logic, no filesystem access).
//!
//! `@path` tokens typed in a user's prompt are expanded at submit time into
//! the same user message as delimited inline text — never as a synthetic
//! tool-role message, since at least one supported provider rejects an
//! orphan tool-role message on the wire (see the `at-mention-file-injection`
//! ticket). This module is pure: recognizing a token, deciding whether it's
//! worth resolving, formatting the injected block, and applying size caps
//! all happen here with zero filesystem access. The actual read is supplied
//! by the caller as the `resolve` closure (`crates/rokr/src/main.rs` backs
//! it with `std::fs::read_to_string`), so `rokr-core` never depends on
//! `rokr-tools` or the filesystem directly.

/// Outcome of attempting to resolve a path-shaped mention candidate.
pub enum MentionResolution {
    /// The path exists and was read; the file's contents.
    Found(String),
    /// The path does not exist (or otherwise could not be read).
    NotFound,
}

/// Per-file injection cap, in bytes. A single mentioned file's contents are
/// truncated to this many bytes (plus a truncation notice) before being
/// injected. 64 KiB is generous enough for the overwhelming majority of
/// source files a user would `@`-mention, while keeping any single mention
/// from dominating the request body / context window.
pub const MAX_MENTION_FILE_BYTES: usize = 64 * 1024;

/// Per-turn injection cap, in bytes, across every mention resolved within
/// one `expand_mentions` call. 256 KiB (4x the per-file cap) allows several
/// substantial files to be mentioned in the same prompt while still bounding
/// how much a single turn can inflate the outgoing request.
pub const MAX_MENTION_TURN_BYTES: usize = 256 * 1024;

/// Expands every `@`-mention in `text`, returning the expanded string.
///
/// An `@` at the start of `text` or preceded by whitespace, followed by a
/// run of non-whitespace characters, is a mention *candidate*. A candidate
/// is only "path-shaped" (worth calling `resolve` on) if its token contains
/// a `/` or a `.` — see the module doc for the judgment call this encodes.
/// Non-path-shaped candidates (e.g. `@support`) are left as literal text and
/// `resolve` is never called for them.
///
/// Per-file contents are truncated to [`MAX_MENTION_FILE_BYTES`] with a
/// truncation notice appended. Across the whole call, total injected bytes
/// are tracked against [`MAX_MENTION_TURN_BYTES`]; once that budget is
/// spent, further path-shaped mentions in the same turn get a "budget
/// exhausted" notice instead of their contents (resolve is still called for
/// them — a mention can still turn out `NotFound` — but a `Found` result is
/// no longer injected).
pub fn expand_mentions(text: &str, resolve: impl Fn(&str) -> MentionResolution) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    let mut preceded_by_boundary = true;
    let mut turn_bytes_injected: usize = 0;

    while let Some((idx, ch)) = chars.next() {
        if ch == '@' && preceded_by_boundary {
            let token_start = idx + ch.len_utf8();
            let mut token_end = token_start;
            while let Some(&(next_idx, next_ch)) = chars.peek() {
                if next_ch.is_whitespace() {
                    break;
                }
                token_end = next_idx + next_ch.len_utf8();
                chars.next();
            }
            let token = &text[token_start..token_end];

            output.push('@');
            output.push_str(token);

            // Only path-shaped candidates are worth a resolve attempt — a
            // token containing '/' or '.' looks like a path (relative,
            // absolute, or with a file extension); a bare word like
            // "@support" does not, and must be left as literal text with
            // `resolve` never invoked (see the module doc for the
            // reasoning behind this heuristic).
            if token.contains('/') || token.contains('.') {
                match resolve(token) {
                    MentionResolution::Found(contents) => {
                        if turn_bytes_injected >= MAX_MENTION_TURN_BYTES {
                            output.push_str(&format!(
                                " [skipped: per-turn mention budget of {MAX_MENTION_TURN_BYTES} bytes already reached, {token} not injected]"
                            ));
                        } else {
                            let (body, truncated_notice) =
                                truncate_to_cap(&contents, MAX_MENTION_FILE_BYTES);
                            turn_bytes_injected += body.len();
                            output.push_str(&format!("\n[Contents of {token}]\n```\n{body}"));
                            if let Some(notice) = truncated_notice {
                                output.push_str(&notice);
                            }
                            output.push_str("\n```\n");
                        }
                    }
                    MentionResolution::NotFound => {
                        output.push_str(&format!(" (file not found: {token})"));
                    }
                }
            }

            preceded_by_boundary = false;
        } else {
            output.push(ch);
            preceded_by_boundary = ch.is_whitespace();
        }
    }

    output
}

/// Truncates `contents` to at most `cap` bytes (respecting UTF-8 char
/// boundaries), returning the (possibly truncated) body plus an optional
/// notice string to append when truncation occurred.
fn truncate_to_cap(contents: &str, cap: usize) -> (&str, Option<String>) {
    if contents.len() <= cap {
        return (contents, None);
    }

    let mut boundary = cap;
    while boundary > 0 && !contents.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let body = &contents[..boundary];
    let notice = format!(
        "\n[truncated, showing {} of {} bytes]",
        body.len(),
        contents.len()
    );
    (body, Some(notice))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_valid_at_mention_to_file_contents() {
        let output = expand_mentions("look at @/tmp/some/file.txt please", |path| {
            assert_eq!(path, "/tmp/some/file.txt");
            MentionResolution::Found("some file contents".to_string())
        });

        assert!(
            output.contains("some file contents"),
            "expected expanded output to contain the resolved file contents, got: {output:?}"
        );
        assert!(
            output.contains("/tmp/some/file.txt"),
            "expected expanded output to contain the file path as a header, got: {output:?}"
        );
    }

    #[test]
    fn literal_at_sign_not_resolving_to_a_path_is_left_as_text() {
        let input = "reach out @support for help";

        let output = expand_mentions(input, |_| {
            panic!("resolver should not be called for non-path-shaped tokens")
        });

        assert_eq!(
            output, input,
            "a bare @word with no '/' or '.' is not path-shaped and must be left byte-identical"
        );
    }

    #[test]
    fn per_file_contents_exceeding_cap_are_truncated_with_notice() {
        let oversized = "x".repeat(MAX_MENTION_FILE_BYTES + 500);

        let output = expand_mentions("@/tmp/big.txt", |_| {
            MentionResolution::Found(oversized.clone())
        });

        assert!(
            output.contains("truncated"),
            "expected a truncation notice when file contents exceed the per-file cap, got a \
             {}-byte output",
            output.len()
        );
        assert!(
            output.len() < oversized.len() + 200,
            "expected the output to be meaningfully smaller than the untruncated contents, got \
             {} bytes vs {} original",
            output.len(),
            oversized.len()
        );
    }

    #[test]
    fn per_turn_budget_stops_injecting_further_file_contents() {
        // Five files, each larger than the per-file cap (so each is
        // truncated down to exactly MAX_MENTION_FILE_BYTES on injection).
        // Four of them exactly exhaust the per-turn budget
        // (4 * MAX_MENTION_FILE_BYTES == MAX_MENTION_TURN_BYTES), so the
        // fifth must be skipped with a clear notice rather than pushing the
        // turn over budget.
        let oversized = "y".repeat(MAX_MENTION_FILE_BYTES + 1000);
        let text = "@/tmp/f1.txt @/tmp/f2.txt @/tmp/f3.txt @/tmp/f4.txt @/tmp/f5.txt";

        let call_count = std::cell::Cell::new(0);
        let output = expand_mentions(text, |_path| {
            call_count.set(call_count.get() + 1);
            MentionResolution::Found(oversized.clone())
        });

        assert_eq!(
            call_count.get(),
            5,
            "resolve should still be called for every mention"
        );
        assert!(
            output.contains("skipped"),
            "expected a skipped/budget notice once the per-turn cap was reached, got output of \
             length {}",
            output.len()
        );
    }
}
