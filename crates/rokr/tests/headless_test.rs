//! Ticket 54 (headless-print-mode-text-output) acceptance tests: `rokr
//! -p/--print <prompt>` runs headless -- no TUI -- against a single prompt
//! and prints only the final assistant text to stdout, exiting 0 on
//! success. A `<prompt>` of `-` reads the prompt from stdin instead of the
//! argument.

use assert_cmd::Command;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Creates a fresh, uniquely-named directory under the system temp dir,
/// mirroring `cli_test.rs`'s own helper of the same name.
fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "rokr-headless-test-{label}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `rokr -p "<prompt>"` against a scripted OpenAI-compatible provider must
/// print EXACTLY the final assistant text to stdout -- no framing, no
/// tool-call chatter -- and exit 0.
#[tokio::test]
async fn print_flag_writes_only_final_assistant_text_and_exits_zero() {
    const MARKER: &str = "HeadlessPrintReplyMarker3391";

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": MARKER}}]
        })))
        .mount(&mock)
        .await;

    let home = unique_temp_dir("print-home");
    let xdg_config_home = unique_temp_dir("print-xdg-config-home");

    let mut cmd = Command::cargo_bin("rokr").unwrap();
    let assert = cmd
        .arg("-p")
        .arg("say hi")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("ROKR_OPENAI_BASE_URL", mock.uri())
        .env("ROKR_OPENAI_MODEL", "gpt-4o-mini")
        .env("ROKR_OPENAI_API_KEY", "test-key")
        .assert()
        .success();

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        MARKER,
        "expected stdout to be exactly the final assistant text with no framing or \
         tool-call output, got: {stdout:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

/// `echo "say hi" | rokr -p -` must produce the identical stdout result as
/// `rokr -p "say hi"` -- the `-` argument reads the prompt from stdin
/// instead of being treated as the literal prompt text.
#[tokio::test]
async fn piped_stdin_prompt_via_dash_argument_produces_same_result_as_inline_prompt() {
    const MARKER: &str = "HeadlessStdinReplyMarker7742";

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": MARKER}}]
        })))
        .mount(&mock)
        .await;

    let home = unique_temp_dir("stdin-home");
    let xdg_config_home = unique_temp_dir("stdin-xdg-config-home");

    let mut cmd = Command::cargo_bin("rokr").unwrap();
    let assert = cmd
        .arg("-p")
        .arg("-")
        .write_stdin("say hi\n")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("ROKR_OPENAI_BASE_URL", mock.uri())
        .env("ROKR_OPENAI_MODEL", "gpt-4o-mini")
        .env("ROKR_OPENAI_API_KEY", "test-key")
        .assert()
        .success();

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        MARKER,
        "expected stdin-sourced prompt to produce the same exact final-assistant-text \
         stdout as the inline-prompt case, got: {stdout:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

/// Ticket 55 (headless-output-formats-and-permission-mode): `--output-format
/// json` against a scripted (tool-call-free) provider must print EXACTLY
/// one parseable JSON object to stdout -- no framing text before or after
/// it -- carrying all eight documented result-object fields (the
/// acceptance line's field list; see `crates/rokr-app/src/result_schema.rs`
/// for why "seven" in this test's own mandated name is a ticket-text
/// drift), and exit 0.
#[tokio::test]
async fn json_output_format_produces_one_parseable_result_object_with_required_fields() {
    const MARKER: &str = "HeadlessJsonReplyMarker5510";

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": MARKER}}]
        })))
        .mount(&mock)
        .await;

    let home = unique_temp_dir("json-home");
    let xdg_config_home = unique_temp_dir("json-xdg-config-home");

    let mut cmd = Command::cargo_bin("rokr").unwrap();
    let assert = cmd
        .arg("-p")
        .arg("say hi")
        .arg("--output-format")
        .arg("json")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("ROKR_OPENAI_BASE_URL", mock.uri())
        .env("ROKR_OPENAI_MODEL", "gpt-4o-mini")
        .env("ROKR_OPENAI_API_KEY", "test-key")
        .assert()
        .success();

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim_end().lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "expected exactly one line of stdout (one JSON result object, no framing), got: {stdout:?}"
    );

    let result: serde_json::Value =
        serde_json::from_str(lines[0]).expect("the single stdout line must be parseable JSON");
    let obj = result
        .as_object()
        .expect("the result object must be a JSON object");

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
            "expected field `{field}` in the JSON result object, got: {result}"
        );
    }

    assert_eq!(obj["subtype"], serde_json::json!("success"));
    assert_eq!(obj["is_error"], serde_json::json!(false));
    assert_eq!(obj["result"], serde_json::json!(MARKER));
    assert!(
        obj["session_id"].as_str().is_some_and(|s| !s.is_empty()),
        "expected a non-empty session_id, got: {result}"
    );
    assert_eq!(obj["cost_usd"], serde_json::json!(0.0));
    assert!(obj["num_turns"].as_u64().is_some());
    assert!(obj["duration_ms"].as_u64().is_some());
    assert!(obj["usage"].is_object());

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

/// Ticket 57 (cost-command-and-headless-reporting): the JSON result
/// object's `cost_usd` must be computed via `rokr_core::pricing::calculate_cost`
/// against the run's own reported usage -- NOT the `0.0` placeholder ticket
/// 55 hardcoded. The test above
/// (`json_output_format_produces_one_parseable_result_object_with_required_fields`)
/// also asserts `cost_usd == 0.0`, but that's a fake-green for THIS
/// ticket's wiring: its mocked response carries no `usage` object at all,
/// so `cost_usd` would be `0.0` regardless of whether real pricing math is
/// wired in (zero tokens * any rate is still zero). This test uses
/// `gpt-4o-mini` -- one of the two models with non-zero entries in
/// `rokr_config::default_model_pricing` (the other, `claude-3-5-sonnet-20241022`,
/// is the Anthropic backend, not exercised by this OpenAI-mock harness) --
/// AND mocks a real, non-zero `usage` object, so a genuinely non-zero
/// `cost_usd` can only appear if the pricing math actually ran.
#[tokio::test]
async fn headless_json_result_cost_usd_matches_pricing_math_for_run_usage() {
    const MARKER: &str = "HeadlessCostUsdReplyMarker3321";

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": MARKER}}],
            "usage": {
                "prompt_tokens": 800000,
                "completion_tokens": 500000,
                "prompt_tokens_details": { "cached_tokens": 200000 }
            }
        })))
        .mount(&mock)
        .await;

    let home = unique_temp_dir("cost-usd-home");
    let xdg_config_home = unique_temp_dir("cost-usd-xdg-config-home");

    let mut cmd = Command::cargo_bin("rokr").unwrap();
    let assert = cmd
        .arg("-p")
        .arg("say hi")
        .arg("--output-format")
        .arg("json")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("ROKR_OPENAI_BASE_URL", mock.uri())
        .env("ROKR_OPENAI_MODEL", "gpt-4o-mini")
        .env("ROKR_OPENAI_API_KEY", "test-key")
        .assert()
        .success();

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let result: serde_json::Value =
        serde_json::from_str(stdout.trim_end()).expect("stdout must be one parseable JSON object");

    // The same `gpt-4o-mini` pricing entry `rokr_config::default_model_pricing`
    // hardcodes (that function is private to `rokr-config`, so this
    // duplicates its literal per-token rates rather than calling it) and
    // the exact usage the mocked response above reports, run through the
    // SAME `calculate_cost` function `crates/rokr-app/src/headless.rs`
    // must now call.
    let usage = rokr_core::Usage {
        input_tokens: 800_000,
        output_tokens: 500_000,
        cache_read_tokens: 200_000,
        cache_write_tokens: 0,
    };
    let pricing = rokr_core::pricing::PricingEntry {
        input_price_per_token: 0.000_000_15,
        output_price_per_token: 0.000_000_6,
        cache_read_price_per_token: 0.000_000_075,
        cache_write_price_per_token: 0.000_000_15,
    };
    let expected_cost_usd = rokr_core::pricing::calculate_cost(usage, Some(&pricing));
    assert!(
        expected_cost_usd > 0.0,
        "test setup bug: expected a non-zero priced cost, got {expected_cost_usd}"
    );

    let actual_cost_usd = result["cost_usd"]
        .as_f64()
        .expect("cost_usd must be a JSON number");
    assert!(
        (actual_cost_usd - expected_cost_usd).abs() < 1e-9,
        "expected cost_usd ({actual_cost_usd}) to match calculate_cost's own result \
         ({expected_cost_usd}) for this run's usage, got full result: {result}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

/// Ticket 55: `--output-format stream-json` against the same kind of
/// scripted provider must print JSONL -- every line but the last a
/// parseable JSON "event" object -- with the LAST line being a full,
/// parseable result object matching the exact same schema/values
/// `--output-format json` alone would produce for this run (subtype,
/// session_id, result, is_error, usage, cost_usd, num_turns, duration_ms).
#[tokio::test]
async fn stream_json_output_is_valid_jsonl_terminated_by_json_mode_result_object() {
    const MARKER: &str = "HeadlessStreamJsonReplyMarker6621";

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": MARKER}}]
        })))
        .mount(&mock)
        .await;

    let home = unique_temp_dir("stream-json-home");
    let xdg_config_home = unique_temp_dir("stream-json-xdg-config-home");

    let mut cmd = Command::cargo_bin("rokr").unwrap();
    let assert = cmd
        .arg("-p")
        .arg("say hi")
        .arg("--output-format")
        .arg("stream-json")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("ROKR_OPENAI_BASE_URL", mock.uri())
        .env("ROKR_OPENAI_MODEL", "gpt-4o-mini")
        .env("ROKR_OPENAI_API_KEY", "test-key")
        .assert()
        .success();

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim_end().lines().collect();
    assert!(
        lines.len() >= 2,
        "expected at least one event line plus the terminating result object, got: {stdout:?}"
    );

    for line in &lines[..lines.len() - 1] {
        let event: serde_json::Value =
            serde_json::from_str(line).expect("every non-terminal JSONL line must be valid JSON");
        assert!(
            event.get("type").is_some(),
            "expected every event line to carry a `type` field, got: {event}"
        );
    }

    let result: serde_json::Value = serde_json::from_str(lines[lines.len() - 1])
        .expect("the terminating line must be a parseable JSON result object");
    let obj = result
        .as_object()
        .expect("the terminating line must be a JSON object");

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
            "expected field `{field}` in the terminating result object, got: {result}"
        );
    }
    assert_eq!(obj["subtype"], serde_json::json!("success"));
    assert_eq!(obj["is_error"], serde_json::json!(false));
    assert_eq!(obj["result"], serde_json::json!(MARKER));

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

/// Ticket 55: with `--agent build` (so a gated tool -- `bash` -- is actually
/// in the tool set) and NO `--permission-mode` flag, a gated tool call the
/// model attempts must be denied by default -- never silently allowed --
/// and the run must exit 1 with `subtype: "error_permission"`. Proven two
/// ways: the JSON result object carries that subtype, AND the marker file
/// the scripted bash command would have created never appears on disk.
#[tokio::test]
async fn default_permission_mode_denies_gated_tool_call_without_flag() {
    let mock = MockServer::start().await;

    let temp_dir = unique_temp_dir("denied-bash-marker");
    let marker_file = temp_dir.join("should-never-be-created.txt");
    let marker_path = marker_file.to_string_lossy().into_owned();

    // First call: the model asks to run a `bash` command that would touch
    // the marker file. `up_to_n_times(1)` plus insertion-order priority
    // means this stops matching after its one hit (mirrors
    // `tui_test.rs`'s tool-call mock pattern).
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-tool-call",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "bash",
                            "arguments": serde_json::json!({
                                "command": format!("touch {marker_path}")
                            }).to_string()
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })))
        .up_to_n_times(1)
        .mount(&mock)
        .await;

    // Second call: the loop feeds the (denied) tool result back and the
    // model replies with a final, tool-call-free text answer.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-final",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "acknowledged the denial"},
                "finish_reason": "stop"
            }]
        })))
        .mount(&mock)
        .await;

    let home = unique_temp_dir("denied-home");
    let xdg_config_home = unique_temp_dir("denied-xdg-config-home");

    let mut cmd = Command::cargo_bin("rokr").unwrap();
    let assert = cmd
        .arg("--agent")
        .arg("build")
        .arg("-p")
        .arg("please run a command")
        .arg("--output-format")
        .arg("json")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("ROKR_OPENAI_BASE_URL", mock.uri())
        .env("ROKR_OPENAI_MODEL", "gpt-4o-mini")
        .env("ROKR_OPENAI_API_KEY", "test-key")
        .assert()
        .failure()
        .code(1);

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let result: serde_json::Value = serde_json::from_str(stdout.trim_end())
        .expect("even a denied-permission run must print one parseable JSON result object");
    assert_eq!(result["subtype"], serde_json::json!("error_permission"));
    assert_eq!(result["is_error"], serde_json::json!(true));

    assert!(
        !marker_file.exists(),
        "the denied bash command must never have executed -- found marker file at {marker_path}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&temp_dir);
}

/// F-005: `rokr_core::run_tool_loop`'s `max_iterations` cap
/// (`crate::headless::HEADLESS_MAX_ITERATIONS` in `rokr-app`) must stop an
/// unattended headless run against a misbehaving/looping provider -- one
/// that NEVER emits a tool-call-free reply, so without the cap the loop
/// would run forever. Plan tier (the default agent, no `--agent` flag) is
/// used deliberately: `read` is never gated, so every round trip actually
/// executes and loops back to the provider, with no permission prompt ever
/// in play to confound the assertion below with `error_permission` instead.
#[tokio::test]
async fn looping_provider_terminates_via_max_iterations_with_error_max_turns_subtype() {
    let mock = MockServer::start().await;

    // Every single call -- there is no second, terminating mock -- returns
    // a fresh `read` tool call, so the loop never sees a tool-call-free
    // reply on its own and must be stopped by the `max_iterations` cap.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-looping",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_loop",
                        "type": "function",
                        "function": {
                            "name": "read",
                            "arguments": serde_json::json!({
                                "path": "/nonexistent/max-iterations-probe.txt"
                            }).to_string()
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })))
        .mount(&mock)
        .await;

    let home = unique_temp_dir("max-iter-home");
    let xdg_config_home = unique_temp_dir("max-iter-xdg-config-home");

    let mut cmd = Command::cargo_bin("rokr").unwrap();
    let assert = cmd
        .arg("-p")
        .arg("read forever")
        .arg("--output-format")
        .arg("json")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("ROKR_OPENAI_BASE_URL", mock.uri())
        .env("ROKR_OPENAI_MODEL", "gpt-4o-mini")
        .env("ROKR_OPENAI_API_KEY", "test-key")
        .assert()
        .failure()
        .code(1);

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let result: serde_json::Value = serde_json::from_str(stdout.trim_end())
        .expect("even a max-iterations run must print one parseable JSON result object");
    assert_eq!(result["subtype"], serde_json::json!("error_max_turns"));
    assert_eq!(result["is_error"], serde_json::json!(true));

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

/// F-006 (1/2): `--permission-mode bypass` WITHOUT the accompanying
/// `--dangerously-skip-permissions` flag is CLI misuse
/// (`crate::headless::build_permission_requester`) -- caught before any
/// session/provider bootstrap even starts, so this test needs no mock
/// provider at all. Must exit 2 (the CLI-misuse code, distinct from exit 1
/// for an agent/runtime error) with a non-empty stderr message naming the
/// missing flag.
#[tokio::test]
async fn bypass_permission_mode_without_dangerously_skip_permissions_exits_two_with_stderr_error()
{
    let home = unique_temp_dir("bypass-misuse-home");
    let xdg_config_home = unique_temp_dir("bypass-misuse-xdg-config-home");

    let mut cmd = Command::cargo_bin("rokr").unwrap();
    let assert = cmd
        .arg("-p")
        .arg("say hi")
        .arg("--permission-mode")
        .arg("bypass")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .assert()
        .failure()
        .code(2);

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.trim().is_empty(),
        "expected no stdout for a CLI-misuse exit, got: {stdout:?}"
    );
    assert!(
        stderr.contains("--dangerously-skip-permissions"),
        "expected stderr to name the missing --dangerously-skip-permissions flag, got: {stderr:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

/// F-006 (2/2): `--permission-mode accept-edits`'s documented behavior
/// (`crate::headless::HeadlessPermissionRequester::request`) is "grant only
/// a write/edit `Diff` call, still deny everything else (bash, MCP) exactly
/// like `deny`". This is deliberately ONE test exercising BOTH halves of
/// that `matches!` arm against a real `--agent build` run, rather than two
/// separate tests each checking only a grant or only a denial: a test that
/// only checked the grant half would stay green even if the arm were
/// inverted (denying diffs, granting everything else), since an inverted
/// arm still "grants something". Checking both halves together is what
/// makes an inverted `matches!` arm provably fail this test.
///
/// The write-tool half proves the grant is REAL (the file is actually
/// written, not just a happy-path subtype), and the bash half reuses the
/// same marker-file non-execution proof
/// `default_permission_mode_denies_gated_tool_call_without_flag` above
/// uses for `deny` mode.
#[tokio::test]
async fn accept_edits_permission_mode_grants_file_writes_but_denies_bash_execution() {
    // --- Half 1: a write-tool call must be GRANTED (file actually written,
    // result subtype success). ---
    let write_mock = MockServer::start().await;
    let write_temp_dir = unique_temp_dir("accept-edits-write-target");
    let target_file = write_temp_dir.join("agent-written.txt");
    let target_path = target_file.to_string_lossy().into_owned();
    const WRITTEN_CONTENT: &str = "written under accept-edits";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-write-call",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_write",
                        "type": "function",
                        "function": {
                            "name": "write",
                            "arguments": serde_json::json!({
                                "path": target_path,
                                "content": WRITTEN_CONTENT
                            }).to_string()
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })))
        .up_to_n_times(1)
        .mount(&write_mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-write-final",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "wrote the file"},
                "finish_reason": "stop"
            }]
        })))
        .mount(&write_mock)
        .await;

    let write_home = unique_temp_dir("accept-edits-write-home");
    let write_xdg_config_home = unique_temp_dir("accept-edits-write-xdg-config-home");

    let mut write_cmd = Command::cargo_bin("rokr").unwrap();
    let write_assert = write_cmd
        .arg("--agent")
        .arg("build")
        .arg("--permission-mode")
        .arg("accept-edits")
        .arg("-p")
        .arg("please write the file")
        .arg("--output-format")
        .arg("json")
        .current_dir(&write_temp_dir)
        .env("HOME", &write_home)
        .env("XDG_CONFIG_HOME", &write_xdg_config_home)
        .env("ROKR_OPENAI_BASE_URL", write_mock.uri())
        .env("ROKR_OPENAI_MODEL", "gpt-4o-mini")
        .env("ROKR_OPENAI_API_KEY", "test-key")
        .assert()
        .success();

    let write_output = write_assert.get_output();
    let write_stdout = String::from_utf8_lossy(&write_output.stdout);
    let write_result: serde_json::Value = serde_json::from_str(write_stdout.trim_end())
        .expect("the write-grant run must print one parseable JSON result object");
    assert_eq!(write_result["subtype"], serde_json::json!("success"));
    assert_eq!(write_result["is_error"], serde_json::json!(false));
    assert_eq!(
        std::fs::read_to_string(&target_file).expect("the write call must have been granted"),
        WRITTEN_CONTENT,
        "accept-edits must grant a write/edit (Diff) call -- the file must actually be written"
    );

    let _ = std::fs::remove_dir_all(&write_home);
    let _ = std::fs::remove_dir_all(&write_xdg_config_home);
    let _ = std::fs::remove_dir_all(&write_temp_dir);

    // --- Half 2: a bash call must be DENIED (marker file never created,
    // result subtype error_permission) -- same accept-edits mode, same
    // agent tier, only the requested tool differs. ---
    let bash_mock = MockServer::start().await;
    let bash_temp_dir = unique_temp_dir("accept-edits-bash-marker");
    let marker_file = bash_temp_dir.join("should-never-be-created.txt");
    let marker_path = marker_file.to_string_lossy().into_owned();

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-bash-call",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_bash",
                        "type": "function",
                        "function": {
                            "name": "bash",
                            "arguments": serde_json::json!({
                                "command": format!("touch {marker_path}")
                            }).to_string()
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })))
        .up_to_n_times(1)
        .mount(&bash_mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-bash-final",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "acknowledged the denial"},
                "finish_reason": "stop"
            }]
        })))
        .mount(&bash_mock)
        .await;

    let bash_home = unique_temp_dir("accept-edits-bash-home");
    let bash_xdg_config_home = unique_temp_dir("accept-edits-bash-xdg-config-home");

    let mut bash_cmd = Command::cargo_bin("rokr").unwrap();
    let bash_assert = bash_cmd
        .arg("--agent")
        .arg("build")
        .arg("--permission-mode")
        .arg("accept-edits")
        .arg("-p")
        .arg("please run a command")
        .arg("--output-format")
        .arg("json")
        .env("HOME", &bash_home)
        .env("XDG_CONFIG_HOME", &bash_xdg_config_home)
        .env("ROKR_OPENAI_BASE_URL", bash_mock.uri())
        .env("ROKR_OPENAI_MODEL", "gpt-4o-mini")
        .env("ROKR_OPENAI_API_KEY", "test-key")
        .assert()
        .failure()
        .code(1);

    let bash_output = bash_assert.get_output();
    let bash_stdout = String::from_utf8_lossy(&bash_output.stdout);
    let bash_result: serde_json::Value = serde_json::from_str(bash_stdout.trim_end())
        .expect("the bash-denial run must print one parseable JSON result object");
    assert_eq!(bash_result["subtype"], serde_json::json!("error_permission"));
    assert_eq!(bash_result["is_error"], serde_json::json!(true));
    assert!(
        !marker_file.exists(),
        "accept-edits must deny a bash call -- it must never actually execute, found marker \
         file at {marker_path}"
    );

    let _ = std::fs::remove_dir_all(&bash_home);
    let _ = std::fs::remove_dir_all(&bash_xdg_config_home);
    let _ = std::fs::remove_dir_all(&bash_temp_dir);
}

/// F-007 (PRD story 10, pre-ship review) acceptance test: `rokr -p "<prompt
/// with @skill:<name>>"` must resolve the mention before submission --
/// `CommandRegistry::resolve_skills` is applied to headless's raw `-p`
/// prompt in `run_result_object` the same way it's applied to a plain
/// TUI-typed prompt (see `tui_test.rs`'s
/// `plain_prompt_with_skill_mention_inlines_skill_file_contents_in_outgoing_request`).
/// Inspects the wire-level outgoing request body the mock provider
/// transport actually received to confirm the skill file's full contents
/// landed there, not just the intermediate prompt string.
#[tokio::test]
async fn print_flag_prompt_with_skill_mention_inlines_skill_file_contents_in_outgoing_request() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}}]
        })))
        .mount(&mock)
        .await;

    let home = unique_temp_dir("skill-mention-home");
    let xdg_config_home = unique_temp_dir("skill-mention-xdg-config-home");

    // User-scope skills/ directory (config_dir/skills/) -- a headless `-p`
    // run has no project cwd fixture wired up here, so this exercises the
    // user-scope skill discovery path.
    let user_skills_dir = xdg_config_home.join("rokr").join("skills");
    std::fs::create_dir_all(&user_skills_dir)
        .expect("failed to create fixture user-scope skills directory");
    let skill_content = "SKILL-CONTENT-MARKER: headless plain-prompt skill resolution.";
    std::fs::write(user_skills_dir.join("code-style.md"), skill_content)
        .expect("failed to write fixture code-style.md");

    let mut cmd = Command::cargo_bin("rokr").unwrap();
    cmd.arg("-p")
        .arg("Please follow @skill:code-style")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("ROKR_OPENAI_BASE_URL", mock.uri())
        .env("ROKR_OPENAI_MODEL", "gpt-4o-mini")
        .env("ROKR_OPENAI_API_KEY", "test-key")
        .assert()
        .success();

    let received_requests = mock.received_requests().await.expect(
        "mock server should have recorded received requests \
         (make sure the request recorder wasn't disabled)",
    );
    assert!(
        !received_requests.is_empty(),
        "expected at least one outgoing request to /chat/completions"
    );
    let first_request_body = String::from_utf8_lossy(&received_requests[0].body).into_owned();
    assert!(
        first_request_body.contains(skill_content),
        "expected the outgoing request body to contain the skill file's full contents inlined \
         in place of the @skill:code-style mention in headless's raw -p prompt; got body: \
         {first_request_body:?}"
    );
    assert!(
        !first_request_body.contains("@skill:code-style"),
        "expected the @skill:code-style mention to have been replaced, not left literal; got \
         body: {first_request_body:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}
