//! Ticket 78 (commit-candidate-set): derives the distinct set of paths
//! rokr touched this session, by reading the same `snapshot_paths.jsonl`
//! manifest `rokr_session::CheckpointStore` (tickets 38/39) already writes
//! for `/rollback` -- support module for `/commit` (ticket 79).
//!
//! `CheckpointStore` doesn't expose a public accessor for the manifest
//! path or its (private) `SnapshotPathEntry` shape, so this module
//! independently reconstructs the manifest path using the exact same
//! convention `CheckpointStore::open` uses --
//! `data_dir/sessions/<session_id>/snapshot_paths.jsonl` -- and defines its
//! own local struct mirroring the on-disk JSON-lines shape to deserialize
//! it.

use std::path::PathBuf;

use serde::Deserialize;

/// Mirrors the on-disk shape of `rokr_session::CheckpointStore`'s private
/// `SnapshotPathEntry` -- only `path` is used here, `snapshot_id` is kept
/// only so `serde` has a field to ignore extra data through, not read.
#[derive(Debug, Deserialize)]
struct SnapshotPathEntry {
    #[allow(dead_code)]
    snapshot_id: String,
    path: String,
}

/// Derives the distinct, sorted set of paths rokr wrote/edited (via
/// `write`/`edit` tool calls, per `CheckpointStore::snapshot`) during
/// session `session_id`, by reading `CheckpointStore::open(data_dir,
/// session_id)`'s `snapshot_paths.jsonl` manifest.
///
/// A session with no manifest yet (no write/edit ever checkpointed) yields
/// an empty set, matching `CheckpointStore::rollback_to`'s same-situation
/// behavior.
pub fn distinct_touched_paths(
    data_dir: impl Into<PathBuf>,
    session_id: &str,
) -> std::io::Result<Vec<String>> {
    let manifest_path = data_dir
        .into()
        .join("sessions")
        .join(session_id)
        .join("snapshot_paths.jsonl");

    match std::fs::read_to_string(&manifest_path) {
        Ok(contents) => {
            let paths: std::collections::BTreeSet<String> = contents
                .lines()
                .filter(|line| !line.trim().is_empty())
                .filter_map(|line| serde_json::from_str::<SnapshotPathEntry>(line).ok())
                .map(|entry| entry.path)
                .collect();
            Ok(paths.into_iter().collect())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_session_manifest_yields_empty_candidate_set() {
        let data_dir = tempfile::tempdir().expect("tempdir");

        let result = distinct_touched_paths(data_dir.path().to_path_buf(), "session-with-no-manifest")
            .expect("no manifest file is not an error");

        assert_eq!(result, Vec::<String>::new());
    }

    #[test]
    fn single_turn_multiple_snapshots_of_same_path_yield_one_entry() {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let store = rokr_session::CheckpointStore::open(data_dir.path().to_path_buf(), "sess-dup-write");

        // First write to "a.rs" at turn 0: real pre-image capture.
        store
            .snapshot(0, "a.rs", None)
            .expect("first snapshot call should succeed");
        // Second write to the SAME (turn_index, path) key within the same turn
        // (e.g. write then edit) -- CheckpointStore's first-write-wins means
        // this is a no-op that does not append a duplicate manifest line.
        store
            .snapshot(0, "a.rs", Some("content after the first write"))
            .expect("second snapshot call for the same key should also succeed (no-op)");

        let result = distinct_touched_paths(data_dir.path().to_path_buf(), "sess-dup-write")
            .expect("manifest exists and should parse");

        assert_eq!(result, vec!["a.rs".to_string()]);
    }

    #[test]
    fn distinct_paths_touched_across_multiple_turns_are_deduped() {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let store = rokr_session::CheckpointStore::open(data_dir.path().to_path_buf(), "sess-multi-turn");

        store
            .snapshot(0, "a.rs", None)
            .expect("turn 0 snapshot of a.rs should succeed");
        store
            .snapshot(1, "b.rs", None)
            .expect("turn 1 snapshot of b.rs should succeed");
        // Same path "a.rs" touched again, but at a DIFFERENT turn_index --
        // CheckpointStore's dedup key is (turn_index, path), so this appends a
        // SECOND, distinct manifest line for "a.rs" (not a no-op).
        store
            .snapshot(2, "a.rs", Some("a.rs content as of turn 2"))
            .expect("turn 2 snapshot of a.rs (different turn) should succeed");

        let result = distinct_touched_paths(data_dir.path().to_path_buf(), "sess-multi-turn")
            .expect("manifest exists and should parse");

        assert_eq!(result, vec!["a.rs".to_string(), "b.rs".to_string()]);
    }
}
