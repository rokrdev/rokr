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
