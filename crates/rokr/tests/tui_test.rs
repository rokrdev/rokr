use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Creates a fresh, uniquely-named directory under the system temp dir.
fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "rokr-tui-test-{label}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn tui_renders_three_sections_and_quits_on_q() {
    let home = unique_temp_dir("home");
    let xdg_config_home = unique_temp_dir("xdg-config-home");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &xdg_config_home);

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("failed to spawn rokr in pty");
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone pty reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut output = String::new();
    let render_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < render_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Header") && output.contains("View") && output.contains("Prompt") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("Header"),
        "expected pty output to contain Header, got: {output:?}"
    );
    assert!(
        output.contains("View"),
        "expected pty output to contain View, got: {output:?}"
    );
    assert!(
        output.contains("Prompt"),
        "expected pty output to contain Prompt, got: {output:?}"
    );

    {
        let mut writer = pair
            .master
            .take_writer()
            .expect("failed to take pty writer");
        writer.write_all(b"q").expect("failed to write q to pty");
    }

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
            break status;
        }
        if Instant::now() > exit_deadline {
            let _ = child.kill();
            panic!("rokr did not exit within timeout after pressing q; output so far: {output:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };

    assert!(
        status.success(),
        "expected rokr to exit cleanly after q, got status: {status:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

#[tokio::test]
async fn typed_prompt_renders_model_response_in_view() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    // Single tokens (no spaces): ratatui only redraws cells that actually
    // changed, so a multi-word phrase's raw ANSI byte stream can have
    // cursor-jump gaps where a space cell was already blank and thus never
    // rewritten — a literal substring match on the raw pty bytes would then
    // spuriously fail even though the rendered screen is correct.
    let canned_response = "MockedAssistantReplyForTesting";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": canned_response
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-prompt");
    let xdg_config_home = unique_temp_dir("xdg-config-home-prompt");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("failed to spawn rokr in pty");
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone pty reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = pair
        .master
        .take_writer()
        .expect("failed to take pty writer");

    let mut output = String::new();
    let render_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < render_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Header") && output.contains("View") && output.contains("Prompt") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("Header"),
        "expected pty output to contain Header, got: {output:?}"
    );

    writer
        .write_all(b"helloworld\r")
        .expect("failed to write prompt to pty");

    let response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(canned_response) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("helloworld"),
        "expected pty output to contain the typed prompt, got: {output:?}"
    );
    assert!(
        output.contains(canned_response),
        "expected pty output to contain the mocked assistant response, got: {output:?}"
    );

    writer.write_all(b"q").expect("failed to write q to pty");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
            break status;
        }
        if Instant::now() > exit_deadline {
            let _ = child.kill();
            panic!("rokr did not exit within timeout after pressing q; output so far: {output:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };

    assert!(
        status.success(),
        "expected rokr to exit cleanly after q, got status: {status:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

#[tokio::test]
async fn typed_prompt_triggers_read_tool_call_and_renders_final_reply() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    // Single tokens (no spaces) for the same reason as
    // `typed_prompt_renders_model_response_in_view`: ratatui only redraws
    // changed cells, so a literal substring match on raw pty bytes needs to
    // avoid spaces that might not get rewritten.
    let final_reply_text = "FinalReplyAfterToolCallForTesting";

    let temp_dir = unique_temp_dir("read-tool-target");
    let target_file = temp_dir.join("target.txt");
    std::fs::write(&target_file, "contents used only by the read tool").unwrap();
    let target_path = target_file.to_string_lossy().into_owned();

    // First call: the model asks to invoke the `read` tool against the real
    // temp file. `up_to_times(1)` plus insertion-order priority means this
    // mock stops matching after its one hit, so the second (broader) mock
    // below is used for every call after that.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-tool-call",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "tool_calls": [
                            {
                                "id": "call_1",
                                "type": "function",
                                "function": {
                                    "name": "read",
                                    "arguments": serde_json::json!({ "path": target_path }).to_string()
                                }
                            }
                        ]
                    },
                    "finish_reason": "tool_calls"
                }
            ]
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second call: the loop feeds the tool result back, and the model
    // replies with a final, tool-call-free text answer.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-final",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": final_reply_text
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-tool-loop");
    let xdg_config_home = unique_temp_dir("xdg-config-home-tool-loop");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("failed to spawn rokr in pty");
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone pty reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = pair
        .master
        .take_writer()
        .expect("failed to take pty writer");

    let mut output = String::new();
    let render_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < render_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Header") && output.contains("View") && output.contains("Prompt") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("Header"),
        "expected pty output to contain Header, got: {output:?}"
    );

    writer
        .write_all(b"readthefile\r")
        .expect("failed to write prompt to pty");

    let response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(final_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("readthefile"),
        "expected pty output to contain the typed prompt, got: {output:?}"
    );
    assert!(
        output.contains(final_reply_text),
        "expected pty output to contain the final assistant reply after the tool call loop \
         completed, got: {output:?}"
    );

    writer.write_all(b"q").expect("failed to write q to pty");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
            break status;
        }
        if Instant::now() > exit_deadline {
            let _ = child.kill();
            panic!("rokr did not exit within timeout after pressing q; output so far: {output:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };

    assert!(
        status.success(),
        "expected rokr to exit cleanly after q, got status: {status:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn second_prompt_includes_prior_turn_in_request_body() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    // Single tokens (no spaces) for the same reason as the other tests:
    // ratatui only redraws changed cells, so a literal substring match on
    // raw pty bytes needs to avoid spaces that might not get rewritten.
    let first_reply_text = "FirstReplyTokenForTesting";
    let second_reply_text = "SecondReplyTokenForTesting";

    // First call gets a distinct reply so we can tell it apart on screen
    // from the second call's reply. `up_to_n_times(1)` plus insertion-order
    // priority means this mock stops matching after its one hit, so the
    // broader mock below handles every call after that.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-first",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": first_reply_text
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-second",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": second_reply_text
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-second-turn");
    let xdg_config_home = unique_temp_dir("xdg-config-home-second-turn");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("failed to spawn rokr in pty");
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone pty reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = pair
        .master
        .take_writer()
        .expect("failed to take pty writer");

    let mut output = String::new();
    let render_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < render_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Header") && output.contains("View") && output.contains("Prompt") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("Header"),
        "expected pty output to contain Header, got: {output:?}"
    );

    writer
        .write_all(b"firstpromptunique\r")
        .expect("failed to write first prompt to pty");

    let first_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < first_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(first_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("firstpromptunique"),
        "expected pty output to contain the first typed prompt, got: {output:?}"
    );
    assert!(
        output.contains(first_reply_text),
        "expected pty output to contain the first mocked assistant response, got: {output:?}"
    );

    writer
        .write_all(b"secondpromptunique\r")
        .expect("failed to write second prompt to pty");

    let second_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < second_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(second_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("secondpromptunique"),
        "expected pty output to contain the second typed prompt, got: {output:?}"
    );
    assert!(
        output.contains(second_reply_text),
        "expected pty output to contain the second mocked assistant response, got: {output:?}"
    );

    writer.write_all(b"q").expect("failed to write q to pty");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
            break status;
        }
        if Instant::now() > exit_deadline {
            let _ = child.kill();
            panic!("rokr did not exit within timeout after pressing q; output so far: {output:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };

    assert!(
        status.success(),
        "expected rokr to exit cleanly after q, got status: {status:?}"
    );

    let received_requests = mock_server
        .received_requests()
        .await
        .expect("request recording should be enabled on the mock server by default");

    assert!(
        received_requests.len() >= 2,
        "expected at least 2 requests to /chat/completions, got {}: {received_requests:?}",
        received_requests.len()
    );

    let first_request_body = String::from_utf8_lossy(&received_requests[0].body).into_owned();
    let second_request_body = String::from_utf8_lossy(&received_requests[1].body).into_owned();

    assert!(
        !first_request_body.contains("secondpromptunique"),
        "expected the first request body to NOT already contain the second prompt's text, \
         got: {first_request_body}"
    );

    assert!(
        second_request_body.contains("firstpromptunique"),
        "expected the second request body to contain the first prompt's text, proving the \
         prior turn was included in the conversation history, got: {second_request_body}"
    );
    assert!(
        second_request_body.contains(first_reply_text),
        "expected the second request body to contain the first turn's assistant reply text, \
         proving the prior turn was included in the conversation history, got: \
         {second_request_body}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

/// Ticket 21 (manual-compact-command) acceptance test: typing `/compact`
/// must synchronously call `rokr_core::compact_transcript` (no auto-compact
/// threshold check — that's the whole point of a manual command) and
/// actually replace the running transcript, proven by inspecting the
/// request body of the prompt submitted right after `/compact`.
///
/// `compact_transcript` only makes a summarization call when there's a
/// genuine "prefix" before the most recent user turn (see
/// `tail_start_index` in rokr-core) — with only one turn in the transcript,
/// it's a no-op. So this test submits *two* prompts before `/compact`
/// (rather than one) to give it something real to summarize: the first
/// turn becomes the summarized prefix, the second turn is the preserved
/// "tail" that must survive compaction verbatim.
///
/// Sequence of real `/chat/completions` requests: 1st = first prompt, 2nd =
/// second (pre-compact) prompt, 3rd = the compaction call itself, 4th = the
/// post-compact prompt.
#[tokio::test]
async fn typing_compact_command_compacts_transcript_immediately() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let first_reply_text = "FirstReplyBeforeCompactForTesting";
    let second_reply_text = "SecondReplyBeforeCompactForTesting";
    let compaction_summary_token = "CompactionSummaryTokenForManualCompactTesting";
    let post_compact_reply_text = "ThirdReplyAfterCompactForTesting";

    // 1st request: reply to the first pre-compact prompt.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-first",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": first_reply_text },
                    "finish_reason": "stop"
                }
            ]
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // 2nd request: reply to the second pre-compact prompt (this turn is the
    // "tail" that compaction must preserve verbatim).
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-second",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": second_reply_text },
                    "finish_reason": "stop"
                }
            ]
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // 3rd request: the compaction call's own summarization reply.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-compaction",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": compaction_summary_token },
                    "finish_reason": "stop"
                }
            ]
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // 4th+ request(s): catch-all reply to the post-compact prompt.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-post-compact",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": post_compact_reply_text },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-compact-command");
    let xdg_config_home = unique_temp_dir("xdg-config-home-compact-command");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("failed to spawn rokr in pty");
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone pty reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = pair
        .master
        .take_writer()
        .expect("failed to take pty writer");

    let mut output = String::new();
    let render_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < render_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Header") && output.contains("View") && output.contains("Prompt") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("Header"),
        "expected pty output to contain Header, got: {output:?}"
    );

    writer
        .write_all(b"firstpromptunique\r")
        .expect("failed to write first prompt to pty");

    let first_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < first_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(first_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(first_reply_text),
        "expected pty output to contain the first mocked assistant response, got: {output:?}"
    );

    writer
        .write_all(b"secondpromptbeforecompact\r")
        .expect("failed to write second prompt to pty");

    let second_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < second_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(second_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(second_reply_text),
        "expected pty output to contain the second mocked assistant response, got: {output:?}"
    );

    writer
        .write_all(b"/compact\r")
        .expect("failed to write /compact to pty");

    let compact_confirmation_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < compact_confirmation_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.to_lowercase().contains("compact") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.to_lowercase().contains("compact"),
        "expected pty output to contain a compaction confirmation after /compact, got: {output:?}"
    );

    writer
        .write_all(b"thirdpromptunique\r")
        .expect("failed to write third prompt to pty");

    let third_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < third_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(post_compact_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(post_compact_reply_text),
        "expected pty output to contain the post-compact mocked assistant response, got: {output:?}"
    );

    writer.write_all(b"q").expect("failed to write q to pty");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
            break status;
        }
        if Instant::now() > exit_deadline {
            let _ = child.kill();
            panic!("rokr did not exit within timeout after pressing q; output so far: {output:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };
    assert!(
        status.success(),
        "expected rokr to exit cleanly after q, got status: {status:?}"
    );

    let received_requests = mock_server
        .received_requests()
        .await
        .expect("request recording should be enabled on the mock server by default");

    assert!(
        received_requests.len() >= 4,
        "expected at least 4 requests to /chat/completions (2 pre-compact prompts + the \
         compaction call + the post-compact prompt), got {}: {received_requests:?}",
        received_requests.len()
    );

    let post_compact_request_body =
        String::from_utf8_lossy(&received_requests[3].body).into_owned();

    assert!(
        post_compact_request_body.contains(compaction_summary_token),
        "expected the post-compact prompt's request body to contain the compaction summary \
         token, proving the transcript was replaced with the compacted version, got: \
         {post_compact_request_body}"
    );
    assert!(
        !post_compact_request_body.contains("firstpromptunique"),
        "expected the post-compact prompt's request body to NOT contain the first (now \
         summarized-away) prompt's text, proving compaction actually replaced the transcript \
         rather than just appending to it, got: {post_compact_request_body}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

/// Ticket 17 (cache-breakpoint-activation) acceptance test: submitting two
/// consecutive prompts must produce two outgoing request bodies whose
/// static prefix (tool specs + the system segment) is byte-identical
/// despite the growing transcript, and the OpenAI adapter must never emit
/// an explicit `cache_control` wire directive (OpenAI-compatible endpoints
/// do implicit prefix caching; explicit emission is Anthropic-only, a later
/// phase). Copies `second_prompt_includes_prior_turn_in_request_body`'s
/// exact PTY/wiremock/env-var harness.
#[tokio::test]
async fn second_prompt_static_prefix_bytes_identical_to_first() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let first_reply_text = "FirstReplyTokenForTesting";
    let second_reply_text = "SecondReplyTokenForTesting";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-first",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": first_reply_text
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-second",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": second_reply_text
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-static-prefix");
    let xdg_config_home = unique_temp_dir("xdg-config-home-static-prefix");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("failed to spawn rokr in pty");
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone pty reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = pair
        .master
        .take_writer()
        .expect("failed to take pty writer");

    let mut output = String::new();
    let render_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < render_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Header") && output.contains("View") && output.contains("Prompt") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("Header"),
        "expected pty output to contain Header, got: {output:?}"
    );

    writer
        .write_all(b"firstpromptunique\r")
        .expect("failed to write first prompt to pty");

    let first_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < first_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(first_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains(first_reply_text),
        "expected pty output to contain the first mocked assistant response, got: {output:?}"
    );

    writer
        .write_all(b"secondpromptunique\r")
        .expect("failed to write second prompt to pty");

    let second_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < second_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(second_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains(second_reply_text),
        "expected pty output to contain the second mocked assistant response, got: {output:?}"
    );

    writer.write_all(b"q").expect("failed to write q to pty");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
            break status;
        }
        if Instant::now() > exit_deadline {
            let _ = child.kill();
            panic!("rokr did not exit within timeout after pressing q; output so far: {output:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };

    assert!(
        status.success(),
        "expected rokr to exit cleanly after q, got status: {status:?}"
    );

    let received_requests = mock_server
        .received_requests()
        .await
        .expect("request recording should be enabled on the mock server by default");

    assert!(
        received_requests.len() >= 2,
        "expected at least 2 requests to /chat/completions, got {}: {received_requests:?}",
        received_requests.len()
    );

    let first_request_body = String::from_utf8_lossy(&received_requests[0].body).into_owned();
    let second_request_body = String::from_utf8_lossy(&received_requests[1].body).into_owned();

    assert!(
        !first_request_body.contains("cache_control"),
        "the OpenAI adapter must never emit an explicit cache_control wire directive, \
         got first request body: {first_request_body}"
    );
    assert!(
        !second_request_body.contains("cache_control"),
        "the OpenAI adapter must never emit an explicit cache_control wire directive, \
         got second request body: {second_request_body}"
    );

    let first_parsed: serde_json::Value =
        serde_json::from_str(&first_request_body).expect("first request body should be valid JSON");
    let second_parsed: serde_json::Value = serde_json::from_str(&second_request_body)
        .expect("second request body should be valid JSON");

    let first_static_prefix = (
        first_parsed
            .get("tools")
            .cloned()
            .expect("first request body should have a tools array"),
        first_parsed["messages"]
            .get(0)
            .cloned()
            .expect("first request body should have at least one message"),
    );
    let second_static_prefix = (
        second_parsed
            .get("tools")
            .cloned()
            .expect("second request body should have a tools array"),
        second_parsed["messages"]
            .get(0)
            .cloned()
            .expect("second request body should have at least one message"),
    );

    let first_static_prefix_bytes =
        serde_json::to_vec(&first_static_prefix).expect("serialize first static prefix");
    let second_static_prefix_bytes =
        serde_json::to_vec(&second_static_prefix).expect("serialize second static prefix");

    assert_eq!(
        first_static_prefix_bytes, second_static_prefix_bytes,
        "the static prefix (tool specs + system segment) must be byte-identical across \
         requests despite the growing transcript; first: {first_static_prefix:?}, second: \
         {second_static_prefix:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

#[tokio::test]
async fn bash_tool_call_renders_permission_prompt_and_runs_on_accept() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let final_reply_text = "FinalReplyAfterBashAcceptForTesting";

    let temp_dir = unique_temp_dir("bash-accept-target");
    let marker_path = temp_dir.join("accept-marker");
    let marker_path_str = marker_path.to_string_lossy().into_owned();
    let bash_command = format!("touch {marker_path_str}");

    // First call: the model asks to invoke the `bash` tool with a harmless
    // command (touching a marker file in a temp dir so the test can assert
    // on the real side effect without depending on stdout capture).
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-bash-accept",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "tool_calls": [
                            {
                                "id": "call_1",
                                "type": "function",
                                "function": {
                                    "name": "bash",
                                    "arguments": serde_json::json!({ "command": bash_command }).to_string()
                                }
                            }
                        ]
                    },
                    "finish_reason": "tool_calls"
                }
            ]
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second call: the loop feeds the tool result back, and the model
    // replies with a final, tool-call-free text answer.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-bash-accept-final",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": final_reply_text
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-bash-accept");
    let xdg_config_home = unique_temp_dir("xdg-config-home-bash-accept");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");
    cmd.arg("--agent");
    cmd.arg("build");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("failed to spawn rokr in pty");
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone pty reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = pair
        .master
        .take_writer()
        .expect("failed to take pty writer");

    let mut output = String::new();
    let render_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < render_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Header") && output.contains("View") && output.contains("Prompt") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("Header"),
        "expected pty output to contain Header, got: {output:?}"
    );

    writer
        .write_all(b"runbash\r")
        .expect("failed to write prompt to pty");

    // Wait for the permission prompt to render, showing the tool name and
    // the command it would run, before we've granted anything.
    let prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("bash") && output.contains("accept-marker") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("bash"),
        "expected pty output to contain the tool name 'bash' in a permission prompt, got: {output:?}"
    );
    assert!(
        output.contains("accept-marker"),
        "expected pty output to contain the command in a permission prompt, got: {output:?}"
    );
    assert!(
        !marker_path.exists(),
        "the bash command must not have run before permission was granted"
    );

    writer
        .write_all(b"y")
        .expect("failed to write accept keypress to pty");

    let response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(final_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains(final_reply_text),
        "expected pty output to contain the final assistant reply after accepting the bash \
         tool call, got: {output:?}"
    );
    assert!(
        marker_path.exists(),
        "expected the bash command to have run (marker file created) after accepting"
    );

    writer.write_all(b"q").expect("failed to write q to pty");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
            break status;
        }
        if Instant::now() > exit_deadline {
            let _ = child.kill();
            panic!("rokr did not exit within timeout after pressing q; output so far: {output:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };

    assert!(
        status.success(),
        "expected rokr to exit cleanly after q, got status: {status:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn bash_tool_call_skips_execution_on_reject() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let final_reply_text = "FinalReplyAfterBashRejectForTesting";

    let temp_dir = unique_temp_dir("bash-reject-target");
    let marker_path = temp_dir.join("reject-marker");
    let marker_path_str = marker_path.to_string_lossy().into_owned();
    let bash_command = format!("touch {marker_path_str}");

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-bash-reject",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "tool_calls": [
                            {
                                "id": "call_1",
                                "type": "function",
                                "function": {
                                    "name": "bash",
                                    "arguments": serde_json::json!({ "command": bash_command }).to_string()
                                }
                            }
                        ]
                    },
                    "finish_reason": "tool_calls"
                }
            ]
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // The loop must continue after a rejection: the model gets a
    // rejection-flavored tool result and still produces a final reply.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-bash-reject-final",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": final_reply_text
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-bash-reject");
    let xdg_config_home = unique_temp_dir("xdg-config-home-bash-reject");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");
    cmd.arg("--agent");
    cmd.arg("build");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("failed to spawn rokr in pty");
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone pty reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = pair
        .master
        .take_writer()
        .expect("failed to take pty writer");

    let mut output = String::new();
    let render_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < render_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Header") && output.contains("View") && output.contains("Prompt") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("Header"),
        "expected pty output to contain Header, got: {output:?}"
    );

    writer
        .write_all(b"runbash\r")
        .expect("failed to write prompt to pty");

    let prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("bash") && output.contains("reject-marker") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("bash"),
        "expected pty output to contain the tool name 'bash' in a permission prompt, got: {output:?}"
    );
    assert!(
        output.contains("reject-marker"),
        "expected pty output to contain the command in a permission prompt, got: {output:?}"
    );

    writer
        .write_all(b"n")
        .expect("failed to write reject keypress to pty");

    let response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(final_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains(final_reply_text),
        "expected pty output to contain the final assistant reply after rejecting the bash \
         tool call (the loop must continue, not crash), got: {output:?}"
    );
    assert!(
        output.to_lowercase().contains("den") || output.to_lowercase().contains("reject"),
        "expected pty output to contain a rejection-related result, got: {output:?}"
    );
    assert!(
        !marker_path.exists(),
        "the bash command must never run after permission was rejected"
    );

    writer.write_all(b"q").expect("failed to write q to pty");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
            break status;
        }
        if Instant::now() > exit_deadline {
            let _ = child.kill();
            panic!("rokr did not exit within timeout after pressing q; output so far: {output:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };

    assert!(
        status.success(),
        "expected rokr to exit cleanly after q, got status: {status:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn plan_agent_bash_tool_call_yields_unavailable_tool_result_without_prompt() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let final_reply_text = "FinalReplyAfterPlanTierBashUnavailableForTesting";

    let temp_dir = unique_temp_dir("plan-bash-unavailable-target");
    let marker_path = temp_dir.join("plan-bash-marker");
    let marker_path_str = marker_path.to_string_lossy().into_owned();
    let bash_command = format!("touch {marker_path_str}");

    // First call: the model asks to invoke the `bash` tool, which is not
    // part of the default Plan tier's tool set.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-plan-bash-unavailable",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "tool_calls": [
                            {
                                "id": "call_1",
                                "type": "function",
                                "function": {
                                    "name": "bash",
                                    "arguments": serde_json::json!({ "command": bash_command }).to_string()
                                }
                            }
                        ]
                    },
                    "finish_reason": "tool_calls"
                }
            ]
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second call: the loop feeds the "unknown tool" result back, and the
    // model replies with a final, tool-call-free text answer -- no
    // permission prompt should ever have been shown in between.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-plan-bash-unavailable-final",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": final_reply_text
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-plan-bash-unavailable");
    let xdg_config_home = unique_temp_dir("xdg-config-home-plan-bash-unavailable");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    // Deliberately no `--agent` flag: exercises the new default `plan` tier.
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("failed to spawn rokr in pty");
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone pty reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = pair
        .master
        .take_writer()
        .expect("failed to take pty writer");

    let mut output = String::new();
    let render_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < render_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Header") && output.contains("View") && output.contains("Prompt") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("Header"),
        "expected pty output to contain Header, got: {output:?}"
    );

    writer
        .write_all(b"try running bash\r")
        .expect("failed to write prompt to pty");

    // No permission prompt should ever appear: the loop should sail straight
    // through the unknown-tool result to the final reply without pausing for
    // a y/n keypress. Poll directly for the final reply text.
    let response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(final_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains(final_reply_text),
        "expected pty output to contain the final assistant reply after the bash tool call was \
         reported unavailable, got: {output:?}"
    );
    assert!(
        !output.contains("permission needed"),
        "expected no permission prompt to ever be shown for a tool unavailable in the Plan tier, \
         got: {output:?}"
    );
    assert!(
        !marker_path.exists(),
        "the bash command must never run under the Plan tier"
    );

    writer.write_all(b"q").expect("failed to write q to pty");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
            break status;
        }
        if Instant::now() > exit_deadline {
            let _ = child.kill();
            panic!("rokr did not exit within timeout after pressing q; output so far: {output:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };

    assert!(
        status.success(),
        "expected rokr to exit cleanly after q, got status: {status:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn write_tool_call_renders_diff_and_writes_file_on_accept() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let final_reply_text = "FinalReplyAfterWriteAcceptForTesting";

    let temp_dir = unique_temp_dir("write-accept-target");
    let target_file = temp_dir.join("target.txt");
    let old_content = "originalfilecontent";
    let new_content = "updatedfilecontent";
    std::fs::write(&target_file, old_content).unwrap();
    let target_path = target_file.to_string_lossy().into_owned();

    // First call: the model asks to invoke the `write` tool against the real
    // temp file, replacing its content.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-write-accept",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "tool_calls": [
                            {
                                "id": "call_1",
                                "type": "function",
                                "function": {
                                    "name": "write",
                                    "arguments": serde_json::json!({
                                        "path": target_path,
                                        "content": new_content
                                    }).to_string()
                                }
                            }
                        ]
                    },
                    "finish_reason": "tool_calls"
                }
            ]
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second call: the loop feeds the tool result back, and the model
    // replies with a final, tool-call-free text answer.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-write-accept-final",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": final_reply_text
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-write-accept");
    let xdg_config_home = unique_temp_dir("xdg-config-home-write-accept");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");
    cmd.arg("--agent");
    cmd.arg("build");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("failed to spawn rokr in pty");
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone pty reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = pair
        .master
        .take_writer()
        .expect("failed to take pty writer");

    let mut output = String::new();
    let render_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < render_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Header") && output.contains("View") && output.contains("Prompt") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("Header"),
        "expected pty output to contain Header, got: {output:?}"
    );

    writer
        .write_all(b"writethefile\r")
        .expect("failed to write prompt to pty");

    // Wait for the permission prompt to render the diff before we've
    // granted anything.
    let prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("write")
            && output.contains("-originalfilecontent")
            && output.contains("+updatedfilecontent")
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("write"),
        "expected pty output to contain the tool name 'write' in a permission prompt, got: {output:?}"
    );
    assert!(
        output.contains("-originalfilecontent"),
        "expected pty output to contain the old-content diff line, got: {output:?}"
    );
    assert!(
        output.contains("+updatedfilecontent"),
        "expected pty output to contain the new-content diff line, got: {output:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&target_file).unwrap(),
        old_content,
        "the file must not have been written before permission was granted"
    );

    writer
        .write_all(b"y")
        .expect("failed to write accept keypress to pty");

    let response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(final_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains(final_reply_text),
        "expected pty output to contain the final assistant reply after accepting the write \
         tool call, got: {output:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&target_file).unwrap(),
        new_content,
        "expected the file to have been written with the new content after accepting"
    );

    writer.write_all(b"q").expect("failed to write q to pty");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
            break status;
        }
        if Instant::now() > exit_deadline {
            let _ = child.kill();
            panic!("rokr did not exit within timeout after pressing q; output so far: {output:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };

    assert!(
        status.success(),
        "expected rokr to exit cleanly after q, got status: {status:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&temp_dir);
}

/// Extends `write_tool_call_renders_diff_and_writes_file_on_accept` to a
/// second gated tool: unlike `write`'s whole-file diff, `edit`'s diff-review
/// must render only the targeted replacement region, not the whole file. Does
/// not re-assert ticket 11's write-tool acceptance.
#[tokio::test]
async fn edit_tool_call_renders_partial_diff_and_applies_on_accept() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let final_reply_text = "FinalReplyAfterEditAcceptForTesting";

    let temp_dir = unique_temp_dir("edit-accept-target");
    let target_file = temp_dir.join("target.txt");
    let old_str = "targetline";
    let new_str = "replacedline";
    let unrelated_before = "unrelatedbeforeline";
    let unrelated_after = "unrelatedafterline";
    let original_content = format!("{unrelated_before}\n{old_str}\n{unrelated_after}\n");
    let expected_content = format!("{unrelated_before}\n{new_str}\n{unrelated_after}\n");
    std::fs::write(&target_file, &original_content).unwrap();
    let target_path = target_file.to_string_lossy().into_owned();

    // First call: the model asks to invoke the `edit` tool against the real
    // temp file, requesting a targeted replacement (not a whole-file
    // overwrite).
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-edit-accept",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "tool_calls": [
                            {
                                "id": "call_1",
                                "type": "function",
                                "function": {
                                    "name": "edit",
                                    "arguments": serde_json::json!({
                                        "path": target_path,
                                        "old_str": old_str,
                                        "new_str": new_str
                                    }).to_string()
                                }
                            }
                        ]
                    },
                    "finish_reason": "tool_calls"
                }
            ]
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second call: the loop feeds the tool result back, and the model
    // replies with a final, tool-call-free text answer.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-edit-accept-final",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": final_reply_text
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-edit-accept");
    let xdg_config_home = unique_temp_dir("xdg-config-home-edit-accept");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");
    cmd.arg("--agent");
    cmd.arg("build");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("failed to spawn rokr in pty");
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone pty reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = pair
        .master
        .take_writer()
        .expect("failed to take pty writer");

    let mut output = String::new();
    let render_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < render_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Header") && output.contains("View") && output.contains("Prompt") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("Header"),
        "expected pty output to contain Header, got: {output:?}"
    );

    writer
        .write_all(b"editthefile\r")
        .expect("failed to write prompt to pty");

    // Wait for the permission prompt to render the partial diff before we've
    // granted anything.
    let prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("edit")
            && output.contains("-targetline")
            && output.contains("+replacedline")
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("edit"),
        "expected pty output to contain the tool name 'edit' in a permission prompt, got: {output:?}"
    );
    assert!(
        output.contains("-targetline"),
        "expected pty output to contain the targeted old-snippet diff line, got: {output:?}"
    );
    assert!(
        output.contains("+replacedline"),
        "expected pty output to contain the targeted new-snippet diff line, got: {output:?}"
    );
    assert!(
        !output.contains("unrelatedbeforeline") && !output.contains("unrelatedafterline"),
        "expected the diff-review to render only the targeted region, not unrelated file \
         lines, got: {output:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&target_file).unwrap(),
        original_content,
        "the file must not have been edited before permission was granted"
    );

    writer
        .write_all(b"y")
        .expect("failed to write accept keypress to pty");

    let response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(final_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains(final_reply_text),
        "expected pty output to contain the final assistant reply after accepting the edit \
         tool call, got: {output:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&target_file).unwrap(),
        expected_content,
        "expected exactly the targeted replacement to have landed on disk, with unrelated \
         lines untouched"
    );

    writer.write_all(b"q").expect("failed to write q to pty");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
            break status;
        }
        if Instant::now() > exit_deadline {
            let _ = child.kill();
            panic!("rokr did not exit within timeout after pressing q; output so far: {output:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };

    assert!(
        status.success(),
        "expected rokr to exit cleanly after q, got status: {status:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn typed_prompt_renders_provider_error_in_view() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("mock server error"))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-error");
    let xdg_config_home = unique_temp_dir("xdg-config-home-error");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("failed to spawn rokr in pty");
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone pty reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = pair
        .master
        .take_writer()
        .expect("failed to take pty writer");

    let mut output = String::new();
    let render_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < render_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Header") && output.contains("View") && output.contains("Prompt") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("Header"),
        "expected pty output to contain Header, got: {output:?}"
    );

    writer
        .write_all(b"helloworld\r")
        .expect("failed to write prompt to pty");

    let response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Error:") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("helloworld"),
        "expected pty output to contain the typed prompt, got: {output:?}"
    );
    assert!(
        output.contains("Error:"),
        "expected pty output to contain the rendered provider error, got: {output:?}"
    );

    writer.write_all(b"q").expect("failed to write q to pty");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
            break status;
        }
        if Instant::now() > exit_deadline {
            let _ = child.kill();
            panic!("rokr did not exit within timeout after pressing q; output so far: {output:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };

    assert!(
        status.success(),
        "expected rokr to exit cleanly after q, got status: {status:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

#[tokio::test]
async fn agents_md_content_appears_in_outgoing_system_prompt() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    // Single token (no spaces): ratatui only redraws cells that actually
    // changed, so a multi-word phrase's raw ANSI byte stream can have
    // cursor-jump gaps where a space cell was already blank and thus never
    // rewritten — a literal substring match on the raw pty bytes would then
    // spuriously fail even though the rendered screen is correct.
    let canned_response = "MockedAssistantReplyForAgentsMdTesting";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-agents-md",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": canned_response
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-agents-md");
    let xdg_config_home = unique_temp_dir("xdg-config-home-agents-md");
    let project_dir = unique_temp_dir("agents-md-project");
    let agents_md_content = "DistinctiveAgentsMdContentForSystemPromptTesting";
    std::fs::write(project_dir.join("AGENTS.md"), agents_md_content).unwrap();

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");
    cmd.cwd(&project_dir);

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("failed to spawn rokr in pty");
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone pty reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = pair
        .master
        .take_writer()
        .expect("failed to take pty writer");

    let mut output = String::new();
    let render_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < render_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Header") && output.contains("View") && output.contains("Prompt") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("Header"),
        "expected pty output to contain Header, got: {output:?}"
    );

    writer
        .write_all(b"helloworld\r")
        .expect("failed to write prompt to pty");

    let response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(canned_response) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("helloworld"),
        "expected pty output to contain the typed prompt, got: {output:?}"
    );
    assert!(
        output.contains(canned_response),
        "expected pty output to contain the mocked assistant response, got: {output:?}"
    );

    writer.write_all(b"q").expect("failed to write q to pty");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
            break status;
        }
        if Instant::now() > exit_deadline {
            let _ = child.kill();
            panic!("rokr did not exit within timeout after pressing q; output so far: {output:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };

    assert!(
        status.success(),
        "expected rokr to exit cleanly after q, got status: {status:?}"
    );

    let received_requests = mock_server
        .received_requests()
        .await
        .expect("request recording should be enabled on the mock server by default");

    assert!(
        !received_requests.is_empty(),
        "expected at least 1 request to /chat/completions, got 0"
    );

    let first_request_body = String::from_utf8_lossy(&received_requests[0].body).into_owned();

    assert!(
        first_request_body.contains(agents_md_content),
        "expected the outgoing request body to contain the AGENTS.md content, proving the \
         project's AGENTS.md was loaded into the system prompt, got: {first_request_body}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&project_dir);
}

/// Ticket 18 (repo-map-generation) acceptance test: with cwd set to a temp
/// project containing a tracked file and a `.gitignore`-excluded file,
/// submitting a prompt sends a request whose repo-map context segment lists
/// the tracked file and omits the gitignored one. Copies
/// `agents_md_content_appears_in_outgoing_system_prompt`'s exact
/// cwd/PTY/wiremock harness.
#[tokio::test]
async fn repo_map_segment_lists_tracked_file_and_omits_gitignored_file() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    // Single token (no spaces) for the same reason as the other tests:
    // ratatui only redraws changed cells, so a literal substring match on
    // raw pty bytes needs to avoid spaces that might not get rewritten.
    let canned_response = "MockedAssistantReplyForRepoMapTesting";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-repo-map",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": canned_response
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-repo-map");
    let xdg_config_home = unique_temp_dir("xdg-config-home-repo-map");
    let project_dir = unique_temp_dir("repo-map-project");
    std::fs::create_dir_all(project_dir.join("src")).unwrap();
    std::fs::write(project_dir.join("src/lib.rs"), "pub fn tracked() {}").unwrap();
    std::fs::write(project_dir.join(".gitignore"), "secret.txt\n").unwrap();
    std::fs::write(project_dir.join("secret.txt"), "top secret").unwrap();

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");
    cmd.cwd(&project_dir);

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("failed to spawn rokr in pty");
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone pty reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = pair
        .master
        .take_writer()
        .expect("failed to take pty writer");

    let mut output = String::new();
    let render_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < render_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Header") && output.contains("View") && output.contains("Prompt") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("Header"),
        "expected pty output to contain Header, got: {output:?}"
    );

    writer
        .write_all(b"helloworld\r")
        .expect("failed to write prompt to pty");

    let response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(canned_response) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("helloworld"),
        "expected pty output to contain the typed prompt, got: {output:?}"
    );
    assert!(
        output.contains(canned_response),
        "expected pty output to contain the mocked assistant response, got: {output:?}"
    );

    writer.write_all(b"q").expect("failed to write q to pty");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
            break status;
        }
        if Instant::now() > exit_deadline {
            let _ = child.kill();
            panic!("rokr did not exit within timeout after pressing q; output so far: {output:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };

    assert!(
        status.success(),
        "expected rokr to exit cleanly after q, got status: {status:?}"
    );

    let received_requests = mock_server
        .received_requests()
        .await
        .expect("request recording should be enabled on the mock server by default");

    assert!(
        !received_requests.is_empty(),
        "expected at least 1 request to /chat/completions, got 0"
    );

    let first_request_body = String::from_utf8_lossy(&received_requests[0].body).into_owned();

    assert!(
        first_request_body.contains("lib.rs"),
        "expected the outgoing request body's repo-map segment to list the tracked file, \
         got: {first_request_body}"
    );
    assert!(
        !first_request_body.contains("secret.txt"),
        "expected the outgoing request body to never mention the gitignored file, \
         got: {first_request_body}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&project_dir);
}

/// Ticket 19 (at-mention-file-injection) acceptance test: an `@<path>` token
/// typed in the prompt whose path does not resolve to a real file must
/// still submit successfully — the outgoing request's user message gets a
/// "file not found" note referencing the path instead of failing the
/// submission — and no orphan tool-role message is ever introduced by
/// mention handling (mentions are expanded inline into the user turn, never
/// as a synthetic tool result).
#[tokio::test]
async fn at_mention_for_missing_file_still_submits_with_not_found_note() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    // Single token (no spaces) for the same reason as the other tests:
    // ratatui only redraws changed cells, so a literal substring match on
    // raw pty bytes needs to avoid spaces that might not get rewritten.
    let canned_response = "ReplyAfterMissingMentionForTesting";

    // The provider always replies with plain text — no tool call — so this
    // test proves mention handling alone, not the tool loop.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-mention-missing",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": canned_response
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-mention-missing");
    let xdg_config_home = unique_temp_dir("xdg-config-home-mention-missing");
    // `unique_temp_dir` creates this directory, but the file referenced
    // inside it is never written, so the mention is guaranteed to be
    // path-shaped (it has an extension) yet unresolvable.
    let mention_dir = unique_temp_dir("mention-missing");
    let missing_file = mention_dir.join("does-not-exist.txt");
    let missing_file_str = missing_file.to_string_lossy().into_owned();

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("failed to spawn rokr in pty");
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone pty reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = pair
        .master
        .take_writer()
        .expect("failed to take pty writer");

    let mut output = String::new();
    let render_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < render_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Header") && output.contains("View") && output.contains("Prompt") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("Header"),
        "expected pty output to contain Header, got: {output:?}"
    );

    let typed_prompt = format!("@{missing_file_str}\r");
    writer
        .write_all(typed_prompt.as_bytes())
        .expect("failed to write prompt to pty");

    let response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(canned_response) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains(canned_response),
        "expected pty output to contain the mocked assistant response, proving submission \
         succeeded despite the missing mentioned file, got: {output:?}"
    );

    writer.write_all(b"q").expect("failed to write q to pty");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
            break status;
        }
        if Instant::now() > exit_deadline {
            let _ = child.kill();
            panic!("rokr did not exit within timeout after pressing q; output so far: {output:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };

    assert!(
        status.success(),
        "expected rokr to exit cleanly after q, got status: {status:?}"
    );

    let received_requests = mock_server
        .received_requests()
        .await
        .expect("request recording should be enabled on the mock server by default");

    assert!(
        !received_requests.is_empty(),
        "expected at least 1 request to /chat/completions, got 0"
    );

    let first_request_body = String::from_utf8_lossy(&received_requests[0].body).into_owned();

    assert!(
        first_request_body.contains(&missing_file_str),
        "expected the outgoing request body to reference the missing mentioned path, \
         got: {first_request_body}"
    );
    assert!(
        first_request_body.to_lowercase().contains("not found"),
        "expected the outgoing request body to contain a 'not found' note for the missing \
         mentioned file, got: {first_request_body}"
    );
    assert!(
        !first_request_body.contains("\"role\":\"tool\""),
        "expected no orphan tool-role message to be introduced by mention handling, \
         got: {first_request_body}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&mention_dir);
}

/// Ticket 20 (auto-compaction-threshold) acceptance test: once a turn's
/// reported usage crosses `auto_compact_threshold * context_window_size`
/// (default 0.7 * 200_000 = 140_000), the very next submit must trigger one
/// extra summarization call to the same provider before the next turn's
/// request goes out, and that next request must carry the compacted
/// transcript — the summary in place of the compacted-away middle turn, with
/// the most recent turn preserved verbatim. Copies
/// `second_prompt_includes_prior_turn_in_request_body`'s exact PTY/wiremock
/// harness, extended to four mocked responses.
#[tokio::test]
async fn auto_compaction_triggers_once_usage_crosses_threshold_and_preserves_recent_turn() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let first_reply_text = "FirstReplyTokenForCompaction";
    let second_reply_text = "SecondReplyTokenForCompaction";
    let compaction_summary_text = "CompactionSummaryTokenForTesting";
    let third_reply_text = "ThirdReplyTokenForCompaction";

    // Turn 1: low usage, well under the compaction threshold.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-first",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": first_reply_text
                    },
                    "finish_reason": "stop"
                }
            ],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150
            }
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Turn 2: usage crosses the default 140_000-token compaction budget
    // (0.7 * 200_000), triggering compaction right after this turn
    // completes and before the third prompt's request goes out.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-second",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": second_reply_text
                    },
                    "finish_reason": "stop"
                }
            ],
            "usage": {
                "prompt_tokens": 150000,
                "completion_tokens": 5000,
                "total_tokens": 155000
            }
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // The compaction summarization call, auto-triggered right after turn 2.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-compaction",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": compaction_summary_text
                    },
                    "finish_reason": "stop"
                }
            ],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150
            }
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Turn 3: catch-all for everything after the compaction call.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-third",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": third_reply_text
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-auto-compaction");
    let xdg_config_home = unique_temp_dir("xdg-config-home-auto-compaction");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("failed to spawn rokr in pty");
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone pty reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = pair
        .master
        .take_writer()
        .expect("failed to take pty writer");

    let mut output = String::new();
    let render_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < render_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Header") && output.contains("View") && output.contains("Prompt") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("Header"),
        "expected pty output to contain Header, got: {output:?}"
    );

    writer
        .write_all(b"firstpromptunique\r")
        .expect("failed to write first prompt to pty");

    let first_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < first_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(first_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains(first_reply_text),
        "expected pty output to contain the first mocked assistant response, got: {output:?}"
    );

    writer
        .write_all(b"secondpromptunique\r")
        .expect("failed to write second prompt to pty");

    // By the time this text renders, the whole submit future — including
    // the auto-triggered compaction call — has already completed, since
    // compaction happens synchronously before the reply text is returned
    // from the submit closure.
    let second_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < second_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(second_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains(second_reply_text),
        "expected pty output to contain the second mocked assistant response, got: {output:?}"
    );

    writer
        .write_all(b"thirdpromptunique\r")
        .expect("failed to write third prompt to pty");

    let third_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < third_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(third_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains(third_reply_text),
        "expected pty output to contain the third mocked assistant response, got: {output:?}"
    );

    writer.write_all(b"q").expect("failed to write q to pty");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
            break status;
        }
        if Instant::now() > exit_deadline {
            let _ = child.kill();
            panic!("rokr did not exit within timeout after pressing q; output so far: {output:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };

    assert!(
        status.success(),
        "expected rokr to exit cleanly after q, got status: {status:?}"
    );

    let received_requests = mock_server
        .received_requests()
        .await
        .expect("request recording should be enabled on the mock server by default");

    assert!(
        received_requests.len() >= 4,
        "expected at least 4 requests to /chat/completions, got {}: {received_requests:?}",
        received_requests.len()
    );

    let turn1_body = String::from_utf8_lossy(&received_requests[0].body).into_owned();
    let turn2_body = String::from_utf8_lossy(&received_requests[1].body).into_owned();
    let compaction_body = String::from_utf8_lossy(&received_requests[2].body).into_owned();
    let turn3_body = String::from_utf8_lossy(&received_requests[3].body).into_owned();

    assert!(
        turn1_body.contains("firstpromptunique"),
        "expected turn 1's request body to contain the first typed prompt, got: {turn1_body}"
    );

    assert!(
        turn2_body.contains("secondpromptunique"),
        "expected turn 2's request body to contain the second typed prompt, got: {turn2_body}"
    );
    assert!(
        turn2_body.contains("firstpromptunique"),
        "expected turn 2's request body to still contain the first (prior) turn, got: {turn2_body}"
    );

    assert!(
        compaction_body.contains("firstpromptunique"),
        "expected the compaction call's body to summarize the first turn's prompt, \
         got: {compaction_body}"
    );
    assert!(
        compaction_body.contains(first_reply_text),
        "expected the compaction call's body to summarize the first turn's reply, \
         got: {compaction_body}"
    );
    assert!(
        !compaction_body.contains("thirdpromptunique"),
        "the compaction call happens before the third prompt is even typed, so it must not \
         contain it, got: {compaction_body}"
    );

    assert!(
        !turn3_body.contains("firstpromptunique"),
        "expected turn 3's request body to have the compacted-away first turn's prompt removed, \
         got: {turn3_body}"
    );
    assert!(
        !turn3_body.contains(first_reply_text),
        "expected turn 3's request body to have the compacted-away first turn's reply removed, \
         got: {turn3_body}"
    );
    assert!(
        turn3_body.contains(compaction_summary_text),
        "expected turn 3's request body to fold in the compaction summary, got: {turn3_body}"
    );
    assert!(
        turn3_body.contains("secondpromptunique") && turn3_body.contains(second_reply_text),
        "expected turn 3's request body to still contain the most-recent (second) turn \
         verbatim, got: {turn3_body}"
    );

    let turn2_parsed: serde_json::Value =
        serde_json::from_str(&turn2_body).expect("turn 2 request body should be valid JSON");
    let turn3_parsed: serde_json::Value =
        serde_json::from_str(&turn3_body).expect("turn 3 request body should be valid JSON");

    let turn2_static_prefix = (
        turn2_parsed
            .get("tools")
            .cloned()
            .expect("turn 2 request body should have a tools array"),
        turn2_parsed["messages"]
            .get(0)
            .cloned()
            .expect("turn 2 request body should have at least one message"),
    );
    let turn3_static_prefix = (
        turn3_parsed
            .get("tools")
            .cloned()
            .expect("turn 3 request body should have a tools array"),
        turn3_parsed["messages"]
            .get(0)
            .cloned()
            .expect("turn 3 request body should have at least one message"),
    );

    let turn2_static_prefix_bytes =
        serde_json::to_vec(&turn2_static_prefix).expect("serialize turn 2 static prefix");
    let turn3_static_prefix_bytes =
        serde_json::to_vec(&turn3_static_prefix).expect("serialize turn 3 static prefix");

    assert_eq!(
        turn2_static_prefix_bytes, turn3_static_prefix_bytes,
        "the static prefix (tool specs + system segment) must stay byte-identical across \
         requests even when compaction rewrites the transcript; turn2: {turn2_static_prefix:?}, \
         turn3: {turn3_static_prefix:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

/// Ticket 20 (auto-compaction-threshold) acceptance test: if the compaction
/// summarization call itself fails (provider error), the transcript must be
/// left completely untouched — the next request must still carry the full,
/// uncompacted history — and a notice must be surfaced to the user rather
/// than silently losing history or crashing the session.
#[tokio::test]
async fn auto_compaction_failure_leaves_transcript_intact_and_shows_notice() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let first_reply_text = "FirstReplyTokenForCompaction";
    let second_reply_text = "SecondReplyTokenForCompaction";
    let third_reply_text = "ThirdReplyAfterFailedCompaction";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-first",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": first_reply_text
                    },
                    "finish_reason": "stop"
                }
            ],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150
            }
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-second",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": second_reply_text
                    },
                    "finish_reason": "stop"
                }
            ],
            "usage": {
                "prompt_tokens": 150000,
                "completion_tokens": 5000,
                "total_tokens": 155000
            }
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // The compaction call fails outright.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-third",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": third_reply_text
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-auto-compaction-failure");
    let xdg_config_home = unique_temp_dir("xdg-config-home-auto-compaction-failure");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("failed to spawn rokr in pty");
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone pty reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = pair
        .master
        .take_writer()
        .expect("failed to take pty writer");

    let mut output = String::new();
    let render_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < render_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Header") && output.contains("View") && output.contains("Prompt") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("Header"),
        "expected pty output to contain Header, got: {output:?}"
    );

    writer
        .write_all(b"firstpromptunique\r")
        .expect("failed to write first prompt to pty");

    let first_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < first_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(first_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains(first_reply_text),
        "expected pty output to contain the first mocked assistant response, got: {output:?}"
    );

    writer
        .write_all(b"secondpromptunique\r")
        .expect("failed to write second prompt to pty");

    // The compaction attempt fails, but the submit future must still
    // complete and surface a notice alongside the second turn's own reply.
    let second_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < second_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(second_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains(second_reply_text),
        "expected pty output to contain the second mocked assistant response, got: {output:?}"
    );

    writer
        .write_all(b"thirdpromptunique\r")
        .expect("failed to write third prompt to pty");

    let third_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < third_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(third_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains(third_reply_text),
        "expected pty output to contain the third mocked assistant response, got: {output:?}"
    );

    writer.write_all(b"q").expect("failed to write q to pty");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
            break status;
        }
        if Instant::now() > exit_deadline {
            let _ = child.kill();
            panic!("rokr did not exit within timeout after pressing q; output so far: {output:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };

    assert!(
        status.success(),
        "expected rokr to exit cleanly after q, got status: {status:?}"
    );

    let received_requests = mock_server
        .received_requests()
        .await
        .expect("request recording should be enabled on the mock server by default");

    assert!(
        received_requests.len() >= 4,
        "expected at least 4 requests to /chat/completions (including the failed compaction \
         attempt), got {}: {received_requests:?}",
        received_requests.len()
    );

    let turn3_body = String::from_utf8_lossy(&received_requests[3].body).into_owned();

    assert!(
        turn3_body.contains("firstpromptunique"),
        "expected turn 3's request body to still contain the first turn's prompt, since \
         compaction failed and must have left the transcript untouched, got: {turn3_body}"
    );
    assert!(
        turn3_body.contains(first_reply_text),
        "expected turn 3's request body to still contain the first turn's reply, since \
         compaction failed and must have left the transcript untouched, got: {turn3_body}"
    );

    // Checked as separate substrings rather than one contiguous phrase:
    // ratatui word-wraps rendered text by placing each word at its own
    // explicit cursor position, so "auto-compaction" and "failed," land at
    // different escape-sequence-separated positions in the raw pty byte
    // stream even though they render adjacently on screen.
    assert!(
        output.contains("auto-compaction") && output.contains("failed,"),
        "expected the pty output to surface a notice about the failed compaction attempt, \
         got: {output:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

/// F-001 acceptance test: the repo map must regenerate on `/compact` (and
/// only on `/compact`, never per turn). Starts rokr in a temp project with
/// `src/lib.rs`, submits a prompt, then writes a NEW `src/new_module.rs` to
/// disk. A second pre-compact prompt is submitted (its request body must
/// still lack the new file, proving the map is NOT regenerated per turn),
/// then `/compact` runs an actual compaction (which requires >= 2 prior user
/// turns — a single turn would no-op, so nothing would regenerate), and a
/// final post-compact prompt's request body must now list `new_module.rs`,
/// proving the map regenerated on `/compact`.
///
/// Sequence of real `/chat/completions` requests: 1st = first prompt, 2nd =
/// second (pre-compact) prompt, 3rd = the compaction call itself, 4th = the
/// post-compact prompt.
#[tokio::test]
async fn repo_map_regenerates_on_compact_to_pick_up_new_file() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let first_reply_text = "FirstReplyBeforeCompactForRepoMapRegen";
    let second_reply_text = "SecondReplyBeforeCompactForRepoMapRegen";
    let compaction_summary_token = "CompactionSummaryTokenForRepoMapRegen";
    let post_compact_reply_text = "ThirdReplyAfterCompactForRepoMapRegen";

    // 1st request: reply to the first pre-compact prompt.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-first",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": first_reply_text },
                    "finish_reason": "stop"
                }
            ]
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // 2nd request: reply to the second pre-compact prompt (gives compaction
    // a genuine prefix to summarize — one turn alone would no-op).
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-second",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": second_reply_text },
                    "finish_reason": "stop"
                }
            ]
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // 3rd request: the compaction call's own summarization reply.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-compaction",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": compaction_summary_token },
                    "finish_reason": "stop"
                }
            ]
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // 4th+ request(s): catch-all reply to the post-compact prompt.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-post-compact",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": post_compact_reply_text },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-repo-map-regen");
    let xdg_config_home = unique_temp_dir("xdg-config-home-repo-map-regen");
    let project_dir = unique_temp_dir("repo-map-regen-project");
    std::fs::create_dir_all(project_dir.join("src")).unwrap();
    std::fs::write(project_dir.join("src/lib.rs"), "pub fn tracked() {}").unwrap();

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");
    cmd.cwd(&project_dir);

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("failed to spawn rokr in pty");
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone pty reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = pair
        .master
        .take_writer()
        .expect("failed to take pty writer");

    let mut output = String::new();
    let render_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < render_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Header") && output.contains("View") && output.contains("Prompt") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("Header"),
        "expected pty output to contain Header, got: {output:?}"
    );

    writer
        .write_all(b"firstpromptunique\r")
        .expect("failed to write first prompt to pty");

    let first_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < first_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(first_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(first_reply_text),
        "expected pty output to contain the first mocked assistant response, got: {output:?}"
    );

    // Write the new file AFTER the first turn's request has gone out. The
    // repo map was generated once at startup (only `lib.rs`), so neither
    // pre-compact request should list this file — proving the map is not
    // regenerated per turn.
    std::fs::write(
        project_dir.join("src/new_module.rs"),
        "pub fn added_later() {}",
    )
    .unwrap();

    writer
        .write_all(b"secondpromptbeforecompact\r")
        .expect("failed to write second prompt to pty");

    let second_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < second_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(second_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(second_reply_text),
        "expected pty output to contain the second mocked assistant response, got: {output:?}"
    );

    writer
        .write_all(b"/compact\r")
        .expect("failed to write /compact to pty");

    let compact_confirmation_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < compact_confirmation_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.to_lowercase().contains("compact") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.to_lowercase().contains("compact"),
        "expected pty output to contain a compaction confirmation after /compact, got: {output:?}"
    );

    writer
        .write_all(b"thirdpromptunique\r")
        .expect("failed to write third prompt to pty");

    let third_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < third_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(post_compact_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(post_compact_reply_text),
        "expected pty output to contain the post-compact mocked assistant response, got: {output:?}"
    );

    writer.write_all(b"q").expect("failed to write q to pty");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
            break status;
        }
        if Instant::now() > exit_deadline {
            let _ = child.kill();
            panic!("rokr did not exit within timeout after pressing q; output so far: {output:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };
    assert!(
        status.success(),
        "expected rokr to exit cleanly after q, got status: {status:?}"
    );

    let received_requests = mock_server
        .received_requests()
        .await
        .expect("request recording should be enabled on the mock server by default");

    assert!(
        received_requests.len() >= 4,
        "expected at least 4 requests to /chat/completions (2 pre-compact prompts + the \
         compaction call + the post-compact prompt), got {}: {received_requests:?}",
        received_requests.len()
    );

    let first_request_body = String::from_utf8_lossy(&received_requests[0].body).into_owned();
    let second_request_body = String::from_utf8_lossy(&received_requests[1].body).into_owned();
    let post_compact_request_body =
        String::from_utf8_lossy(&received_requests[3].body).into_owned();

    assert!(
        !first_request_body.contains("new_module.rs"),
        "the first (pre-compact) prompt's repo map must not list the not-yet-created file, \
         got: {first_request_body}"
    );
    assert!(
        !second_request_body.contains("new_module.rs"),
        "the second (pre-compact) prompt's repo map must still not list the new file, proving \
         the map is not regenerated per turn, got: {second_request_body}"
    );
    assert!(
        post_compact_request_body.contains("new_module.rs"),
        "the post-compact prompt's repo map must list new_module.rs, proving the map \
         regenerated on /compact, got: {post_compact_request_body}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&project_dir);
}
