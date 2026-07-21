//! Ticket 55 (headless-output-formats-and-permission-mode): the headless
//! result-object schema. `--output-format json` prints exactly one
//! [`ResultObject`] (and `stream-json` terminates its JSONL stream with the
//! same object -- see `crate::headless`), so this shape is schema v1: a
//! versioned, external contract now that headless output is consumed by
//! scripts and tooling, not just read by a human off a TUI. See
//! `docs/adr/0013-headless-output-schema.md` for the full versioning
//! discipline (why it's "v1", what a breaking change to it means, and why
//! that discipline lives here in a doc comment plus the ADR rather than as
//! a wire `schema_version` field).

use serde::Serialize;

/// Schema v1's outcome discriminant. `Success` is a normal completion.
/// `ErrorPermission` is set when a gated tool call was denied during this
/// run (see `crate::headless::HeadlessPermissionRequester`). `ErrorMaxTurns`
/// is reserved for a future turn-count cap -- `rokr_core::run_tool_loop` has
/// no such cap today (out of this ticket's scope to add one), so this
/// variant is currently unreachable in practice but kept in the schema
/// because the ticket's `## Context` documents exactly these three values;
/// any `run_submission` error that isn't the tracked permission-denial case
/// maps here today as the closest fit among the three documented subtypes
/// (see `crate::headless::run`'s doc comment and this ticket's report for
/// the reasoning).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Subtype {
    Success,
    ErrorMaxTurns,
    ErrorPermission,
}

/// Schema v1's `usage` field: a serializable mirror of `rokr_core::Usage`
/// (which itself derives no `serde` impls -- see that type's own doc
/// comment on why `rokr-core` types only derive `Serialize`/`Deserialize`
/// for rokr's own persistence). Kept as a distinct, headless-output-owned
/// type rather than adding `Serialize` to `rokr_core::Usage` itself, since
/// `rokr-core` is outside this ticket's `files-touched`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct UsageObject {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl From<rokr_core::Usage> for UsageObject {
    fn from(usage: rokr_core::Usage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
        }
    }
}

/// Schema v1 (see this module's doc comment and
/// `docs/adr/0013-headless-output-schema.md`): the exact eight fields the
/// ticket's acceptance line documents. `--output-format json` prints
/// exactly one of these, serialized with `serde_json::to_string`;
/// `stream-json` terminates its JSONL stream with the identical object (see
/// `crate::headless::run`).
#[derive(Debug, Clone, Serialize)]
pub struct ResultObject {
    pub subtype: Subtype,
    pub session_id: String,
    pub result: String,
    pub is_error: bool,
    pub usage: UsageObject,
    /// Provisional: always `0.0` until ticket 57's pricing math lands (the
    /// field is present now, per the ticket's `## Context`, so downstream
    /// tooling can rely on the key's presence today and only its value
    /// changes when ticket 57 lands -- not a schema change).
    pub cost_usd: f64,
    pub num_turns: u32,
    pub duration_ms: u64,
}

impl ResultObject {
    /// The exit-code contract's success/failure half (0 vs 1) -- CLI misuse
    /// (exit 2) is decided earlier, before a `ResultObject` even exists
    /// (see `crate::headless::run`), so this method only ever distinguishes
    /// `Success` from the two error subtypes.
    pub fn exit_code(&self) -> std::process::ExitCode {
        match self.subtype {
            Subtype::Success => std::process::ExitCode::SUCCESS,
            Subtype::ErrorMaxTurns | Subtype::ErrorPermission => std::process::ExitCode::FAILURE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The result object is the wire contract the ticket's acceptance line
    /// documents: `subtype`, `session_id`, `result`, `is_error`, `usage`,
    /// `cost_usd`, `num_turns`, `duration_ms` (eight fields as the
    /// acceptance line lists them; the test's own name says "seven" -- a
    /// drift in the ticket text, see this ticket's report -- all eight are
    /// asserted here since the acceptance line is the more specific,
    /// enumerated source of truth). Serializing a `ResultObject` must
    /// produce a JSON *object* with every one of those keys present, with
    /// values that round-trip the fields set on it.
    #[test]
    fn result_object_serializes_all_seven_documented_fields() {
        let result = ResultObject {
            subtype: Subtype::Success,
            session_id: "01ABCDEF".to_string(),
            result: "final reply text".to_string(),
            is_error: false,
            usage: UsageObject {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 1,
                cache_write_tokens: 2,
            },
            cost_usd: 0.0,
            num_turns: 3,
            duration_ms: 42,
        };

        let json = serde_json::to_value(&result).expect("ResultObject must serialize to JSON");
        let obj = json
            .as_object()
            .expect("ResultObject must serialize as a JSON object");

        for field in [
            "subtype",
            "session_id",
            "result",
            "is_error",
            "usage",
            "cost_usd",
            "num_turns",
            "duration_ms",
        ] {
            assert!(
                obj.contains_key(field),
                "expected field `{field}` in the serialized result object, got: {json}"
            );
        }

        assert_eq!(obj["subtype"], serde_json::json!("success"));
        assert_eq!(obj["session_id"], serde_json::json!("01ABCDEF"));
        assert_eq!(obj["result"], serde_json::json!("final reply text"));
        assert_eq!(obj["is_error"], serde_json::json!(false));
        assert_eq!(obj["cost_usd"], serde_json::json!(0.0));
        assert_eq!(obj["num_turns"], serde_json::json!(3));
        assert_eq!(obj["duration_ms"], serde_json::json!(42));
        assert_eq!(
            obj["usage"],
            serde_json::json!({
                "input_tokens": 10,
                "output_tokens": 20,
                "cache_read_tokens": 1,
                "cache_write_tokens": 2,
            })
        );
    }
}
