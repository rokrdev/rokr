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
}
