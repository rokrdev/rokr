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
}
