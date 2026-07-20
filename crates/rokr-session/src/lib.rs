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
        /// Schema v2 (architect ruling, phase-5): a `Turn` is one record per
        /// submit, carrying the user prompt PLUS every
        /// assistant/tool-use/tool-result/final message that submit produced
        /// -- not just the user prompt as schema v1 did. The `#[serde(alias =
        /// "message")]` + custom `deserialize_with` are a READ SHIM ONLY for
        /// v1 logs (Header `schema_version: 1`, a bare singular `message`
        /// object): serde accepts either the v2 `messages` array key or the
        /// v1 `message` object key, and `deserialize_messages_or_message`
        /// disambiguates the shape. `Serialize` stays derived and always
        /// writes the v2 `messages` array, so a v1 file is never rewritten --
        /// only read-shimmed on load.
        #[serde(alias = "message", deserialize_with = "deserialize_messages_or_message")]
        messages: Vec<Message>,
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

/// Read shim (architect ruling, phase-5, schema v2): deserializes a `Turn`
/// record's messages from EITHER the v2 shape (a `messages` array) or the v1
/// shape (a bare singular `message` object), so a v1 session log still
/// resumes correctly without ever being rewritten. A v1 single message is
/// wrapped into a one-element `Vec`.
fn deserialize_messages_or_message<'de, D>(deserializer: D) -> Result<Vec<Message>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum MessagesOrMessage {
        Many(Vec<Message>),
        One(Message),
    }
    match MessagesOrMessage::deserialize(deserializer)? {
        MessagesOrMessage::Many(messages) => Ok(messages),
        MessagesOrMessage::One(message) => Ok(vec![message]),
    }
}

/// Folds an ordered log of [`SessionRecord`]s into the messages a resumed
/// session should see, plus the most recently known [`rokr_core::Usage`].
///
/// See the module-level fold semantics: `Header`/`Checkpoint` are ignored;
/// `Turn` appends; `Compaction` collapses everything up to and including
/// `replaced_through` into one summary message; `Rollback` discards
/// everything above `target` from the *current* working output (it does not
/// rewind `next_turn_index`, so later genuinely-new turns still append).
pub fn fold(records: &[SessionRecord]) -> (Vec<Message>, Option<rokr_core::Usage>, usize) {
    let mut output: Vec<(usize, Message)> = Vec::new();
    let mut next_turn_index: usize = 0;
    let mut last_known_usage: Option<rokr_core::Usage> = None;
    // RULING 2 (F-00x fold contract fix): the compaction summary is tracked
    // OUT of band rather than positionally inserted into `output`. A
    // `Compaction` record OVERWRITES it (newest-wins -- `compact_transcript`
    // re-summarizes a transcript whose head already IS the prior summary, so
    // the newest summary text subsumes the old), and at the end it becomes
    // ONE leading message at the very FRONT of the output -- never trailing.
    // This replaces the old positional `insert_at` lookup, which silently
    // appended the summary at the END whenever `replaced_through` matched no
    // currently-buffered index.
    let mut pending_summary: Option<String> = None;

    for record in records {
        match record {
            SessionRecord::Header { .. } | SessionRecord::Checkpoint { .. } => {}
            SessionRecord::Turn {
                messages, usage, ..
            } => {
                // Schema v2 (architect ruling, phase-5): a `Turn` record can
                // now carry multiple messages (user prompt + assistant/tool
                // messages from the same submit). Flatten every message into
                // the output in order, tagging each with its OWNING turn's
                // index (all share `next_turn_index`) so Compaction/Rollback
                // filtering treats the whole exchange as one atomic turn.
                for message in messages {
                    output.push((next_turn_index, message.clone()));
                }
                last_known_usage = Some((*usage).into());
                next_turn_index += 1;
            }
            SessionRecord::Compaction {
                summary,
                replaced_through,
            } => {
                // Newest-wins: overwrite (never combine/concatenate) the
                // pending summary, and drop every buffered message at or
                // below `replaced_through` (same as before).
                pending_summary = Some(summary.clone());
                output.retain(|(turn_index, _)| *turn_index > *replaced_through);
            }
            SessionRecord::Rollback { target } => {
                output.retain(|(turn_index, _)| *turn_index <= *target);
            }
        }
    }

    // Flatten: the pending summary (if any) is ONE leading `user_text`
    // message at the FRONT, followed by every retained buffered message in
    // order.
    let mut messages: Vec<Message> = Vec::with_capacity(output.len() + 1);
    if let Some(summary) = pending_summary {
        messages.push(Message::user_text(format!(
            "[Earlier conversation summary — compacted to save context]\n\n{summary}"
        )));
    }
    messages.extend(output.into_iter().map(|(_, message)| message));
    (messages, last_known_usage, next_turn_index)
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
    /// The count of prior raw `Turn` records folded so far -- the index the
    /// NEXT genuinely-new `Turn` record will occupy once appended (matches
    /// `fold`'s own `next_turn_index` semantics: a `Rollback` does not
    /// rewind it).
    pub next_turn_index: usize,
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
    /// prompt, after that turn's whole exchange (user prompt plus every
    /// assistant/tool-use/tool-result/final message it produced) and usage
    /// are known (schema v2, architect ruling phase-5): exactly ONE `Turn`
    /// record per submit carrying the full `Vec<Message>`.
    pub fn append_turn(&self, messages: Vec<Message>, usage: UsageRecord, timestamp: String) {
        self.enqueue(SessionRecord::Turn {
            messages,
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

    /// Enqueues a `Compaction` record (RULING 2, architect ruling phase-5).
    /// Called from BOTH compaction call sites in `main.rs` -- the
    /// auto-compaction branch in `submit` and the manual `/compact` handler
    /// -- ONLY when `rokr_core::compact_transcript` actually
    /// `CompactionOutcome::Compacted(..)`, never on `NothingToCompact`.
    /// `summary` is the RAW summary text (the caller strips the
    /// `[Earlier conversation summary ...]` wrapper prefix back off before
    /// storing, because `fold` re-applies that exact wrapper on resume);
    /// `replaced_through` is the highest turn index the summary collapses
    /// through. Mirrors `append_rollback`'s append-only shape exactly.
    pub fn append_compaction(&self, summary: String, replaced_through: usize) {
        self.enqueue(SessionRecord::Compaction {
            summary,
            replaced_through,
        });
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

        let (messages, last_known_usage, next_turn_index) = fold(&records);

        Ok((
            messages,
            meta,
            ResumeState {
                last_known_usage,
                next_turn_index,
            },
        ))
    }

    /// RULING 3 (architect ruling, phase-5): the `replaced_through` of the
    /// LAST `Compaction` record in `session_id`'s log (in file order), or
    /// `None` if the session has never been compacted. `handle_rollback_command`
    /// consults this to REFUSE a `/rollback` whose target is at or before the
    /// last compaction boundary -- earlier turns were summarized away and
    /// cannot be un-folded. Reads the raw records directly (same
    /// forward-compatibility skip-unparseable-line policy as
    /// `resume_session`), never the denormalized index. `Ok(None)` if the log
    /// doesn't exist yet.
    pub fn last_compaction_replaced_through(
        &self,
        session_id: &str,
    ) -> std::io::Result<Option<usize>> {
        let session_jsonl_path = self
            .data_dir
            .join("sessions")
            .join(session_id)
            .join("session.jsonl");
        let contents = match std::fs::read_to_string(&session_jsonl_path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };

        let last = contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<SessionRecord>(line).ok())
            .filter_map(|record| match record {
                SessionRecord::Compaction {
                    replaced_through, ..
                } => Some(replaced_through),
                _ => None,
            })
            .next_back();

        Ok(last)
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
                    SessionRecord::Turn { messages, .. } => {
                        messages.iter().any(|message| message.text().contains(term))
                    }
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

    /// Ticket 39 (rollback-command), PRD decision 4; RULING 3 boundary
    /// correction (architect ruling, phase-5): restores every captured
    /// pre-image at turn indices STRICTLY GREATER THAN `target_turn`
    /// (previously `>=`), in reverse-chronological order (highest turn_index
    /// first), so that for a path snapshotted at multiple qualifying turns the
    /// FINAL restored content on disk is the EARLIEST qualifying one (the
    /// pre-image of the first turn AFTER `target_turn`) -- which is exactly
    /// the state the path was in at the END of `target_turn`. Semantics:
    /// "rollback to N = world as of END of turn N", so turn N's OWN file
    /// mutation is LEFT IN PLACE; only turns strictly after N are undone. A
    /// snapshot captured with no pre-image (a `.absent` marker -- a
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
            .filter(|(turn_index, _)| *turn_index > target_turn)
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
    ///
    /// F-006 (argus review): appends a short deterministic hash of the
    /// UNSANITIZED `path` so two different paths that sanitize to the same
    /// string (e.g. `foo-bar.txt` vs `foo_bar.txt`) still get distinct ids.
    /// `parse_turn_index` remains unaffected: it only reads the digits
    /// before the FIRST `-`, which is still exactly the boundary right
    /// after `turn_index` (the sanitized path segment never contains a
    /// literal `-`, since sanitization maps it to `_`) -- the new trailing
    /// `-{hash_suffix}` lands inside the discarded remainder.
    fn snapshot_id(turn_index: usize, path: &str) -> String {
        use std::hash::{Hash, Hasher};
        let sanitized_path: String = path
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut hasher);
        let hash_suffix = format!("{:08x}", hasher.finish() as u32);
        format!("t{turn_index}-{sanitized_path}-{hash_suffix}")
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
const TITLE_MAX_CHARS: usize = 80;

fn truncate_title(text: &str) -> String {
    if text.chars().count() > TITLE_MAX_CHARS {
        let truncated: String = text.chars().take(TITLE_MAX_CHARS).collect();
        format!("{truncated}...")
    } else {
        text.to_string()
    }
}

/// Derives a session index title (the one-line `/sessions` listing's
/// "title" column) from a message: the FIRST LINE of its first `Text`
/// content block, with internal whitespace collapsed to single spaces,
/// trimmed, and truncated to `TITLE_MAX_CHARS` with an ellipsis if it
/// overflows (architect ruling, phase-5-session-management: supersedes an
/// earlier "just replace \n with a space over the whole message" draft).
/// Only the FIRST `Text` block is considered (other block kinds --
/// `ToolUse`/`ToolResult` -- contribute nothing to a title and are never
/// reached here since a `Turn` record's message is always constructed via
/// `Message::user_text`, a single `Text` block); only its FIRST LINE is
/// considered, which naturally clips e.g. an @-mention-expanded file's
/// contents down to something reasonable for a one-line listing, without
/// separate handling for that case.
fn derive_title(message: &Message) -> String {
    let first_text_block = message.content.iter().find_map(|block| match block {
        rokr_core::ContentBlock::Text { text, .. } => Some(text.as_str()),
        rokr_core::ContentBlock::ToolUse { .. } | rokr_core::ContentBlock::ToolResult { .. } => {
            None
        }
    });
    let first_line = first_text_block.and_then(|text| text.lines().next()).unwrap_or("");
    let collapsed = first_line.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_title(&collapsed)
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
                    messages,
                    timestamp,
                    ..
                } => {
                    state.turn_count += 1;
                    if state.title.is_none() {
                        // Title is the first user prompt: schema v2's first
                        // message in a Turn is that prompt (submit pushes the
                        // user message first, then the exchange).
                        if let Some(first) = messages.first() {
                            state.title = Some(derive_title(first));
                        }
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
                // F-004 (argus review): every failure branch below used to
                // be silently discarded (`let _ = ...`/`if let Ok(...) =
                // ...` with no `else`) -- a full disk, a permissions
                // problem, or a serialization bug would vanish without a
                // trace. Each now logs a warning naming the session id, so
                // a real failure is at least visible, while still not
                // panicking/propagating (this task is scoped to visibility,
                // not retry/backoff semantics).
                match serde_json::to_string(&record) {
                    Ok(mut line) => {
                        line.push('\n');
                        if let Err(err) = file.write_all(line.as_bytes()).await {
                            eprintln!(
                                "warning: session {}: failed to write session.jsonl record: {err}",
                                index_state.session_id
                            );
                        }
                    }
                    Err(err) => {
                        eprintln!(
                            "warning: session {}: failed to serialize session.jsonl record: {err}",
                            index_state.session_id
                        );
                    }
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
                        messages,
                        timestamp,
                        ..
                    } => {
                        index_state.turn_count += 1;
                        if index_state.title.is_none() {
                            if let Some(first) = messages.first() {
                                index_state.title = Some(derive_title(first));
                            }
                        }
                        index_state.updated_at = timestamp.clone();
                        true
                    }
                    _ => false,
                };

                if index_changed {
                    match serde_json::to_string(&index_state.to_entry()) {
                        Ok(mut line) => {
                            line.push('\n');
                            if let Err(err) = index_file.write_all(line.as_bytes()).await {
                                eprintln!(
                                    "warning: session {}: failed to write index.jsonl entry: {err}",
                                    index_state.session_id
                                );
                            }
                        }
                        Err(err) => {
                            eprintln!(
                                "warning: session {}: failed to serialize index.jsonl entry: {err}",
                                index_state.session_id
                            );
                        }
                    }
                }
            }
            WriterCommand::Flush(ack) => {
                if let Err(err) = file.flush().await {
                    eprintln!(
                        "warning: session {}: failed to flush session.jsonl: {err}",
                        index_state.session_id
                    );
                }
                if let Err(err) = index_file.flush().await {
                    eprintln!(
                        "warning: session {}: failed to flush index.jsonl: {err}",
                        index_state.session_id
                    );
                }
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
            messages: vec![Message::user_text("hello world")],
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
                messages: vec![Message::user_text("first")],
                usage: usage(1),
                timestamp: "t0".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![Message::assistant_text("second")],
                usage: usage(2),
                timestamp: "t1".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![Message::user_text("third")],
                usage: usage(3),
                timestamp: "t2".to_string(),
            },
        ];

        let (messages, last_usage, _next_turn_index) = fold(&records);

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
                messages: vec![Message::user_text("turn0")],
                usage: usage(0),
                timestamp: "t0".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![Message::assistant_text("turn1")],
                usage: usage(1),
                timestamp: "t1".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![Message::user_text("turn2")],
                usage: usage(2),
                timestamp: "t2".to_string(),
            },
            SessionRecord::Compaction {
                summary: "summary of turns 0-2".to_string(),
                replaced_through: 2,
            },
            SessionRecord::Turn {
                messages: vec![Message::assistant_text("turn3")],
                usage: usage(3),
                timestamp: "t3".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![Message::user_text("turn4")],
                usage: usage(4),
                timestamp: "t4".to_string(),
            },
        ];

        let (messages, last_usage, _next_turn_index) = fold(&records);

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
                messages: vec![Message::user_text("turn0")],
                usage: usage(0),
                timestamp: "t0".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![Message::assistant_text("turn1")],
                usage: usage(1),
                timestamp: "t1".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![Message::user_text("turn2")],
                usage: usage(2),
                timestamp: "t2".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![Message::assistant_text("turn3")],
                usage: usage(3),
                timestamp: "t3".to_string(),
            },
            SessionRecord::Rollback { target: 1 },
        ];

        let (messages, last_usage, _next_turn_index) = fold(&records);

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
                messages: vec![Message::user_text("turn0")],
                usage: usage(0),
                timestamp: "t0".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![Message::assistant_text("turn1")],
                usage: usage(1),
                timestamp: "t1".to_string(),
            },
            SessionRecord::Compaction {
                summary: "summary of turns 0-1".to_string(),
                replaced_through: 1,
            },
            SessionRecord::Turn {
                messages: vec![Message::user_text("turn2")],
                usage: usage(2),
                timestamp: "t2".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![Message::assistant_text("turn3")],
                usage: usage(3),
                timestamp: "t3".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![Message::user_text("turn4")],
                usage: usage(4),
                timestamp: "t4".to_string(),
            },
            SessionRecord::Rollback { target: 3 },
        ];

        let (messages, last_usage, _next_turn_index) = fold(&records);

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

    /// RULING 1 (schema v2): a single `Turn` record now carries a whole
    /// exchange (`Vec<Message>` -- user prompt PLUS assistant/tool messages).
    /// `fold` must flatten ALL of a Turn's messages into the output in order,
    /// not just the first. Proves the user prompt and its assistant reply
    /// from the same submit both survive the fold, in order.
    #[test]
    fn fold_flattens_all_messages_of_a_multi_message_turn_in_order() {
        let records = vec![
            SessionRecord::Turn {
                messages: vec![
                    Message::user_text("user asks"),
                    Message::assistant_text("assistant replies"),
                ],
                usage: usage(1),
                timestamp: "t0".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![
                    Message::user_text("user asks again"),
                    Message::assistant_text("assistant replies again"),
                ],
                usage: usage(2),
                timestamp: "t1".to_string(),
            },
        ];

        let (messages, _last_usage, next_turn_index) = fold(&records);

        assert_eq!(
            messages,
            vec![
                Message::user_text("user asks"),
                Message::assistant_text("assistant replies"),
                Message::user_text("user asks again"),
                Message::assistant_text("assistant replies again"),
            ],
            "expected fold to flatten every message of each Turn in order"
        );
        // Two Turn records -> next_turn_index is 2 (one per SUBMIT, not one
        // per message), proving the whole exchange is tagged as one turn.
        assert_eq!(next_turn_index, 2);
    }

    /// RULING 1 (schema v2): every message of a Turn shares that Turn's OWN
    /// raw index (not one incrementing index per message), so a `Rollback`
    /// treats the whole exchange atomically. A naive "one index per message"
    /// implementation would give the assistant reply of turn 0 index 1 and
    /// `Rollback { target: 0 }` would wrongly drop it -- this asserts it
    /// survives.
    #[test]
    fn fold_tags_every_message_of_a_turn_with_that_turns_index_so_rollback_is_atomic() {
        let records = vec![
            SessionRecord::Turn {
                messages: vec![
                    Message::user_text("turn0 user"),
                    Message::assistant_text("turn0 assistant"),
                ],
                usage: usage(0),
                timestamp: "t0".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![
                    Message::user_text("turn1 user"),
                    Message::assistant_text("turn1 assistant"),
                ],
                usage: usage(1),
                timestamp: "t1".to_string(),
            },
            SessionRecord::Rollback { target: 0 },
        ];

        let (messages, _last_usage, _next_turn_index) = fold(&records);

        assert_eq!(
            messages,
            vec![
                Message::user_text("turn0 user"),
                Message::assistant_text("turn0 assistant"),
            ],
            "expected rollback to target 0 to keep BOTH of turn 0's messages (atomic turn) and \
             drop all of turn 1's"
        );
    }

    /// RULING 3: `last_compaction_replaced_through` returns the
    /// `replaced_through` of the LAST `Compaction` record in file order (or
    /// `None` for a session that was never compacted).
    #[test]
    fn last_compaction_replaced_through_returns_the_last_compaction_or_none() {
        let dir = unique_temp_dir("last-compaction");

        // Never-compacted session -> None.
        let plain_id = "plain-session".to_string();
        write_session_fixture(
            &dir,
            &plain_id,
            &[SessionRecord::Turn {
                messages: vec![Message::user_text("hi")],
                usage: usage(0),
                timestamp: "t0".to_string(),
            }],
        );

        // Two compactions -> the LAST one's replaced_through wins.
        let compacted_id = "compacted-session".to_string();
        write_session_fixture(
            &dir,
            &compacted_id,
            &[
                SessionRecord::Turn {
                    messages: vec![Message::user_text("turn0")],
                    usage: usage(0),
                    timestamp: "t0".to_string(),
                },
                SessionRecord::Compaction {
                    summary: "first".to_string(),
                    replaced_through: 0,
                },
                SessionRecord::Turn {
                    messages: vec![Message::user_text("turn1")],
                    usage: usage(1),
                    timestamp: "t1".to_string(),
                },
                SessionRecord::Compaction {
                    summary: "second".to_string(),
                    replaced_through: 2,
                },
            ],
        );

        let store = SessionStore::open(&dir);
        assert_eq!(store.last_compaction_replaced_through(&plain_id).unwrap(), None);
        assert_eq!(
            store.last_compaction_replaced_through(&compacted_id).unwrap(),
            Some(2),
            "expected the LAST compaction's replaced_through (2), not the first (0)"
        );
        assert_eq!(
            store.last_compaction_replaced_through("nonexistent").unwrap(),
            None,
            "a session with no log yet must return None, not an error"
        );
    }

    /// RULING 2 done-when #3: folding a `Turn... Compaction Turn...`
    /// sequence yields `[summary, retained-tail-turns-in-order..., later
    /// turns...]` with the summary at the FRONT. Turn 2 survives the
    /// compaction (its index > `replaced_through`) and stays ahead of the
    /// later turn 3.
    #[test]
    fn fold_places_compaction_summary_at_front_followed_by_retained_and_later_turns() {
        let records = vec![
            SessionRecord::Turn {
                messages: vec![Message::user_text("turn0")],
                usage: usage(0),
                timestamp: "t0".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![Message::assistant_text("turn1")],
                usage: usage(1),
                timestamp: "t1".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![Message::user_text("turn2")],
                usage: usage(2),
                timestamp: "t2".to_string(),
            },
            SessionRecord::Compaction {
                summary: "summary through turn1".to_string(),
                replaced_through: 1,
            },
            SessionRecord::Turn {
                messages: vec![Message::user_text("turn3")],
                usage: usage(3),
                timestamp: "t3".to_string(),
            },
        ];

        let (messages, _last_usage, _next) = fold(&records);

        assert_eq!(
            messages,
            vec![
                Message::user_text(
                    "[Earlier conversation summary — compacted to save context]\n\nsummary through turn1"
                ),
                Message::user_text("turn2"),
                Message::user_text("turn3"),
            ],
            "expected [summary, retained turn2, later turn3] with the summary at the FRONT"
        );
    }

    /// RULING 2 done-when #4 (regression): the OLD fold computed the summary's
    /// insert position by finding the first buffered `(idx, _)` with `idx <=
    /// replaced_through`, falling back to the END if none matched -- so a
    /// `Compaction` whose `replaced_through` is below EVERY currently-buffered
    /// index would silently append the summary at the END of the transcript.
    /// This fixture makes that edge fire: a SECOND compaction whose
    /// `replaced_through` (1) is below the only surviving buffered turn (index
    /// 3, retained by the first compaction). The fix must put the summary at
    /// the FRONT, and (newest-wins) it must be the SECOND summary's text, not
    /// the first's.
    #[test]
    fn fold_keeps_summary_leading_not_trailing_when_replaced_through_matches_no_buffered_index() {
        let records = vec![
            SessionRecord::Turn {
                messages: vec![Message::user_text("turn0")],
                usage: usage(0),
                timestamp: "t0".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![Message::user_text("turn1")],
                usage: usage(1),
                timestamp: "t1".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![Message::user_text("turn2")],
                usage: usage(2),
                timestamp: "t2".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![Message::user_text("turn3")],
                usage: usage(3),
                timestamp: "t3".to_string(),
            },
            // First compaction retains only turn 3 (index > 2).
            SessionRecord::Compaction {
                summary: "SUMMARY_ALPHA".to_string(),
                replaced_through: 2,
            },
            // Second compaction: replaced_through 1 is BELOW the only
            // surviving buffered index (3) -- the OLD code's `position()`
            // lookup finds no match and appends at the END.
            SessionRecord::Compaction {
                summary: "SUMMARY_BETA".to_string(),
                replaced_through: 1,
            },
        ];

        let (messages, _last_usage, _next) = fold(&records);

        assert_eq!(
            messages,
            vec![
                Message::user_text(
                    "[Earlier conversation summary — compacted to save context]\n\nSUMMARY_BETA"
                ),
                Message::user_text("turn3"),
            ],
            "expected the (newest) summary at the FRONT followed by the retained turn3, not the \
             old buggy trailing-append"
        );
        // Explicit leading-not-trailing guard: the last message must be the
        // retained turn, never the summary.
        assert_eq!(messages.last().unwrap().text(), "turn3");
    }

    /// RULING 1 done-when #3 (v1 compat, READ SHIM ONLY): a v1 log -- Header
    /// `schema_version: 1` and a Turn written in the OLD singular shape
    /// (`{"type":"Turn","message":{...},...}`, a bare object under the
    /// `message` key, not a `messages` array) -- must still fold correctly
    /// with its message intact, AND the on-disk file must be byte-identical
    /// before and after resume (resume only reads; it never rewrites a v1
    /// file). Deliberately hand-builds the v1 JSON line (the `message` key no
    /// longer exists on the current `SessionRecord::Turn`, which serializes
    /// `messages`) to exercise the real read shim.
    #[test]
    fn v1_log_with_singular_message_turn_resumes_intact_and_is_never_rewritten() {
        let dir = unique_temp_dir("v1-compat-resume");
        let session_id = "01V1COMPATSESSION".to_string();
        let session_dir = dir.join("sessions").join(&session_id);
        std::fs::create_dir_all(&session_dir).unwrap();

        // A real Header (schema_version 1) plus a hand-built v1-shape Turn
        // line carrying a singular `message` object (the pre-v2 wire shape).
        let header = SessionRecord::Header {
            schema_version: 1,
            session_id: session_id.clone(),
            created_at: "2026-07-20T00:00:00Z".to_string(),
            project_path: "/projects/v1".to_string(),
            agent_tier: "build".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-test".to_string(),
        };
        let v1_message_json =
            serde_json::to_string(&Message::user_text("v1 prompt text")).unwrap();
        let v1_usage_json = serde_json::to_string(&usage(7)).unwrap();
        let v1_turn_line = format!(
            "{{\"type\":\"Turn\",\"message\":{v1_message_json},\"usage\":{v1_usage_json},\"timestamp\":\"t0\"}}"
        );
        let contents = format!(
            "{}\n{}\n",
            serde_json::to_string(&header).unwrap(),
            v1_turn_line
        );
        let session_jsonl_path = session_dir.join("session.jsonl");
        std::fs::write(&session_jsonl_path, &contents).unwrap();

        let bytes_before = std::fs::read(&session_jsonl_path).unwrap();

        let store = SessionStore::open(&dir);
        let (messages, meta, resume_state) = store
            .resume_session(&session_id)
            .expect("resuming a v1 log via the read shim should succeed");

        assert_eq!(
            messages,
            vec![Message::user_text("v1 prompt text")],
            "the v1 singular-message Turn must fold into exactly its one message"
        );
        assert_eq!(meta.session_id, session_id);
        assert_eq!(resume_state.next_turn_index, 1);
        assert_eq!(resume_state.last_known_usage, Some(rokr_core::Usage::from(usage(7))));

        let bytes_after = std::fs::read(&session_jsonl_path).unwrap();
        assert_eq!(
            bytes_before, bytes_after,
            "resuming a v1 log must never rewrite it -- the file must be byte-identical"
        );
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
            vec![Message::user_text("hello session store")],
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
            SessionRecord::Turn { messages, .. } => {
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].text(), "hello session store");
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
                messages: vec![Message::user_text("turn0")],
                usage: usage(0),
                timestamp: "t0".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![Message::assistant_text("turn1")],
                usage: usage(1),
                timestamp: "t1".to_string(),
            },
            SessionRecord::Compaction {
                summary: "summary of turns 0-1".to_string(),
                replaced_through: 1,
            },
            SessionRecord::Turn {
                messages: vec![Message::user_text("turn2")],
                usage: usage(2),
                timestamp: "t2".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![Message::assistant_text("turn3")],
                usage: usage(3),
                timestamp: "t3".to_string(),
            },
            SessionRecord::Turn {
                messages: vec![Message::user_text("turn4")],
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

        let (expected_messages, expected_last_usage, expected_next_turn_index) = fold(&records);
        assert_eq!(messages, expected_messages);
        assert_eq!(resume_state.last_known_usage, expected_last_usage);
        assert_eq!(resume_state.next_turn_index, expected_next_turn_index);

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
            vec![Message::user_text("first prompt in session alpha")],
            usage(1),
            "2026-07-20T00:00:01Z".to_string(),
        );
        handle_a.append_turn(
            vec![Message::assistant_text("reply in session alpha")],
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
            vec![Message::user_text("first prompt in session beta")],
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
                    messages: vec![Message::user_text("please find zzyzxfindableterm in here")],
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
                    messages: vec![Message::user_text("unrelated live turn content")],
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
                    messages: vec![Message::user_text("completely unrelated content")],
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

    /// RULING 3 (architect ruling, phase-5): "rollback to N = world as of
    /// END of turn N". `CheckpointStore::rollback_to(target)` restores every
    /// captured pre-image at turn indices STRICTLY GREATER THAN `target`
    /// (previously `>=`), in reverse-chronological order, so a turn's OWN file
    /// mutation survives a rollback TO that turn; only turns strictly after it
    /// are undone. Proves this against five real files on disk:
    /// - path_a: snapshotted at turns 0, 2, and 4 -- target is 2, so only the
    ///   turn-4 pre-image qualifies (turn 2's own mutation is LEFT ALONE), and
    ///   turn 4's pre-image IS turn 2's post-write state (world as of end of
    ///   turn 2).
    /// - path_e: snapshotted ONLY at the target turn 2 -- under the NEW `>`
    ///   boundary its own mutation must SURVIVE untouched (under the old `>=`
    ///   boundary it would have been wrongly reverted); this is the direct
    ///   boundary proof.
    /// - path_b / path_d: snapshotted only before the target -- untouched.
    /// - path_c: a brand-new file created strictly after the target (turn 3,
    ///   ABSENT pre-image) -- must be DELETED.
    ///
    /// Also asserts the returned touched-paths set is exactly the paths
    /// actually restored/deleted (path_a and path_c), NOT path_e.
    #[test]
    fn rollback_to_restores_pre_images_strictly_after_target_leaving_target_turn_mutations_intact() {
        let dir = unique_temp_dir("rollback-to");
        let store = CheckpointStore::open(&dir, "sess-rollback-1");

        let path_a = dir.join("a.txt").to_string_lossy().into_owned();
        let path_b = dir.join("b.txt").to_string_lossy().into_owned();
        let path_c = dir.join("c.txt").to_string_lossy().into_owned();
        let path_d = dir.join("d.txt").to_string_lossy().into_owned();
        let path_e = dir.join("e.txt").to_string_lossy().into_owned();

        // path_a: mutated at turns 0, 2, and 4 -- target is 2, so under the
        // NEW `> target` boundary only the turn-4 pre-image qualifies. Turn
        // 2's own pre-image is NOT restored (its mutation stays), and turn 4's
        // pre-image ("a-pre-turn4") is exactly turn 2's post-write state --
        // the world as of the END of turn 2.
        store.snapshot(0, &path_a, Some("a-pre-turn0")).unwrap();
        store.snapshot(2, &path_a, Some("a-pre-turn2")).unwrap();
        store.snapshot(4, &path_a, Some("a-pre-turn4")).unwrap();
        std::fs::write(&path_a, "a-current-post-turn4").unwrap();

        // path_b: mutated only at turn 1, which is BEFORE target -- must be
        // left completely untouched by rollback_to(2).
        store.snapshot(1, &path_b, Some("b-pre-turn1")).unwrap();
        std::fs::write(&path_b, "b-current-post-turn1").unwrap();

        // path_c: a brand-new file created at turn 3 (> target) -- its
        // pre-image is "absent", so rollback must DELETE it.
        store.snapshot(3, &path_c, None).unwrap();
        std::fs::write(&path_c, "c-current-post-turn3").unwrap();

        // path_d: mutated only at turn 0, BEFORE target -- second untouched
        // control, proving the "before target" exclusion isn't a fluke of
        // path_b alone.
        store.snapshot(0, &path_d, Some("d-pre-turn0")).unwrap();
        std::fs::write(&path_d, "d-current-post-turn0").unwrap();

        // path_e: mutated ONLY at the target turn 2, never after -- the direct
        // boundary proof. Under the NEW `> target` semantics its own mutation
        // must SURVIVE (its turn-2 pre-image must NOT be restored).
        store.snapshot(2, &path_e, Some("e-pre-turn2")).unwrap();
        std::fs::write(&path_e, "e-current-post-turn2").unwrap();

        let mut touched = store.rollback_to(2).expect("rollback_to should succeed");
        touched.sort();

        assert_eq!(
            std::fs::read_to_string(&path_a).unwrap(),
            "a-pre-turn4",
            "expected path_a to land on turn-4's pre-image (world as of END of turn 2); turn 2's \
             own mutation must be left alone, so turn-2's pre-image is NOT restored"
        );
        assert_eq!(
            std::fs::read_to_string(&path_b).unwrap(),
            "b-current-post-turn1",
            "expected path_b (only touched before target) to be left untouched"
        );
        assert!(
            !std::path::Path::new(&path_c).exists(),
            "expected path_c (absent pre-image strictly after target) to be deleted"
        );
        assert_eq!(
            std::fs::read_to_string(&path_d).unwrap(),
            "d-current-post-turn0",
            "expected path_d (only touched before target) to be left untouched"
        );
        assert_eq!(
            std::fs::read_to_string(&path_e).unwrap(),
            "e-current-post-turn2",
            "expected path_e (mutated AT the target turn 2) to KEEP its own mutation under the \
             new `> target` boundary -- turn 2's own write survives a rollback to turn 2"
        );

        let mut expected_touched = vec![path_a.clone(), path_c.clone()];
        expected_touched.sort();
        assert_eq!(
            touched, expected_touched,
            "expected rollback_to to report exactly the paths strictly after the target it \
             actually restored/deleted (path_a, path_c) -- NOT path_e (target turn itself)"
        );
    }

    /// F-006 (argus review): two different paths that sanitize to the SAME
    /// string (e.g. `foo-bar.txt` vs `foo_bar.txt` -- both non-alphanumeric
    /// separator chars collapse to `_`) must not collide on the same
    /// `snapshot_id`. Pre-fix, `snapshot`'s first-write-wins `exists()`
    /// check would treat the second path's snapshot as "already captured"
    /// and silently drop its pre-image content.
    #[test]
    fn checkpoint_store_snapshot_id_distinguishes_paths_that_sanitize_to_the_same_string() {
        let dir = unique_temp_dir("checkpoint-collision");
        let store = CheckpointStore::open(&dir, "sess-collision-1");

        let path_dash = dir.join("foo-bar.txt").to_string_lossy().into_owned();
        let path_underscore = dir.join("foo_bar.txt").to_string_lossy().into_owned();

        // Captured at turn 1 so a rollback to target 0 (RULING 3's `> target`
        // boundary) actually restores them -- the F-006 no-collision proof is
        // orthogonal to which turn index is used.
        let (id_dash, newly_dash) = store.snapshot(1, &path_dash, Some("dash-content")).unwrap();
        let (id_underscore, newly_underscore) =
            store.snapshot(1, &path_underscore, Some("underscore-content")).unwrap();

        assert!(newly_dash && newly_underscore);
        assert_ne!(
            id_dash, id_underscore,
            "two paths that sanitize to the same string must not share a snapshot_id"
        );

        let snapshots_dir = dir.join("sessions").join("sess-collision-1").join("snapshots");
        assert_eq!(std::fs::read_to_string(snapshots_dir.join(&id_dash)).unwrap(), "dash-content");
        assert_eq!(
            std::fs::read_to_string(snapshots_dir.join(&id_underscore)).unwrap(),
            "underscore-content"
        );

        std::fs::write(&path_dash, "dash-current").unwrap();
        std::fs::write(&path_underscore, "underscore-current").unwrap();

        let mut touched = store.rollback_to(0).unwrap();
        touched.sort();
        let mut expected_touched = vec![path_dash.clone(), path_underscore.clone()];
        expected_touched.sort();
        assert_eq!(touched, expected_touched);

        assert_eq!(std::fs::read_to_string(&path_dash).unwrap(), "dash-content");
        assert_eq!(std::fs::read_to_string(&path_underscore).unwrap(), "underscore-content");
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

    /// F-004 (argus review): `run_writer` must not panic when a real
    /// `write_all` fails (previously every failure branch was silently
    /// discarded via `let _ = ...`/`if let Ok(...) = ...` with no
    /// diagnostic at all) -- it should log a warning and keep processing
    /// subsequent commands normally. Forces a genuine write failure by
    /// handing `run_writer` a `File` opened `read(true)` only (no write
    /// access) against a path pre-created and chmod'd `0o444`, so the
    /// internal `file.write_all` call genuinely errors at the OS level
    /// rather than being simulated.
    ///
    /// This test deliberately does NOT assert on the literal `eprintln!`
    /// warning text: capturing actual stderr output in an automated Rust
    /// test would require process-wide fd redirection, which is fragile in
    /// a parallel test binary -- other concurrently-running tests' own
    /// `eprintln!` calls would also land in the redirected stream and
    /// either corrupt this assertion or this test would corrupt theirs.
    /// Instead this proves the *behavioral* contract (no panic, keeps
    /// processing) which is what actually matters for correctness.
    #[tokio::test]
    async fn run_writer_survives_a_write_failure_without_panicking_and_keeps_processing_commands()
    {
        use std::os::unix::fs::PermissionsExt;

        let dir = unique_temp_dir("run-writer-write-failure");

        let session_file_path = dir.join("readonly-session.jsonl");
        std::fs::write(&session_file_path, "").expect("failed to pre-create session.jsonl fixture");
        let mut perms = std::fs::metadata(&session_file_path).unwrap().permissions();
        perms.set_mode(0o444);
        std::fs::set_permissions(&session_file_path, perms).unwrap();

        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(&session_file_path)
            .expect("should be able to open the read-only fixture for reading");

        let index_file_path = dir.join("index.jsonl");
        let index_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&index_file_path)
            .expect("index.jsonl fixture should be creatable");

        let index_state = IndexState {
            session_id: "sess-write-failure".to_string(),
            project_path: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            title: None,
            turn_count: 0,
            last_model: String::new(),
        };

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let writer_task = tokio::spawn(run_writer(file, index_file, index_state, rx));

        // This Append's `file.write_all` will genuinely fail (the file
        // handle has no write access) -- pre-fix this was silently
        // swallowed with no diagnostic; post-fix it additionally logs a
        // warning, but either way the writer task must not panic.
        tx.send(WriterCommand::Append(SessionRecord::Turn {
            messages: vec![Message::user_text("this write is doomed to fail")],
            usage: UsageRecord {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            timestamp: "2026-07-20T00:00:00Z".to_string(),
        }))
        .expect("channel send should succeed");

        // Proves the writer task kept processing commands after the
        // failed write, rather than panicking and dropping the receiver:
        // a `Flush` sent right after still gets acked.
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        tx.send(WriterCommand::Flush(ack_tx))
            .expect("channel send should succeed");
        ack_rx
            .await
            .expect("writer task should still be alive and ack the Flush after the failed write");

        drop(tx);
        writer_task
            .await
            .expect("writer task should exit cleanly (not panic) once the channel is dropped");

        // Restore write permission so the temp dir cleans up without issue.
        let mut restored_perms = std::fs::metadata(&session_file_path).unwrap().permissions();
        restored_perms.set_mode(0o644);
        std::fs::set_permissions(&session_file_path, restored_perms).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Architect ruling (phase-5-session-management): supersedes the earlier
    /// "replace \n with a space over the whole message" mechanical item.
    /// `derive_title` must take only the FIRST LINE of the first `Text`
    /// block, dropping any subsequent lines entirely (not joining them).
    #[test]
    fn derive_title_takes_only_first_line_of_first_text_block_collapsing_whitespace() {
        let message = Message::user_text(
            "line one has  extra   spaces\nline two should be dropped entirely",
        );
        assert_eq!(derive_title(&message), "line one has extra spaces");
    }

    /// Internal whitespace (tabs, stray `\r`, repeated spaces) within that
    /// first line collapses to single spaces, and leading/trailing
    /// whitespace is trimmed.
    #[test]
    fn derive_title_trims_and_collapses_carriage_returns_and_tabs() {
        let message = Message::user_text("  \tfirst line with\ttabs and \r stray carriage return  ");
        // Still only ONE line here (no \n), so the whole thing collapses/trims.
        assert_eq!(
            derive_title(&message),
            "first line with tabs and stray carriage return"
        );
    }

    /// `TITLE_MAX_CHARS` moved from 60 to ~80 per the architect ruling.
    #[test]
    fn derive_title_truncates_long_first_line_to_eighty_chars_with_ellipsis() {
        let long_line = "x".repeat(100);
        let message = Message::user_text(long_line);
        let title = derive_title(&message);
        assert_eq!(title, format!("{}...", "x".repeat(80)));
    }

    /// A message whose first content block is a `ToolUse` (not `Text`) must
    /// still find and use the first REAL `Text` block, not blow up or return
    /// an empty title. Constructed directly since `Message::user_text` only
    /// ever produces a single `Text` block.
    #[test]
    fn derive_title_ignores_non_text_content_blocks_and_uses_first_text_block() {
        let message = rokr_core::Message {
            role: rokr_core::Role::User,
            content: vec![
                rokr_core::ContentBlock::ToolUse {
                    id: "tool-1".to_string(),
                    name: "some_tool".to_string(),
                    input: serde_json::json!({}),
                    cache_control: None,
                },
                rokr_core::ContentBlock::Text {
                    text: "the real first line\nsecond line".to_string(),
                    cache_control: None,
                },
            ],
        };
        assert_eq!(derive_title(&message), "the real first line");
    }
}
