//! Session persistence as append-only JSONL, a metadata index, and resume/search support.

use rokr_core::Message;
use serde::{Deserialize, Serialize};

/// Mirrors [`rokr_core::Usage`] so it can derive `serde::{Serialize,
/// Deserialize}`, which `rokr_core::Usage` deliberately does not (rokr-core
/// stays serde-free outside `Message`/`ContentBlock`/`Role` per this
/// feature's PRD, which leaves rokr-core unchanged).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl From<rokr_core::Usage> for UsageRecord {
    fn from(usage: rokr_core::Usage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
        }
    }
}

impl From<UsageRecord> for rokr_core::Usage {
    fn from(record: UsageRecord) -> Self {
        Self {
            input_tokens: record.input_tokens,
            output_tokens: record.output_tokens,
            cache_read_tokens: record.cache_read_tokens,
            cache_write_tokens: record.cache_write_tokens,
        }
    }
}

/// A single record in a session's append-only JSONL log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionRecord {
    Header {
        schema_version: u32,
        session_id: String,
        created_at: String,
        project_path: String,
        agent_tier: String,
        provider: String,
        model: String,
    },
    Turn {
        message: Message,
        usage: UsageRecord,
        timestamp: String,
    },
    Compaction {
        summary: String,
        replaced_through: usize,
    },
    Rollback {
        target: usize,
    },
    Checkpoint {
        turn_index: usize,
        snapshot_id: String,
    },
}

/// Folds an ordered log of [`SessionRecord`]s into the messages a resumed
/// session should see, plus the most recently known [`rokr_core::Usage`].
///
/// See the module-level fold semantics: `Header`/`Checkpoint` are ignored;
/// `Turn` appends; `Compaction` collapses everything up to and including
/// `replaced_through` into one summary message; `Rollback` discards
/// everything above `target` from the *current* working output (it does not
/// rewind `next_turn_index`, so later genuinely-new turns still append).
pub fn fold(records: &[SessionRecord]) -> (Vec<Message>, Option<rokr_core::Usage>) {
    let mut output: Vec<(usize, Message)> = Vec::new();
    let mut next_turn_index: usize = 0;
    let mut last_known_usage: Option<rokr_core::Usage> = None;

    for record in records {
        match record {
            SessionRecord::Header { .. } | SessionRecord::Checkpoint { .. } => {}
            SessionRecord::Turn { message, usage, .. } => {
                output.push((next_turn_index, message.clone()));
                last_known_usage = Some((*usage).into());
                next_turn_index += 1;
            }
            SessionRecord::Compaction {
                summary,
                replaced_through,
            } => {
                let insert_at = output
                    .iter()
                    .position(|(turn_index, _)| *turn_index <= *replaced_through)
                    .unwrap_or(output.len());
                output.retain(|(turn_index, _)| *turn_index > *replaced_through);
                let summary_message = Message::user_text(format!(
                    "[Earlier conversation summary — compacted to save context]\n\n{summary}"
                ));
                output.insert(insert_at, (*replaced_through, summary_message));
            }
            SessionRecord::Rollback { target } => {
                output.retain(|(turn_index, _)| *turn_index <= *target);
            }
        }
    }

    let messages = output.into_iter().map(|(_, message)| message).collect();
    (messages, last_known_usage)
}

/// Metadata about a session, extracted from its log's `Header` record.
/// Returned by [`SessionStore::resume_session`] alongside the folded
/// transcript and [`ResumeState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMeta {
    pub session_id: String,
    pub created_at: String,
    pub project_path: String,
    pub agent_tier: String,
    pub provider: String,
    pub model: String,
}

/// State restored on resume that isn't itself part of the folded
/// transcript -- currently just the most recently known
/// [`rokr_core::Usage`], carried forward so a resumed session's
/// auto-compaction threshold check (and ticket 36's swap-warning check)
/// have the same figure a live session would have accumulated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeState {
    pub last_known_usage: Option<rokr_core::Usage>,
}

/// One denormalized metadata snapshot about a session, read from and
/// appended to the shared `sessions/index.jsonl` file (PRD decision 2: a
/// single index file is what `list_sessions`/`/sessions` read -- never
/// rebuilt by scanning every session directory). A session's own
/// `session.jsonl` log remains the sole source of truth for what `fold`
/// replays; this is purely a read-optimized view over it, appended
/// incrementally as sessions are created and updated rather than recomputed
/// from scratch on every list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIndexEntry {
    pub session_id: String,
    pub project_path: String,
    pub created_at: String,
    pub updated_at: String,
    /// The first user prompt, truncated. Empty until at least one `Turn`
    /// has been appended.
    pub title: String,
    /// Total number of `Turn` records appended so far (raw count, not the
    /// folded/post-compaction transcript length).
    pub turn_count: usize,
    /// The model recorded on the session's `Header` record.
    /// `SessionRecord::Turn` carries no per-turn model field today, so this
    /// does not reflect a mid-session `/model` switch -- a known
    /// limitation, out of this ticket's scope.
    pub last_model: String,
}

use std::path::PathBuf;

/// Commands sent over a session's single ordered writer channel (PRD
/// decision 1: "One ordered writer per store"). `Append` enqueues a record
/// without waiting for it to reach disk; `Flush` is a synchronization point
/// used by tests (and any future caller, e.g. a graceful-shutdown path) that
/// needs to know a prior `Append` has actually landed -- because the
/// channel preserves FIFO order, a `Flush` command is only processed after
/// every `Append` sent before it.
enum WriterCommand {
    Append(SessionRecord),
    Flush(tokio::sync::oneshot::Sender<()>),
}

/// Owns the write side of one session's on-disk `session.jsonl` log.
/// Constructed by [`SessionStore::create_session`]; every typed `append_*`
/// method enqueues onto an `mpsc` channel read by a single dedicated writer
/// task (spawned in `create_session`) that owns the open file handle --
/// never a direct file write from this handle, so concurrent callers
/// (a parent turn and a subagent's turns, per the PRD's Phase 4 note) can
/// never race on the file.
pub struct SessionHandle {
    session_id: String,
    tx: tokio::sync::mpsc::UnboundedSender<WriterCommand>,
}

impl SessionHandle {
    /// The ULID identifying this session's directory
    /// (`sessions/<session_id>/`).
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Enqueues a `Header` record. Intended to be called exactly once, at
    /// session creation.
    pub fn append_header(
        &self,
        schema_version: u32,
        session_id: String,
        created_at: String,
        project_path: String,
        agent_tier: String,
        provider: String,
        model: String,
    ) {
        self.enqueue(SessionRecord::Header {
            schema_version,
            session_id,
            created_at,
            project_path,
            agent_tier,
            provider,
            model,
        });
    }

    /// Enqueues a `Turn` record. Intended to be called once per submitted
    /// prompt, after that turn's reply and usage are known.
    pub fn append_turn(&self, message: Message, usage: UsageRecord, timestamp: String) {
        self.enqueue(SessionRecord::Turn {
            message,
            usage,
            timestamp,
        });
    }

    /// Enqueues a `Checkpoint` record. Intended to be called once per
    /// captured pre-image snapshot (ticket 38, checkpoint-pre-images),
    /// correlating `turn_index` with the `snapshot_id` a
    /// [`CheckpointStore::snapshot`] call just returned.
    pub fn append_checkpoint(&self, turn_index: usize, snapshot_id: String) {
        self.enqueue(SessionRecord::Checkpoint {
            turn_index,
            snapshot_id,
        });
    }

    /// Enqueues a `Rollback` record. Intended to be called once per
    /// `/rollback [turn]` command (ticket 39, rollback-command), AFTER
    /// `CheckpointStore::rollback_to` has already restored pre-images on
    /// disk -- the log stays append-only (this never rewrites an earlier
    /// line); `fold`'s existing `Rollback` handling (ticket 33) is what
    /// makes a later resume/jump replay the truncated transcript correctly.
    pub fn append_rollback(&self, target: usize) {
        self.enqueue(SessionRecord::Rollback { target });
    }

    fn enqueue(&self, record: SessionRecord) {
        // The writer task's receiver only disappears when the task itself
        // is gone (e.g. process shutdown mid-write) -- a dropped send here
        // is not this handle's problem to recover from; logging is a
        // follow-up concern, not required by this ticket's acceptance
        // criterion.
        let _ = self.tx.send(WriterCommand::Append(record));
    }

    /// Waits until every record enqueued before this call has actually been
    /// written to disk. Not on the hot path of a live session -- used by
    /// tests (and any future graceful-shutdown path) that need a
    /// synchronization point rather than the fire-and-forget behavior
    /// `append_header`/`append_turn` intentionally provide.
    pub async fn flush(&self) {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        if self.tx.send(WriterCommand::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.await;
        }
    }
}

/// Owns the on-disk root under which every session directory lives
/// (`<data_dir>/sessions/<ulid>/`).
#[derive(Clone)]
pub struct SessionStore {
    data_dir: PathBuf,
}

impl SessionStore {
    /// `data_dir` is the already-resolved central data directory (e.g.
    /// `$XDG_DATA_HOME/rokr`); sessions live under `data_dir/sessions/`.
    pub fn open(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    /// Shared by `create_session` and `open_session`: creates
    /// `sessions/<session_id>/` if needed, opens `session.jsonl` for
    /// append (creating it if it doesn't already exist), and spawns the
    /// single dedicated writer task that owns that file handle for the
    /// rest of this session's life. Returns a [`SessionHandle`] exposing
    /// only typed append methods -- callers never see the raw file or
    /// channel.
    fn open_handle_for(&self, session_id: String) -> std::io::Result<SessionHandle> {
        let session_dir = self.data_dir.join("sessions").join(&session_id);
        std::fs::create_dir_all(&session_dir)?;

        let session_file_path = session_dir.join("session.jsonl");
        // Ticket 36 (session-index-list-jump): seeded from whatever this
        // session's log already contains BEFORE opening it for append --
        // empty for a brand-new session, but for a resumed session (no
        // Header re-written, see `open_session`'s doc comment) this is how
        // the in-memory running index state inside `run_writer` picks up
        // the correct baseline turn_count/title/model rather than
        // resetting to zero and drifting from the true log contents.
        let initial_index_state = compute_initial_index_state(&session_file_path, &session_id);

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&session_file_path)?;

        // Ticket 36: a single shared file across every session in this
        // store (PRD decision 2) -- each session's own writer task appends
        // its own index snapshots to it. Concurrent appends from more than
        // one session's writer task are safe here: every write is one
        // complete JSON line + newline, well under the POSIX atomic
        // O_APPEND write size, so interleaved lines from different
        // sessions can never corrupt each other mid-line.
        let index_file_path = self.data_dir.join("sessions").join("index.jsonl");
        let index_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&index_file_path)?;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(run_writer(file, index_file, initial_index_state, rx));

        Ok(SessionHandle { session_id, tx })
    }

    /// Creates a new session: generates a ULID (chosen, per the PRD, so
    /// lexicographic directory sort order is also chronological order),
    /// creates `sessions/<ulid>/`, opens `session.jsonl` for append, and
    /// spawns the single dedicated writer task that owns that file handle
    /// for the rest of this session's life. Returns a [`SessionHandle`]
    /// exposing only typed append methods -- callers never see the raw
    /// file or channel.
    pub fn create_session(&self) -> std::io::Result<SessionHandle> {
        let session_id = ulid::Ulid::new().to_string();
        self.open_handle_for(session_id)
    }

    /// Opens an existing session's log for continued appends after a
    /// resume. No Header record is (re)written here -- it's already the
    /// first line of the log from when the session was originally created.
    pub fn open_session(&self, session_id: impl Into<String>) -> std::io::Result<SessionHandle> {
        self.open_handle_for(session_id.into())
    }

    /// Reads `session.jsonl` for `session_id`, parses each line as a
    /// `SessionRecord` (an unparseable/unknown-tag line is skipped with an
    /// `eprintln` warning rather than aborting the read, per the PRD's
    /// forward-compatibility policy), extracts [`SessionMeta`] from the
    /// `Header` record if present (falling back to a mostly-empty
    /// `SessionMeta` stamped with just `session_id` if no `Header` is
    /// found), and folds every record via [`fold`] into the resumed
    /// `Vec<Message>` plus a [`ResumeState`] carrying the restored
    /// `last_known_usage`.
    pub fn resume_session(
        &self,
        session_id: &str,
    ) -> std::io::Result<(Vec<Message>, SessionMeta, ResumeState)> {
        let session_jsonl_path = self
            .data_dir
            .join("sessions")
            .join(session_id)
            .join("session.jsonl");
        let contents = std::fs::read_to_string(&session_jsonl_path)?;

        let records: Vec<SessionRecord> = contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| match serde_json::from_str::<SessionRecord>(line) {
                Ok(record) => Some(record),
                Err(err) => {
                    eprintln!(
                        "warning: skipping unparseable session.jsonl line in \
                         {session_jsonl_path:?}: {err}"
                    );
                    None
                }
            })
            .collect();

        let meta = records
            .iter()
            .find_map(|record| match record {
                SessionRecord::Header {
                    session_id,
                    created_at,
                    project_path,
                    agent_tier,
                    provider,
                    model,
                    ..
                } => Some(SessionMeta {
                    session_id: session_id.clone(),
                    created_at: created_at.clone(),
                    project_path: project_path.clone(),
                    agent_tier: agent_tier.clone(),
                    provider: provider.clone(),
                    model: model.clone(),
                }),
                _ => None,
            })
            .unwrap_or_else(|| SessionMeta {
                session_id: session_id.to_string(),
                created_at: String::new(),
                project_path: String::new(),
                agent_tier: String::new(),
                provider: String::new(),
                model: String::new(),
            });

        let (messages, last_known_usage) = fold(&records);

        Ok((messages, meta, ResumeState { last_known_usage }))
    }

    /// Lists session ids under `data_dir/sessions/`, sorted lexicographically
    /// (ULIDs sort chronologically), returning the last (most recent) one.
    /// `Ok(None)` if the sessions directory doesn't exist yet.
    pub fn most_recent_session_id(&self) -> std::io::Result<Option<String>> {
        let sessions_dir = self.data_dir.join("sessions");
        let read_dir = match std::fs::read_dir(&sessions_dir) {
            Ok(read_dir) => read_dir,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };

        let mut session_ids: Vec<String> = read_dir
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        session_ids.sort();

        Ok(session_ids.into_iter().next_back())
    }

    /// Reads `sessions/index.jsonl` (PRD decision 2: the sole source
    /// `/sessions` and jump-by-id consult -- never a scan of every session
    /// directory), keeping only the most recent snapshot per `session_id`
    /// since it's an append-only log of snapshots and a later line for the
    /// same id always supersedes an earlier one. Returned sorted by
    /// `session_id` (ULIDs sort chronologically, so this also sorts by
    /// creation order). `Ok(Vec::new())` if the index doesn't exist yet.
    pub fn list_sessions(&self) -> std::io::Result<Vec<SessionIndexEntry>> {
        let index_path = self.data_dir.join("sessions").join("index.jsonl");
        let contents = match std::fs::read_to_string(&index_path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };

        let mut by_id: std::collections::BTreeMap<String, SessionIndexEntry> =
            std::collections::BTreeMap::new();
        for line in contents.lines().filter(|line| !line.trim().is_empty()) {
            if let Ok(entry) = serde_json::from_str::<SessionIndexEntry>(line) {
                by_id.insert(entry.session_id.clone(), entry);
            }
        }

        Ok(by_id.into_values().collect())
    }

    /// Lazily scans every session's own on-disk `session.jsonl` body for a
    /// case-sensitive substring match against `term` (PRD decision 2: full-
    /// text search is an on-demand scan at search time -- no persisted or
    /// maintained secondary search index). Deliberately does not consult
    /// `sessions/index.jsonl`: that cache never carries `Compaction`
    /// summary text, so a term that exists solely inside a `Compaction`
    /// summary (not any live `Turn`) would be invisible to an index-based
    /// lookup -- this scans each session's real log instead, the same way
    /// `resume_session` does. Session ids are discovered by listing
    /// `data_dir/sessions/` directories, mirroring
    /// `most_recent_session_id`. An unparseable line is skipped with an
    /// `eprintln` warning, same policy as `resume_session`. Returned in
    /// session-id sort order (ULIDs sort chronologically). `Ok(Vec::new())`
    /// if the sessions directory doesn't exist yet.
    pub fn search(&self, term: &str) -> std::io::Result<Vec<String>> {
        let sessions_dir = self.data_dir.join("sessions");
        let read_dir = match std::fs::read_dir(&sessions_dir) {
            Ok(read_dir) => read_dir,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };

        let mut session_ids: Vec<String> = read_dir
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        session_ids.sort();

        let mut matches = Vec::new();
        for session_id in session_ids {
            let session_jsonl_path = sessions_dir.join(&session_id).join("session.jsonl");
            let contents = match std::fs::read_to_string(&session_jsonl_path) {
                Ok(contents) => contents,
                Err(_) => continue,
            };

            let found = contents
                .lines()
                .filter(|line| !line.trim().is_empty())
                .filter_map(|line| match serde_json::from_str::<SessionRecord>(line) {
                    Ok(record) => Some(record),
                    Err(err) => {
                        eprintln!(
                            "warning: skipping unparseable session.jsonl line in \
                             {session_jsonl_path:?}: {err}"
                        );
                        None
                    }
                })
                .any(|record| match record {
                    SessionRecord::Turn { message, .. } => message.text().contains(term),
                    SessionRecord::Compaction { summary, .. } => summary.contains(term),
                    _ => false,
                });

            if found {
                matches.push(session_id);
            }
        }

        Ok(matches)
    }
}

/// Copy-on-write pre-image capture for a single session's write/edit tool
/// calls (ticket 38, checkpoint-pre-images; PRD phase-5-session-management
/// decision 4). Deliberately NOT a shadow git repo -- at the moment a
/// write/edit tool call is about to execute, the prior file content (the
/// `old` side of the permission-preview diff, already computed for that
/// prompt and reused here rather than re-read) is captured under
/// `sessions/<id>/snapshots/`, keyed by `(turn_index, path)`. Bash-driven
/// mutations are explicitly out of scope -- only `write`/`edit` have a
/// well-defined pre-image at a clean boundary (the permission-decision
/// point in `crates/rokr/src/main.rs`).
pub struct CheckpointStore {
    snapshots_dir: PathBuf,
    /// Ticket 39 (rollback-command): `snapshot_id`'s sanitized-path
    /// component is lossy (every non-alphanumeric char, including path
    /// separators, collapses to `_`), so the real path can't be recovered
    /// from `snapshot_id` alone -- this append-only manifest (one JSON line
    /// per newly-captured snapshot) is where the real path survives, so
    /// `rollback_to` can look it up. Deliberately lives OUTSIDE
    /// `snapshots_dir` (a sibling of `session.jsonl`, not inside
    /// `snapshots/`) so it doesn't disturb ticket 38's existing "exactly
    /// one file under snapshots/ after one capture" test assertions.
    manifest_path: PathBuf,
}

/// One line of `CheckpointStore`'s path manifest (ticket 39,
/// rollback-command) -- correlates a `snapshot_id` (as also embedded in a
/// `Checkpoint` record) with the real, un-sanitized path it was captured
/// from.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotPathEntry {
    snapshot_id: String,
    path: String,
}

impl CheckpointStore {
    /// `data_dir` is the same already-resolved central data directory
    /// [`SessionStore::open`] takes; snapshots for `session_id` live under
    /// `data_dir/sessions/<session_id>/snapshots/`, and the path manifest
    /// (ticket 39) lives alongside them at
    /// `data_dir/sessions/<session_id>/snapshot_paths.jsonl`.
    pub fn open(data_dir: impl Into<PathBuf>, session_id: &str) -> Self {
        let session_dir = data_dir.into().join("sessions").join(session_id);
        Self {
            snapshots_dir: session_dir.join("snapshots"),
            manifest_path: session_dir.join("snapshot_paths.jsonl"),
        }
    }

    /// Writes `old_content`'s pre-image for `(turn_index, path)` under this
    /// session's `snapshots/` directory (created if needed), returning
    /// `(snapshot_id, newly_written)`: `snapshot_id` is what a
    /// [`SessionHandle::append_checkpoint`] call can correlate with
    /// `turn_index` in a `Checkpoint` record; `newly_written` is `true` only
    /// the FIRST time this `(turn_index, path)` key is captured (see
    /// "First-write-wins" below) -- callers should only append a
    /// `Checkpoint` record when it's `true`, to avoid appending a
    /// duplicate-`snapshot_id` `Checkpoint` record for a mutation that wrote
    /// no new snapshot.
    ///
    /// `old_content` is `Some(content)` for a pre-existing file (including a
    /// genuinely empty one -- `Some("")`) and `None` for a brand-new file
    /// write, which has no pre-image at all. These two cases are stored
    /// distinguishably on disk: `Some(content)` writes `content` verbatim to
    /// the snapshot's content file (so a future rollback ticket can restore
    /// it byte-for-byte); `None` writes no content file at all, only a
    /// `<snapshot_id>.absent` marker file, so a future rollback ticket can
    /// tell "restore this content" apart from "this file didn't exist,
    /// delete it" -- an empty-string content file alone would be ambiguous
    /// between the two.
    ///
    /// First-write-wins (ticket 38 scope-amendment, F-001 per argus review):
    /// a turn's tool loop can mutate the SAME path more than once (e.g.
    /// `write` then `edit`) -- only the FIRST call for a given
    /// `(turn_index, path)` key actually writes anything; a later call for
    /// the same key is a no-op (`newly_written: false`) that just returns
    /// the already-captured snapshot_id, since that first call already
    /// holds the true turn-start pre-image (a later call's `old_content` is
    /// already the post-first-mutation content, not the real pre-turn
    /// state). This also prevents an absent-marker and a content file from
    /// ever coexisting for the same snapshot_id (e.g. a brand-new-file
    /// write followed by a second write to the same now-existing path
    /// within the same turn).
    ///
    /// Ticket 39 (rollback-command): also appends a `SnapshotPathEntry` line
    /// to this session's path manifest on the SAME first-write-wins
    /// condition (never for a no-op repeat call), so `rollback_to` can later
    /// recover the real path this snapshot_id was captured from.
    pub fn snapshot(
        &self,
        turn_index: usize,
        path: &str,
        old_content: Option<&str>,
    ) -> std::io::Result<(String, bool)> {
        std::fs::create_dir_all(&self.snapshots_dir)?;

        let snapshot_id = Self::snapshot_id(turn_index, path);
        let content_path = self.snapshots_dir.join(&snapshot_id);
        let absent_marker_path = self.snapshots_dir.join(format!("{snapshot_id}.absent"));

        if content_path.exists() || absent_marker_path.exists() {
            return Ok((snapshot_id, false));
        }

        {
            use std::io::Write;
            let manifest_line = serde_json::to_string(&SnapshotPathEntry {
                snapshot_id: snapshot_id.clone(),
                path: path.to_string(),
            })
            .expect("serializing SnapshotPathEntry never fails");
            let mut manifest_file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.manifest_path)?;
            writeln!(manifest_file, "{manifest_line}")?;
        }

        match old_content {
            Some(content) => {
                std::fs::write(content_path, content)?;
            }
            None => {
                std::fs::write(absent_marker_path, "")?;
            }
        }

        Ok((snapshot_id, true))
    }

    /// Ticket 39 (rollback-command), PRD decision 4: restores every
    /// captured pre-image at turn indices >= `target_turn`, in
    /// reverse-chronological order (highest turn_index first), so that for
    /// a path snapshotted at multiple qualifying turns the FINAL restored
    /// content on disk is the EARLIEST one (closest to `target_turn`) --
    /// the state the path was in right before `target_turn`'s own mutation
    /// ran. A snapshot captured with no pre-image (a `.absent` marker -- a
    /// brand-new file at capture time) deletes the path instead of writing
    /// content (a missing path at delete time is not an error -- it may
    /// already be gone). Returns the distinct, sorted set of paths actually
    /// touched. `Ok(Vec::new())` if this session has no path manifest yet
    /// (no write/edit tool call was ever checkpointed).
    pub fn rollback_to(&self, target_turn: usize) -> std::io::Result<Vec<String>> {
        let contents = match std::fs::read_to_string(&self.manifest_path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };

        let mut entries: Vec<(usize, SnapshotPathEntry)> = contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<SnapshotPathEntry>(line).ok())
            .filter_map(|entry| {
                Self::parse_turn_index(&entry.snapshot_id).map(|turn_index| (turn_index, entry))
            })
            .filter(|(turn_index, _)| *turn_index >= target_turn)
            .collect();

        // Reverse-chronological: highest turn_index restored first, so a
        // lower turn_index restore for the SAME path (applied later in this
        // loop) overwrites it and is the final state left on disk -- the
        // pre-image closest to target_turn.
        entries.sort_by(|a, b| b.0.cmp(&a.0));

        let mut touched = std::collections::BTreeSet::new();
        for (_, entry) in &entries {
            let content_path = self.snapshots_dir.join(&entry.snapshot_id);
            let absent_marker_path =
                self.snapshots_dir.join(format!("{}.absent", entry.snapshot_id));

            if absent_marker_path.exists() {
                match std::fs::remove_file(&entry.path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err),
                }
            } else {
                let content = std::fs::read_to_string(&content_path)?;
                if let Some(parent) = std::path::Path::new(&entry.path).parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&entry.path, content)?;
            }
            touched.insert(entry.path.clone());
        }

        Ok(touched.into_iter().collect())
    }

    /// Deterministically derives a filesystem-safe snapshot id from
    /// `(turn_index, path)`: every non-alphanumeric character in `path` is
    /// replaced with `_`, prefixed with the turn index, so two different
    /// `(turn_index, path)` pairs never collide (short of `path` itself
    /// containing only characters that sanitize to the same string, which
    /// none of this codebase's real paths do) and the id stays readable for
    /// debugging on disk.
    fn snapshot_id(turn_index: usize, path: &str) -> String {
        let sanitized_path: String = path
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        format!("t{turn_index}-{sanitized_path}")
    }

    /// Ticket 39 (rollback-command): parses the leading `turn_index` back
    /// out of a `t{turn_index}-...` snapshot_id. The sanitized path suffix
    /// never contains a literal `-` (sanitization maps every
    /// non-alphanumeric character, including `-`, to `_`), so the FIRST `-`
    /// after the `t` prefix is always exactly the turn_index/path
    /// separator.
    fn parse_turn_index(snapshot_id: &str) -> Option<usize> {
        let rest = snapshot_id.strip_prefix('t')?;
        let (digits, _) = rest.split_once('-')?;
        digits.parse().ok()
    }
}

/// Cross-session prompt recall (ticket 40, prompt-history; PRD decision 5):
/// a single append-only log at `$XDG_DATA_HOME/rokr/history`, separate from
/// any one session's own `session.jsonl` -- a prompt typed in one session is
/// just as useful to recall in another, so unlike everything else in this
/// file, this isn't scoped to a session id at all. `rokr-tui`'s Up/Down
/// recall (ticket 40) only ever sees the loaded `Vec<String>` primitive
/// `load` hands back, never this type itself, per the PRD's crate-boundary
/// decision (rokr-tui must not depend on rokr-session).
pub struct PromptHistory;

impl PromptHistory {
    /// How many entries `load` returns at most -- once the file holds more
    /// than this many lines, `load` returns only the most recent
    /// `MAX_ENTRIES` (oldest-first within that window), and `append` trims
    /// the on-disk file back down to this bound once it's just been
    /// exceeded. Picked as a round number comfortably larger than any
    /// realistic day's worth of prompts while keeping the file (and the
    /// one-time startup read) small -- not a PRD-specified figure, this
    /// ticket's own implementation choice.
    pub const MAX_ENTRIES: usize = 1000;

    fn history_path(data_dir: &std::path::Path) -> PathBuf {
        data_dir.join("history")
    }

    /// Reads `$data_dir/history`, one entry per line (each line
    /// escaped/unescaped via `encode_entry`/`decode_entry` so a prompt
    /// containing a literal newline round-trips through this line-oriented
    /// format -- ticket 41, multiline-input, is what actually lets a user
    /// type one, but this ticket's storage format must not corrupt it if it
    /// arrives), returned oldest-first. `Ok(Vec::new())` if the file doesn't
    /// exist yet (no prompt has ever been submitted). If the file holds more
    /// than `MAX_ENTRIES` lines, only the most recent `MAX_ENTRIES` are
    /// returned.
    pub fn load(data_dir: impl AsRef<std::path::Path>) -> std::io::Result<Vec<String>> {
        let path = Self::history_path(data_dir.as_ref());
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };

        let mut entries: Vec<String> = contents
            .lines()
            .filter(|line| !line.is_empty())
            .map(decode_entry)
            .collect();

        if entries.len() > Self::MAX_ENTRIES {
            let excess = entries.len() - Self::MAX_ENTRIES;
            entries.drain(0..excess);
        }

        Ok(entries)
    }

    /// Appends `entry` as a new line to `$data_dir/history` (creating
    /// `data_dir` and the file if needed). A plain, fast `O(1)` append in
    /// the common case -- this only pays the cost of a full read+rewrite
    /// when the file has just grown past `MAX_ENTRIES` lines, trimming it
    /// back down to the most recent `MAX_ENTRIES`; since that bound is a
    /// small, fixed constant, the occasional rewrite is negligible, and this
    /// is what keeps the file's on-disk size bounded (decision 5: "capped at
    /// a bounded size") rather than growing forever the way a session's own
    /// `session.jsonl` deliberately does (decision 1's accepted trade-off).
    pub fn append(data_dir: impl AsRef<std::path::Path>, entry: &str) -> std::io::Result<()> {
        let data_dir = data_dir.as_ref();
        std::fs::create_dir_all(data_dir)?;
        let path = Self::history_path(data_dir);

        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            writeln!(file, "{}", encode_entry(entry))?;
        }

        let line_count = std::fs::read_to_string(&path)?
            .lines()
            .filter(|line| !line.is_empty())
            .count();
        if line_count > Self::MAX_ENTRIES {
            let trimmed = Self::load(data_dir)?; // already caps at MAX_ENTRIES
            let rewritten = trimmed
                .iter()
                .map(|entry| encode_entry(entry))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            std::fs::write(&path, rewritten)?;
        }

        Ok(())
    }
}

/// Escapes a single history entry so it survives round-tripping through
/// `PromptHistory`'s line-oriented file format even if it contains a literal
/// newline or carriage return (ticket 41's multiline input is what actually
/// lets a user type one) -- backslash is escaped first so the scheme is
/// unambiguous to reverse, then real `\n`/`\r` characters become the
/// two-character escapes `\n`/`\r` (a literal backslash followed by the
/// letter), never a real line break.
fn encode_entry(entry: &str) -> String {
    entry
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Reverses `encode_entry`.
fn decode_entry(line: &str) -> String {
    let mut result = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('\\') => result.push('\\'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// In-memory running state `run_writer` (ticket 36) keeps for the shared
/// index, updated on every `Header`/`Turn` write and re-serialized as a
/// [`SessionIndexEntry`] snapshot appended to `sessions/index.jsonl`.
#[derive(Debug, Clone)]
struct IndexState {
    session_id: String,
    project_path: String,
    created_at: String,
    updated_at: String,
    title: Option<String>,
    turn_count: usize,
    last_model: String,
}

impl IndexState {
    fn to_entry(&self) -> SessionIndexEntry {
        SessionIndexEntry {
            session_id: self.session_id.clone(),
            project_path: self.project_path.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            title: self.title.clone().unwrap_or_default(),
            turn_count: self.turn_count,
            last_model: self.last_model.clone(),
        }
    }
}

/// How long a title (the first user prompt) is allowed to get before
/// `/sessions`' listing truncates it -- picked to keep a one-line-per-
/// session listing readable, not from any PRD-specified figure.
const TITLE_MAX_CHARS: usize = 60;

fn truncate_title(text: &str) -> String {
    if text.chars().count() > TITLE_MAX_CHARS {
        let truncated: String = text.chars().take(TITLE_MAX_CHARS).collect();
        format!("{truncated}...")
    } else {
        text.to_string()
    }
}

/// Scans `session_file_path`'s existing contents (if any) into the baseline
/// [`IndexState`] a freshly-spawned `run_writer` should start from -- empty
/// for a brand-new session (the file doesn't exist yet), or the true
/// accumulated state for a resumed session, so a newly appended `Turn`
/// continues incrementing `turn_count` from the correct number rather than
/// resetting to zero and drifting from what `session.jsonl` actually
/// contains.
fn compute_initial_index_state(
    session_file_path: &std::path::Path,
    session_id: &str,
) -> IndexState {
    let mut state = IndexState {
        session_id: session_id.to_string(),
        project_path: String::new(),
        created_at: String::new(),
        updated_at: String::new(),
        title: None,
        turn_count: 0,
        last_model: String::new(),
    };

    let contents = match std::fs::read_to_string(session_file_path) {
        Ok(contents) => contents,
        Err(_) => return state,
    };

    for line in contents.lines().filter(|line| !line.trim().is_empty()) {
        if let Ok(record) = serde_json::from_str::<SessionRecord>(line) {
            match record {
                SessionRecord::Header {
                    project_path,
                    created_at,
                    model,
                    ..
                } => {
                    state.project_path = project_path;
                    state.updated_at = created_at.clone();
                    state.created_at = created_at;
                    state.last_model = model;
                }
                SessionRecord::Turn {
                    message, timestamp, ..
                } => {
                    state.turn_count += 1;
                    if state.title.is_none() {
                        state.title = Some(truncate_title(&message.text()));
                    }
                    state.updated_at = timestamp;
                }
                _ => {}
            }
        }
    }

    state
}

/// The single dedicated writer task (PRD decision 1: "One ordered writer
/// per store"). Owns `file` for its entire lifetime; every `Append` is
/// serialized to one JSON line and written in the order it was enqueued,
/// so a parent's and a subagent's turns interleave correctly without any
/// file lock. Ticket 36: also owns `index_file`, the shared cross-session
/// index -- every `Header`/`Turn` append additionally refreshes the
/// in-memory `index_state` and appends a snapshot line to it.
async fn run_writer(
    file: std::fs::File,
    index_file: std::fs::File,
    mut index_state: IndexState,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<WriterCommand>,
) {
    use tokio::io::AsyncWriteExt;

    let mut file = tokio::fs::File::from_std(file);
    let mut index_file = tokio::fs::File::from_std(index_file);
    while let Some(command) = rx.recv().await {
        match command {
            WriterCommand::Append(record) => {
                if let Ok(mut line) = serde_json::to_string(&record) {
                    line.push('\n');
                    let _ = file.write_all(line.as_bytes()).await;
                }

                // A Compaction/Rollback/Checkpoint record leaves the index
                // untouched -- none of them changes project_path/created_at/
                // model, and this ticket doesn't ask the index's turn_count
                // to track the folded (post-compaction/rollback) transcript
                // length, only the raw count of Turn records appended.
                let index_changed = match &record {
                    SessionRecord::Header {
                        project_path,
                        created_at,
                        model,
                        ..
                    } => {
                        index_state.project_path = project_path.clone();
                        index_state.created_at = created_at.clone();
                        index_state.updated_at = created_at.clone();
                        index_state.last_model = model.clone();
                        true
                    }
                    SessionRecord::Turn {
                        message, timestamp, ..
                    } => {
                        index_state.turn_count += 1;
                        if index_state.title.is_none() {
                            index_state.title = Some(truncate_title(&message.text()));
                        }
                        index_state.updated_at = timestamp.clone();
                        true
                    }
                    _ => false,
                };

                if index_changed {
                    if let Ok(mut line) = serde_json::to_string(&index_state.to_entry()) {
                        line.push('\n');
                        let _ = index_file.write_all(line.as_bytes()).await;
                    }
                }
            }
            WriterCommand::Flush(ack) => {
                let _ = file.flush().await;
                let _ = index_file.flush().await;
                let _ = ack.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_turn_compaction_rollback_checkpoint_records_round_trip_through_json() {
        let header = SessionRecord::Header {
            schema_version: 1,
            session_id: "sess-123".to_string(),
            created_at: "2026-07-20T00:00:00Z".to_string(),
            project_path: "/Users/bharat/projects/rokr".to_string(),
            agent_tier: "sonnet".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-sonnet-5".to_string(),
        };
        let turn = SessionRecord::Turn {
            message: Message::user_text("hello world"),
            usage: UsageRecord {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            timestamp: "2026-07-20T00:00:01Z".to_string(),
        };
        let compaction = SessionRecord::Compaction {
            summary: "earlier discussion summarized".to_string(),
            replaced_through: 4,
        };
        let rollback = SessionRecord::Rollback { target: 2 };
        let checkpoint = SessionRecord::Checkpoint {
            turn_index: 3,
            snapshot_id: "snap-abc".to_string(),
        };

        for original in [header, turn, compaction, rollback, checkpoint] {
            let json = serde_json::to_string(&original).expect("serialize SessionRecord");
            let restored: SessionRecord =
                serde_json::from_str(&json).expect("deserialize SessionRecord");
            assert_eq!(restored, original);
        }
    }

    fn usage(input_tokens: u64) -> UsageRecord {
        UsageRecord {
            input_tokens,
            output_tokens: input_tokens * 2,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        }
    }

    #[test]
    fn fold_of_turn_only_sequence_matches_appended_messages() {
        let records = vec![
            SessionRecord::Turn {
                message: Message::user_text("first"),
                usage: usage(1),
                timestamp: "t0".to_string(),
            },
            SessionRecord::Turn {
                message: Message::assistant_text("second"),
                usage: usage(2),
                timestamp: "t1".to_string(),
            },
            SessionRecord::Turn {
                message: Message::user_text("third"),
                usage: usage(3),
                timestamp: "t2".to_string(),
            },
        ];

        let (messages, last_usage) = fold(&records);

        assert_eq!(
            messages,
            vec![
                Message::user_text("first"),
                Message::assistant_text("second"),
                Message::user_text("third"),
            ]
        );
        assert_eq!(last_usage, Some(rokr_core::Usage::from(usage(3))));
    }

    #[test]
    fn fold_collapses_records_before_compaction_replaced_through_index() {
        let records = vec![
            SessionRecord::Turn {
                message: Message::user_text("turn0"),
                usage: usage(0),
                timestamp: "t0".to_string(),
            },
            SessionRecord::Turn {
                message: Message::assistant_text("turn1"),
                usage: usage(1),
                timestamp: "t1".to_string(),
            },
            SessionRecord::Turn {
                message: Message::user_text("turn2"),
                usage: usage(2),
                timestamp: "t2".to_string(),
            },
            SessionRecord::Compaction {
                summary: "summary of turns 0-2".to_string(),
                replaced_through: 2,
            },
            SessionRecord::Turn {
                message: Message::assistant_text("turn3"),
                usage: usage(3),
                timestamp: "t3".to_string(),
            },
            SessionRecord::Turn {
                message: Message::user_text("turn4"),
                usage: usage(4),
                timestamp: "t4".to_string(),
            },
        ];

        let (messages, last_usage) = fold(&records);

        assert_eq!(
            messages,
            vec![
                Message::user_text(
                    "[Earlier conversation summary — compacted to save context]\n\nsummary of turns 0-2"
                ),
                Message::assistant_text("turn3"),
                Message::user_text("turn4"),
            ]
        );
        assert_eq!(last_usage, Some(rokr_core::Usage::from(usage(4))));
    }

    #[test]
    fn fold_truncates_output_at_rollback_target_index_discarding_later_turns() {
        let records = vec![
            SessionRecord::Turn {
                message: Message::user_text("turn0"),
                usage: usage(0),
                timestamp: "t0".to_string(),
            },
            SessionRecord::Turn {
                message: Message::assistant_text("turn1"),
                usage: usage(1),
                timestamp: "t1".to_string(),
            },
            SessionRecord::Turn {
                message: Message::user_text("turn2"),
                usage: usage(2),
                timestamp: "t2".to_string(),
            },
            SessionRecord::Turn {
                message: Message::assistant_text("turn3"),
                usage: usage(3),
                timestamp: "t3".to_string(),
            },
            SessionRecord::Rollback { target: 1 },
        ];

        let (messages, last_usage) = fold(&records);

        assert_eq!(
            messages,
            vec![Message::user_text("turn0"), Message::assistant_text("turn1")]
        );
        // last_known_usage tracks the most recent Turn record *processed* in
        // log order, not the post-rollback output: Rollback trims the
        // working output but the fold has already processed turn3's usage
        // by the time it reaches the Rollback record.
        assert_eq!(last_usage, Some(rokr_core::Usage::from(usage(3))));
    }

    #[test]
    fn fold_handles_compaction_followed_by_rollback_together() {
        let records = vec![
            SessionRecord::Turn {
                message: Message::user_text("turn0"),
                usage: usage(0),
                timestamp: "t0".to_string(),
            },
            SessionRecord::Turn {
                message: Message::assistant_text("turn1"),
                usage: usage(1),
                timestamp: "t1".to_string(),
            },
            SessionRecord::Compaction {
                summary: "summary of turns 0-1".to_string(),
                replaced_through: 1,
            },
            SessionRecord::Turn {
                message: Message::user_text("turn2"),
                usage: usage(2),
                timestamp: "t2".to_string(),
            },
            SessionRecord::Turn {
                message: Message::assistant_text("turn3"),
                usage: usage(3),
                timestamp: "t3".to_string(),
            },
            SessionRecord::Turn {
                message: Message::user_text("turn4"),
                usage: usage(4),
                timestamp: "t4".to_string(),
            },
            SessionRecord::Rollback { target: 3 },
        ];

        let (messages, last_usage) = fold(&records);

        assert_eq!(
            messages,
            vec![
                Message::user_text(
                    "[Earlier conversation summary — compacted to save context]\n\nsummary of turns 0-1"
                ),
                Message::user_text("turn2"),
                Message::assistant_text("turn3"),
            ]
        );
        assert_eq!(last_usage, Some(rokr_core::Usage::from(usage(4))));
    }

    /// Ticket 34 (persist-new-sessions): proves the writer task actually
    /// exists and actually writes -- not just that the types compile.
    /// Appends a Header then a Turn through the typed handle, calls
    /// `flush().await` to synchronize with the writer task (since
    /// `append_*` intentionally never blocks on IO), then reads
    /// `session.jsonl` directly off disk and asserts both records landed,
    /// in order, and round-trip through `SessionRecord`'s own
    /// deserialization.
    #[tokio::test]
    async fn session_store_create_session_spawns_writer_task_and_appends_via_typed_handle() {
        let dir = unique_temp_dir("session-store");
        let store = SessionStore::open(&dir);
        let handle = store
            .create_session()
            .expect("create_session should succeed against a fresh temp dir");

        handle.append_header(
            1,
            handle.session_id().to_string(),
            "2026-07-20T00:00:00Z".to_string(),
            "/some/project".to_string(),
            "build".to_string(),
            "anthropic".to_string(),
            "claude-test".to_string(),
        );
        handle.append_turn(
            Message::user_text("hello session store"),
            UsageRecord {
                input_tokens: 1,
                output_tokens: 2,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            "2026-07-20T00:00:01Z".to_string(),
        );

        handle.flush().await;

        let session_file = dir
            .join("sessions")
            .join(handle.session_id())
            .join("session.jsonl");
        let contents = std::fs::read_to_string(&session_file)
            .expect("session.jsonl should exist after flush");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "expected exactly a Header record followed by a Turn record, got: {contents:?}"
        );

        let first: SessionRecord =
            serde_json::from_str(lines[0]).expect("first line should deserialize");
        let second: SessionRecord =
            serde_json::from_str(lines[1]).expect("second line should deserialize");

        assert!(
            matches!(first, SessionRecord::Header { .. }),
            "expected first record to be Header, got: {first:?}"
        );
        match second {
            SessionRecord::Turn { message, .. } => {
                assert_eq!(message.text(), "hello session store");
            }
            other => panic!("expected second record to be Turn, got: {other:?}"),
        }
    }

    /// Mirrors `crates/rokr/src/main.rs`'s own `unique_temp_dir` test
    /// helper -- a fresh, uniquely-named directory under the system temp
    /// dir, so this test never touches a shared path another test (or a
    /// parallel run) might also be using.
    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "rokr-session-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Ticket 35 (resume-session): hand-builds a `session.jsonl` fixture
    /// (via real `SessionRecord` values serialized with `serde_json`, never
    /// hand-typed JSON) containing a `Header`, several `Turn`s, a
    /// `Compaction`, and a `Rollback` -- proving `SessionStore::resume_session`
    /// actually delegates to the real `fold()` function (reusing the same
    /// record shape as `fold_handles_compaction_followed_by_rollback_together`
    /// above) rather than reimplementing folding logic ad hoc, and that it
    /// correctly extracts `SessionMeta` from the `Header` record.
    #[test]
    fn resume_session_folds_log_into_messages_and_restores_last_known_usage() {
        let dir = unique_temp_dir("resume-session");
        let session_id = "resume-test-session".to_string();
        let session_dir = dir.join("sessions").join(&session_id);
        std::fs::create_dir_all(&session_dir).unwrap();

        let header = SessionRecord::Header {
            schema_version: 1,
            session_id: session_id.clone(),
            created_at: "2026-07-20T00:00:00Z".to_string(),
            project_path: "/Users/bharat/projects/rokr".to_string(),
            agent_tier: "build".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-test".to_string(),
        };
        let records = vec![
            header.clone(),
            SessionRecord::Turn {
                message: Message::user_text("turn0"),
                usage: usage(0),
                timestamp: "t0".to_string(),
            },
            SessionRecord::Turn {
                message: Message::assistant_text("turn1"),
                usage: usage(1),
                timestamp: "t1".to_string(),
            },
            SessionRecord::Compaction {
                summary: "summary of turns 0-1".to_string(),
                replaced_through: 1,
            },
            SessionRecord::Turn {
                message: Message::user_text("turn2"),
                usage: usage(2),
                timestamp: "t2".to_string(),
            },
            SessionRecord::Turn {
                message: Message::assistant_text("turn3"),
                usage: usage(3),
                timestamp: "t3".to_string(),
            },
            SessionRecord::Turn {
                message: Message::user_text("turn4"),
                usage: usage(4),
                timestamp: "t4".to_string(),
            },
            SessionRecord::Rollback { target: 3 },
        ];

        let contents = records
            .iter()
            .map(|record| serde_json::to_string(record).expect("serialize SessionRecord"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(session_dir.join("session.jsonl"), contents)
            .expect("failed to write session.jsonl fixture");

        let store = SessionStore::open(&dir);
        let (messages, meta, resume_state) = store
            .resume_session(&session_id)
            .expect("resume_session should succeed against the fixture log");

        let (expected_messages, expected_last_usage) = fold(&records);
        assert_eq!(messages, expected_messages);
        assert_eq!(resume_state.last_known_usage, expected_last_usage);

        match header {
            SessionRecord::Header {
                session_id,
                created_at,
                project_path,
                agent_tier,
                provider,
                model,
                ..
            } => {
                assert_eq!(meta.session_id, session_id);
                assert_eq!(meta.created_at, created_at);
                assert_eq!(meta.project_path, project_path);
                assert_eq!(meta.agent_tier, agent_tier);
                assert_eq!(meta.provider, provider);
                assert_eq!(meta.model, model);
            }
            _ => unreachable!("header is constructed as Header above"),
        }
    }

    /// Ticket 36 (session-index-list-jump): after creating two sessions
    /// (each with a Header and one or more Turns), `list_sessions` returns
    /// one entry per session whose fields match what was actually appended
    /// -- proving the index and the underlying logs never drift apart
    /// (PRD's "Index consistency" testing decision).
    #[tokio::test]
    async fn list_sessions_reads_index_metadata_matching_created_sessions() {
        let dir = unique_temp_dir("list-sessions");
        let store = SessionStore::open(&dir);

        let handle_a = store
            .create_session()
            .expect("create_session should succeed for session a");
        let session_id_a = handle_a.session_id().to_string();
        handle_a.append_header(
            1,
            session_id_a.clone(),
            "2026-07-20T00:00:00Z".to_string(),
            "/projects/alpha".to_string(),
            "build".to_string(),
            "anthropic".to_string(),
            "claude-test".to_string(),
        );
        handle_a.append_turn(
            Message::user_text("first prompt in session alpha"),
            usage(1),
            "2026-07-20T00:00:01Z".to_string(),
        );
        handle_a.append_turn(
            Message::assistant_text("reply in session alpha"),
            usage(2),
            "2026-07-20T00:00:02Z".to_string(),
        );
        handle_a.flush().await;

        let handle_b = store
            .create_session()
            .expect("create_session should succeed for session b");
        let session_id_b = handle_b.session_id().to_string();
        handle_b.append_header(
            1,
            session_id_b.clone(),
            "2026-07-20T01:00:00Z".to_string(),
            "/projects/beta".to_string(),
            "plan".to_string(),
            "openai".to_string(),
            "gpt-test".to_string(),
        );
        handle_b.append_turn(
            Message::user_text("first prompt in session beta"),
            usage(3),
            "2026-07-20T01:00:01Z".to_string(),
        );
        handle_b.flush().await;

        let entries = store
            .list_sessions()
            .expect("list_sessions should succeed against a populated index");
        assert_eq!(
            entries.len(),
            2,
            "expected one index entry per created session, got: {entries:?}"
        );

        let entry_a = entries
            .iter()
            .find(|entry| entry.session_id == session_id_a)
            .expect("session a's index entry should be present");
        assert_eq!(entry_a.project_path, "/projects/alpha");
        assert_eq!(entry_a.created_at, "2026-07-20T00:00:00Z");
        assert_eq!(entry_a.title, "first prompt in session alpha");
        assert_eq!(entry_a.turn_count, 2);
        assert_eq!(entry_a.last_model, "claude-test");

        let entry_b = entries
            .iter()
            .find(|entry| entry.session_id == session_id_b)
            .expect("session b's index entry should be present");
        assert_eq!(entry_b.project_path, "/projects/beta");
        assert_eq!(entry_b.created_at, "2026-07-20T01:00:00Z");
        assert_eq!(entry_b.title, "first prompt in session beta");
        assert_eq!(entry_b.turn_count, 1);
        assert_eq!(entry_b.last_model, "gpt-test");
    }

    /// Ticket 37 (session-search): `search` is a lazy, on-demand scan of
    /// each session's own `session.jsonl` body (PRD decision 2 -- no
    /// persisted secondary search index), not a lookup against the
    /// `sessions/index.jsonl` cache `list_sessions` reads. Three fixture
    /// sessions prove three cases: a term appearing inside a live `Turn`
    /// matches; a term appearing *only* inside a `Compaction` summary
    /// (never surfaced in `index.jsonl`) still matches -- the PRD calls
    /// this out as a deliberate decision, not an accident; and a session
    /// containing neither is excluded.
    #[test]
    fn search_returns_only_sessions_containing_substring_including_within_compaction_summary() {
        let dir = unique_temp_dir("search-sessions");

        let turn_match_id = "search-turn-match-session".to_string();
        write_session_fixture(
            &dir,
            &turn_match_id,
            &[
                SessionRecord::Header {
                    schema_version: 1,
                    session_id: turn_match_id.clone(),
                    created_at: "2026-07-20T00:00:00Z".to_string(),
                    project_path: "/projects/alpha".to_string(),
                    agent_tier: "build".to_string(),
                    provider: "anthropic".to_string(),
                    model: "claude-test".to_string(),
                },
                SessionRecord::Turn {
                    message: Message::user_text("please find zzyzxfindableterm in here"),
                    usage: usage(0),
                    timestamp: "t0".to_string(),
                },
            ],
        );

        let compaction_match_id = "search-compaction-match-session".to_string();
        write_session_fixture(
            &dir,
            &compaction_match_id,
            &[
                SessionRecord::Header {
                    schema_version: 1,
                    session_id: compaction_match_id.clone(),
                    created_at: "2026-07-20T01:00:00Z".to_string(),
                    project_path: "/projects/beta".to_string(),
                    agent_tier: "build".to_string(),
                    provider: "anthropic".to_string(),
                    model: "claude-test".to_string(),
                },
                SessionRecord::Turn {
                    message: Message::user_text("unrelated live turn content"),
                    usage: usage(0),
                    timestamp: "t0".to_string(),
                },
                SessionRecord::Compaction {
                    summary: "earlier discussion mentioned zzyzxfindableterm in passing"
                        .to_string(),
                    replaced_through: 0,
                },
            ],
        );

        let no_match_id = "search-no-match-session".to_string();
        write_session_fixture(
            &dir,
            &no_match_id,
            &[
                SessionRecord::Header {
                    schema_version: 1,
                    session_id: no_match_id.clone(),
                    created_at: "2026-07-20T02:00:00Z".to_string(),
                    project_path: "/projects/gamma".to_string(),
                    agent_tier: "build".to_string(),
                    provider: "anthropic".to_string(),
                    model: "claude-test".to_string(),
                },
                SessionRecord::Turn {
                    message: Message::user_text("completely unrelated content"),
                    usage: usage(0),
                    timestamp: "t0".to_string(),
                },
            ],
        );

        let store = SessionStore::open(&dir);
        let mut matches = store
            .search("zzyzxfindableterm")
            .expect("search should succeed against fixture sessions");
        matches.sort();

        let mut expected = vec![turn_match_id, compaction_match_id];
        expected.sort();
        assert_eq!(
            matches, expected,
            "expected search to return exactly the turn-match and compaction-match sessions"
        );
        assert!(
            !matches.contains(&no_match_id),
            "expected search to exclude the session with no matching content, got: {matches:?}"
        );
    }

    /// Ticket 38 (checkpoint-pre-images): `CheckpointStore::snapshot` writes
    /// a file's pre-image content under `sessions/<id>/snapshots/`, keyed by
    /// `(turn_index, path)` -- proves the stored bytes exactly match what
    /// was passed in, and that the keying actually distinguishes different
    /// `(turn_index, path)` pairs rather than colliding/overwriting: same
    /// path at two different turn indices, and two different paths at the
    /// same turn index, must each land in their own snapshot file.
    #[test]
    fn checkpoint_store_snapshot_writes_pre_image_keyed_by_turn_and_path() {
        let dir = unique_temp_dir("checkpoint-store");
        let store = CheckpointStore::open(&dir, "sess-checkpoint-1");

        let path_a = "/some/project/a.txt";
        let path_b = "/some/project/b.txt";

        let (snapshot_turn0_path_a, newly_written_turn0_path_a) = store
            .snapshot(0, path_a, Some("content-a-turn0"))
            .expect("snapshotting an existing file's content should succeed");
        let (snapshot_turn1_path_a, newly_written_turn1_path_a) = store
            .snapshot(1, path_a, Some("content-a-turn1"))
            .expect("snapshotting the same path at a later turn should succeed");
        let (snapshot_turn0_path_b, newly_written_turn0_path_b) = store
            .snapshot(0, path_b, Some("content-b-turn0"))
            .expect("snapshotting a different path at the same turn should succeed");

        assert!(
            newly_written_turn0_path_a && newly_written_turn1_path_a && newly_written_turn0_path_b,
            "each of these three distinct (turn_index, path) keys is captured for the first \
             time, so all three should report newly_written: true"
        );

        assert_ne!(
            snapshot_turn0_path_a, snapshot_turn1_path_a,
            "same path at two different turn indices must not collide"
        );
        assert_ne!(
            snapshot_turn0_path_a, snapshot_turn0_path_b,
            "two different paths at the same turn index must not collide"
        );

        let snapshots_dir = dir
            .join("sessions")
            .join("sess-checkpoint-1")
            .join("snapshots");
        assert_eq!(
            std::fs::read_to_string(snapshots_dir.join(&snapshot_turn0_path_a)).unwrap(),
            "content-a-turn0"
        );
        assert_eq!(
            std::fs::read_to_string(snapshots_dir.join(&snapshot_turn1_path_a)).unwrap(),
            "content-a-turn1"
        );
        assert_eq!(
            std::fs::read_to_string(snapshots_dir.join(&snapshot_turn0_path_b)).unwrap(),
            "content-b-turn0"
        );
    }

    /// Ticket 38 (checkpoint-pre-images), step 5: a brand-new file write has
    /// no pre-image at all -- distinct from a pre-existing but genuinely
    /// empty file. `snapshot`'s `old_content: Option<&str>` lets a caller
    /// pass `None` for the former and `Some("")` for the latter, and this
    /// asserts the on-disk result is actually distinguishable: an empty
    /// pre-existing file still produces a (zero-byte) content file, while an
    /// absent pre-image produces no content file at all, only a distinct
    /// marker.
    #[test]
    fn checkpoint_store_snapshot_distinguishes_absent_pre_image_from_empty_pre_existing_file() {
        let dir = unique_temp_dir("checkpoint-store-absent");
        let store = CheckpointStore::open(&dir, "sess-checkpoint-2");

        let (empty_snapshot, _) = store
            .snapshot(0, "/some/pre-existing-empty.txt", Some(""))
            .expect("snapshotting a genuinely empty pre-existing file should succeed");
        let (absent_snapshot, _) = store
            .snapshot(1, "/some/brand-new-file.txt", None)
            .expect("snapshotting a new-file write (no pre-image) should succeed");

        let snapshots_dir = dir
            .join("sessions")
            .join("sess-checkpoint-2")
            .join("snapshots");

        assert!(
            snapshots_dir.join(&empty_snapshot).exists(),
            "an empty pre-existing file must still produce a content file"
        );
        assert_eq!(
            std::fs::read_to_string(snapshots_dir.join(&empty_snapshot)).unwrap(),
            ""
        );
        assert!(
            !snapshots_dir.join(&absent_snapshot).exists(),
            "an absent pre-image must not produce a content file"
        );
        assert!(
            snapshots_dir
                .join(format!("{absent_snapshot}.absent"))
                .exists(),
            "an absent pre-image must produce a distinct on-disk marker"
        );
    }

    /// Ticket 38 scope-amendment (F-001, argus review): a turn's tool loop
    /// doing `write` then `edit` on the SAME path is a normal pattern -- the
    /// second call's `old` is the post-first-write content, which must NOT
    /// clobber the true turn-start pre-image already captured by the first
    /// call. Asserts first-write-wins: a second `snapshot` call for the
    /// SAME `(turn_index, path)` key returns the SAME snapshot_id, and the
    /// stored bytes still match the FIRST call's content, not the second.
    #[test]
    fn checkpoint_store_snapshot_first_write_wins_on_repeated_key_within_same_turn() {
        let dir = unique_temp_dir("checkpoint-store-first-write-wins");
        let store = CheckpointStore::open(&dir, "sess-checkpoint-3");
        let path = "/some/project/repeated.txt";

        let (first_snapshot, first_newly_written) = store
            .snapshot(0, path, Some("original"))
            .expect("first snapshot call should succeed");
        let (second_snapshot, second_newly_written) = store
            .snapshot(0, path, Some("intermediate"))
            .expect("second snapshot call for the same key should succeed (as a no-op write)");

        assert_eq!(
            first_snapshot, second_snapshot,
            "the same (turn_index, path) key must produce the same snapshot_id"
        );
        assert!(first_newly_written, "the first call for a new key must report newly_written: true");
        assert!(
            !second_newly_written,
            "the second call for an ALREADY-captured key must report newly_written: false, so \
             a caller knows not to append a duplicate Checkpoint record"
        );

        let snapshots_dir = dir
            .join("sessions")
            .join("sess-checkpoint-3")
            .join("snapshots");
        assert_eq!(
            std::fs::read_to_string(snapshots_dir.join(&first_snapshot)).unwrap(),
            "original",
            "the SECOND snapshot call for the same (turn_index, path) key must not overwrite \
             the FIRST call's true pre-turn pre-image"
        );
    }

    /// Ticket 38 scope-amendment (F-001, argus review): if the first
    /// mutation of a path within a turn is a brand-new-file write
    /// (`old_content: None`, producing a `.absent` marker) and a LATER
    /// mutation of the SAME path within the SAME turn passes `Some(content)`
    /// (e.g. a second `write` call after the file now exists), the second
    /// call must still be a no-op -- it must NOT also create a content file
    /// alongside the `.absent` marker, which would be an ambiguous,
    /// contradictory on-disk state (both "this file didn't exist" and
    /// "here is its content" for the same snapshot_id).
    #[test]
    fn checkpoint_store_snapshot_first_write_wins_does_not_create_coexisting_absent_and_content_files(
    ) {
        let dir = unique_temp_dir("checkpoint-store-first-write-wins-absent");
        let store = CheckpointStore::open(&dir, "sess-checkpoint-4");
        let path = "/some/project/new-then-mutated.txt";

        let (first_snapshot, first_newly_written) = store
            .snapshot(0, path, None)
            .expect("first snapshot call (brand-new file, no pre-image) should succeed");
        let (second_snapshot, second_newly_written) = store
            .snapshot(0, path, Some("later content"))
            .expect("second snapshot call for the same key should succeed (as a no-op write)");

        assert_eq!(first_snapshot, second_snapshot);
        assert!(first_newly_written);
        assert!(
            !second_newly_written,
            "a second call for an already-captured key must report newly_written: false"
        );

        let snapshots_dir = dir
            .join("sessions")
            .join("sess-checkpoint-4")
            .join("snapshots");
        assert!(
            snapshots_dir.join(format!("{first_snapshot}.absent")).exists(),
            "the original absent marker must still be present"
        );
        assert!(
            !snapshots_dir.join(&first_snapshot).exists(),
            "no content file must coexist with the absent marker for the same snapshot_id"
        );
    }

    /// Ticket 39 (rollback-command): `CheckpointStore::rollback_to(target)`
    /// restores every captured pre-image at turn indices >= `target`, in
    /// reverse-chronological order, so that for a path snapshotted at
    /// multiple qualifying turns the FINAL on-disk content is the EARLIEST
    /// one (closest to `target`) -- proves this against four real files on
    /// disk: one snapshotted at three turns spanning the target (must land
    /// on the closest-to-target content), one snapshotted only at a turn
    /// before the target (must be left untouched), one snapshotted at a
    /// turn at-or-after the target with an ABSENT pre-image (must be
    /// deleted), and one snapshotted only at a turn before the target as a
    /// second untouched control. Also asserts the returned touched-paths
    /// set is exactly the two paths actually restored.
    #[test]
    fn rollback_to_restores_pre_images_at_or_after_target_turn_in_reverse_order() {
        let dir = unique_temp_dir("rollback-to");
        let store = CheckpointStore::open(&dir, "sess-rollback-1");

        let path_a = dir.join("a.txt").to_string_lossy().into_owned();
        let path_b = dir.join("b.txt").to_string_lossy().into_owned();
        let path_c = dir.join("c.txt").to_string_lossy().into_owned();
        let path_d = dir.join("d.txt").to_string_lossy().into_owned();

        // path_a: mutated at turns 0, 2, and 4 -- target is 2, so only the
        // turn-2 and turn-4 pre-images qualify, and the turn-2 one (closest
        // to target) must be the one that survives on disk.
        store.snapshot(0, &path_a, Some("a-pre-turn0")).unwrap();
        store.snapshot(2, &path_a, Some("a-pre-turn2")).unwrap();
        store.snapshot(4, &path_a, Some("a-pre-turn4")).unwrap();
        std::fs::write(&path_a, "a-current-post-turn4").unwrap();

        // path_b: mutated only at turn 1, which is BEFORE target -- must be
        // left completely untouched by rollback_to(2).
        store.snapshot(1, &path_b, Some("b-pre-turn1")).unwrap();
        std::fs::write(&path_b, "b-current-post-turn1").unwrap();

        // path_c: a brand-new file created at turn 3 (>= target) -- its
        // pre-image is "absent", so rollback must DELETE it.
        store.snapshot(3, &path_c, None).unwrap();
        std::fs::write(&path_c, "c-current-post-turn3").unwrap();

        // path_d: mutated only at turn 0, BEFORE target -- second untouched
        // control, proving the "before target" exclusion isn't a fluke of
        // path_b alone.
        store.snapshot(0, &path_d, Some("d-pre-turn0")).unwrap();
        std::fs::write(&path_d, "d-current-post-turn0").unwrap();

        let mut touched = store.rollback_to(2).expect("rollback_to should succeed");
        touched.sort();

        assert_eq!(
            std::fs::read_to_string(&path_a).unwrap(),
            "a-pre-turn2",
            "expected path_a to land on its turn-2 pre-image (closest to target), not turn-4's"
        );
        assert_eq!(
            std::fs::read_to_string(&path_b).unwrap(),
            "b-current-post-turn1",
            "expected path_b (only touched before target) to be left untouched"
        );
        assert!(
            !std::path::Path::new(&path_c).exists(),
            "expected path_c (absent pre-image at or after target) to be deleted"
        );
        assert_eq!(
            std::fs::read_to_string(&path_d).unwrap(),
            "d-current-post-turn0",
            "expected path_d (only touched before target) to be left untouched"
        );

        let mut expected_touched = vec![path_a.clone(), path_c.clone()];
        expected_touched.sort();
        assert_eq!(
            touched, expected_touched,
            "expected rollback_to to report exactly the paths it actually restored"
        );
    }

    /// Ticket 40 (prompt-history): appending more than `MAX_ENTRIES` entries
    /// and reloading must return exactly `MAX_ENTRIES` entries, oldest-first,
    /// with the earliest excess entries trimmed off -- proves both the
    /// ordering and the bound `PromptHistory::load`/`append` are supposed to
    /// enforce together.
    #[test]
    fn prompt_history_append_then_load_returns_entries_in_order_capped_at_bound() {
        let dir = unique_temp_dir("prompt-history");

        let total = PromptHistory::MAX_ENTRIES + 3;
        for i in 0..total {
            PromptHistory::append(&dir, &format!("prompt-{i}")).expect("append should succeed");
        }

        let loaded = PromptHistory::load(&dir).expect("load should succeed");

        assert_eq!(
            loaded.len(),
            PromptHistory::MAX_ENTRIES,
            "expected load to cap at MAX_ENTRIES, got {} entries",
            loaded.len()
        );

        let expected: Vec<String> = (3..total).map(|i| format!("prompt-{i}")).collect();
        assert_eq!(
            loaded, expected,
            "expected the oldest 3 entries to have been trimmed, keeping only the most recent \
             MAX_ENTRIES in order"
        );
    }

    /// Ticket 40 (prompt-history): an entry containing a literal newline and a
    /// literal backslash must round-trip through the line-oriented history
    /// file exactly, proving `encode_entry`/`decode_entry` don't corrupt or
    /// truncate it.
    #[test]
    fn prompt_history_round_trips_entries_containing_newlines_and_backslashes() {
        let dir = unique_temp_dir("prompt-history-escaping");
        PromptHistory::append(&dir, "line one\nline two").unwrap();
        PromptHistory::append(&dir, "a backslash: \\ and a newline: \n end").unwrap();

        let loaded = PromptHistory::load(&dir).unwrap();
        assert_eq!(
            loaded,
            vec![
                "line one\nline two".to_string(),
                "a backslash: \\ and a newline: \n end".to_string(),
            ]
        );
    }

    /// Test-only helper: writes `records` (real `SessionRecord` values,
    /// serialized via `serde_json`, never hand-typed JSON) as
    /// `sessions/<session_id>/session.jsonl` under `dir`, mirroring how
    /// `resume_session_folds_log_into_messages_and_restores_last_known_usage`
    /// builds its fixture inline.
    fn write_session_fixture(dir: &std::path::Path, session_id: &str, records: &[SessionRecord]) {
        let session_dir = dir.join("sessions").join(session_id);
        std::fs::create_dir_all(&session_dir).unwrap();
        let contents = records
            .iter()
            .map(|record| serde_json::to_string(record).expect("serialize SessionRecord"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(session_dir.join("session.jsonl"), contents)
            .expect("failed to write session.jsonl fixture");
    }
}
