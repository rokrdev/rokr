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

    /// Creates a new session: generates a ULID (chosen, per the PRD, so
    /// lexicographic directory sort order is also chronological order),
    /// creates `sessions/<ulid>/`, opens `session.jsonl` for append, and
    /// spawns the single dedicated writer task that owns that file handle
    /// for the rest of this session's life. Returns a [`SessionHandle`]
    /// exposing only typed append methods -- callers never see the raw
    /// file or channel.
    pub fn create_session(&self) -> std::io::Result<SessionHandle> {
        let session_id = ulid::Ulid::new().to_string();
        let session_dir = self.data_dir.join("sessions").join(&session_id);
        std::fs::create_dir_all(&session_dir)?;

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(session_dir.join("session.jsonl"))?;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(run_writer(file, rx));

        Ok(SessionHandle { session_id, tx })
    }
}

/// The single dedicated writer task (PRD decision 1: "One ordered writer
/// per store"). Owns `file` for its entire lifetime; every `Append` is
/// serialized to one JSON line and written in the order it was enqueued,
/// so a parent's and a subagent's turns interleave correctly without any
/// file lock.
async fn run_writer(
    file: std::fs::File,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<WriterCommand>,
) {
    use tokio::io::AsyncWriteExt;

    let mut file = tokio::fs::File::from_std(file);
    while let Some(command) = rx.recv().await {
        match command {
            WriterCommand::Append(record) => {
                if let Ok(mut line) = serde_json::to_string(&record) {
                    line.push('\n');
                    let _ = file.write_all(line.as_bytes()).await;
                }
            }
            WriterCommand::Flush(ack) => {
                let _ = file.flush().await;
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
}
