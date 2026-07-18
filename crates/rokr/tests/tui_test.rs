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
