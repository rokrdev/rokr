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

/// Ticket 52 (clap-and-sessionrunner-extraction) acceptance test: a full
/// PTY session round trip -- render, type a prompt, get the model's reply
/// back in the View -- must behave identically now that the `submit`
/// closure's orchestration is driven by `rokr_app::SessionRunner` instead of
/// being inlined in `main.rs`. This is the regression guard that the
/// SessionRunner extraction was a pure move: the observable submit-path
/// behavior end-to-end through the real running binary is unchanged.
#[tokio::test]
async fn tui_session_round_trip_unchanged_after_sessionrunner_extraction() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    // Single token (no spaces) so a literal substring match on the raw pty
    // byte stream is robust to ratatui's changed-cell-only redraws.
    let canned_response = "SessionRunnerRoundTripReply";

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

    let home = unique_temp_dir("home-runner-round-trip");
    let xdg_config_home = unique_temp_dir("xdg-config-home-runner-round-trip");

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
        .write_all(b"roundtripprompt\r")
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
        output.contains("roundtripprompt"),
        "expected pty output to contain the typed prompt, got: {output:?}"
    );
    assert!(
        output.contains(canned_response),
        "expected the SessionRunner-driven submit path to render the mocked assistant \
         response in the View, got: {output:?}"
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

/// Ticket 34 (persist-new-sessions) acceptance test: submitting a prompt
/// must persist a `Header` record and a `Turn` record (containing the exact
/// prompt text) to a `session.jsonl` file under a ULID-named directory in
/// `XDG_DATA_HOME/rokr/sessions/`. RED phase: `main.rs` has not been wired
/// to construct a `SessionStore` or write any records yet, so this is
/// expected to fail (no `sessions/` directory will even exist).
#[tokio::test]
async fn submitting_a_prompt_persists_header_and_turn_records_to_session_jsonl() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    // Single token (no spaces) for the same reason as the other tests in
    // this file: ratatui only redraws changed cells, so a literal substring
    // match on raw pty bytes needs to avoid spaces that might not get
    // rewritten.
    let canned_response = "MockedAssistantReplyForSessionPersistenceTesting";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-persist",
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

    let home = unique_temp_dir("home-persist");
    let xdg_config_home = unique_temp_dir("xdg-config-home-persist");
    let xdg_data_home = unique_temp_dir("xdg-data-home-persist");

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
    cmd.env("XDG_DATA_HOME", &xdg_data_home);
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
        .write_all(b"persisttestprompt\r")
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
        output.contains("persisttestprompt"),
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

    let sessions_dir = xdg_data_home.join("rokr").join("sessions");
    let session_dir_entries: Vec<std::fs::DirEntry> = std::fs::read_dir(&sessions_dir)
        .unwrap_or_else(|err| {
            panic!("expected sessions directory to exist at {sessions_dir:?}, got error: {err:?}")
        })
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .collect();

    assert_eq!(
        session_dir_entries.len(),
        1,
        "expected exactly one ULID-named session directory under {sessions_dir:?}, got: {:?}",
        session_dir_entries
            .iter()
            .map(|entry| entry.path())
            .collect::<Vec<_>>()
    );

    let session_dir = session_dir_entries[0].path();
    let session_jsonl_path = session_dir.join("session.jsonl");
    assert!(
        session_jsonl_path.exists(),
        "expected session.jsonl to exist at {session_jsonl_path:?}"
    );

    let session_jsonl_contents = std::fs::read_to_string(&session_jsonl_path)
        .expect("failed to read session.jsonl contents");
    let lines: Vec<&str> = session_jsonl_contents
        .lines()
        .filter(|line| !line.is_empty())
        .collect();

    assert!(
        lines.len() >= 2,
        "expected at least 2 non-empty lines in session.jsonl, got {}: {session_jsonl_contents:?}",
        lines.len()
    );

    let header_record: rokr_session::SessionRecord = serde_json::from_str(lines[0])
        .expect("failed to parse first session.jsonl line as a SessionRecord");
    assert!(
        matches!(header_record, rokr_session::SessionRecord::Header { .. }),
        "expected the first session.jsonl record to be a Header record, got: {header_record:?}"
    );

    let turn_record: rokr_session::SessionRecord = serde_json::from_str(lines[1])
        .expect("failed to parse second session.jsonl line as a SessionRecord");
    match turn_record {
        rokr_session::SessionRecord::Turn { messages, .. } => {
            // Schema v2 (architect ruling, phase-5): a Turn now carries the
            // whole exchange -- the user prompt is the FIRST message, followed
            // by the assistant reply. Assert the first message is the prompt.
            assert_eq!(
                messages[0].text(),
                "persisttestprompt",
                "expected the persisted Turn record's first message text to exactly match the \
                 submitted prompt"
            );
        }
        other => {
            panic!("expected the second session.jsonl record to be a Turn record, got: {other:?}")
        }
    }

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&xdg_data_home);
}

/// Ticket 35 (resume-session) acceptance test: `rokr --resume <id>` against
/// a hand-built fixture session directory must fold that session's prior
/// `Header`/`Turn` records into the running transcript BEFORE the first new
/// prompt is accepted, so the very next outgoing request (there's no
/// `/compact` in this test, so it's the first and only request) carries the
/// prior turn's content alongside the newly typed prompt. Mirrors
/// `submitting_a_prompt_persists_header_and_turn_records_to_session_jsonl`'s
/// exact harness pattern (PTY spawn, wiremock mock server, `unique_temp_dir`
/// for HOME/XDG_CONFIG_HOME/XDG_DATA_HOME, ROKR_OPENAI_* env vars), differing
/// only in: (1) pre-seeding a fixture `session.jsonl` under
/// `XDG_DATA_HOME/rokr/sessions/<fixture_id>/` before spawning, (2) passing
/// `--resume <fixture_id>`, and (3) inspecting the captured request body
/// (via `mock_server.received_requests()`) instead of the persisted log.
///
/// Note: the repo map is always regenerated fresh from the current
/// filesystem on every startup, resume included (PRD decision 3, "Resume
/// and jump") -- there is no separate repo-map-from-log code path for this
/// test to accidentally exercise, so no repo-map fixture is needed here.
#[tokio::test]
async fn resuming_a_session_includes_prior_turn_in_next_outgoing_request() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    // Single tokens (no spaces) for the same ratatui-partial-redraw reason
    // as the other tests in this file.
    let canned_response = "MockedAssistantReplyForResumeTesting";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-resume",
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

    let home = unique_temp_dir("home-resume");
    let xdg_config_home = unique_temp_dir("xdg-config-home-resume");
    let xdg_data_home = unique_temp_dir("xdg-data-home-resume");

    // Hand-build the fixture session BEFORE spawning: a Header record (via
    // `rokr_session::SessionRecord` + `serde_json::to_string`, never a
    // hand-typed JSON string) followed by a Turn record whose message text
    // contains a unique token this test can later assert appeared in the
    // outgoing request body.
    let fixture_session_id = "01FIXTURERESUMESESSION0000";
    let fixture_session_dir = xdg_data_home
        .join("rokr")
        .join("sessions")
        .join(fixture_session_id);
    std::fs::create_dir_all(&fixture_session_dir)
        .expect("failed to create fixture session directory");

    let fixture_header = rokr_session::SessionRecord::Header {
        schema_version: 1,
        session_id: fixture_session_id.to_string(),
        created_at: "2026-07-20T00:00:00Z".to_string(),
        project_path: "/tmp/fixture-project".to_string(),
        agent_tier: "plan".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
    };
    let fixture_turn = rokr_session::SessionRecord::Turn {
        messages: vec![rokr_core::Message::user_text("priorturnuniquetoken")],
        usage: rokr_session::UsageRecord {
            input_tokens: 5,
            output_tokens: 7,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        timestamp: "2026-07-20T00:00:01Z".to_string(),
    };
    let fixture_contents = format!(
        "{}\n{}\n",
        serde_json::to_string(&fixture_header).expect("serialize fixture Header"),
        serde_json::to_string(&fixture_turn).expect("serialize fixture Turn"),
    );
    std::fs::write(fixture_session_dir.join("session.jsonl"), fixture_contents)
        .expect("failed to write fixture session.jsonl");

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
    cmd.env("XDG_DATA_HOME", &xdg_data_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");
    cmd.arg("--resume");
    cmd.arg(fixture_session_id);

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
        .write_all(b"newpromptaftereresume\r")
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
        output.contains("newpromptaftereresume"),
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

    let received_requests = mock_server.received_requests().await.expect(
        "mock server should have recorded received requests \
         (make sure the request recorder wasn't disabled)",
    );
    assert!(
        !received_requests.is_empty(),
        "expected at least one outgoing request to /chat/completions"
    );

    // No `/compact` happens in this test, so the FIRST request submitted is
    // the one carrying the resumed history plus the new prompt.
    let first_request_body = String::from_utf8_lossy(&received_requests[0].body).into_owned();
    assert!(
        first_request_body.contains("priorturnuniquetoken"),
        "expected the first outgoing request body to contain the resumed session's prior \
         turn text ('priorturnuniquetoken'), proving the resumed session's prior turn was \
         folded into the transcript before this request went out; got body: \
         {first_request_body:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&xdg_data_home);
}

/// RULING 1 done-when #1 (schema v2) acceptance test: proves an ASSISTANT
/// reply from a PRIOR turn survives resume and is actually SENT back to the
/// provider on the next turn -- not merely that it sits in some in-memory
/// Vec. Under the pre-v2 schema only the user prompt was persisted per turn,
/// so an assistant reply could never reappear in a resumed session's
/// outgoing request; this test would fail against that old code. Runs TWO
/// real rokr processes sharing one `XDG_DATA_HOME`: process 1 submits two
/// turns (each yielding a uniquely-tokened assistant reply) then quits;
/// process 2 resumes via `--continue`, submits a third turn, and the third
/// turn's outgoing request body must contain the prior assistant reply
/// token.
#[tokio::test]
async fn resuming_a_session_resends_prior_assistant_reply_in_next_request() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    // Single-token (no spaces) assistant reply, distinct enough to search for
    // in the raw request body. Every turn gets this same reply.
    let assistant_reply_token = "AssistantReplyTokenThatMustSurviveResume";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-assistant-survives",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": assistant_reply_token
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-assistant-survives");
    let xdg_config_home = unique_temp_dir("xdg-config-home-assistant-survives");
    let xdg_data_home = unique_temp_dir("xdg-data-home-assistant-survives");

    // Drives one rokr PTY process: spawns it (optionally with `--continue`),
    // submits each prompt in `prompts` waiting for the assistant reply token
    // between them, then quits with `q`. Returns nothing -- side effects land
    // in the shared session log / mock server.
    let run_session = |continue_flag: bool, prompts: Vec<&'static str>| {
        let home = home.clone();
        let xdg_config_home = xdg_config_home.clone();
        let xdg_data_home = xdg_data_home.clone();
        let uri = mock_server.uri();
        move || {
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
            cmd.env("XDG_DATA_HOME", &xdg_data_home);
            cmd.env("ROKR_OPENAI_BASE_URL", &uri);
            cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
            cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");
            if continue_flag {
                cmd.arg("--continue");
            }

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
                if output.contains("Header") && output.contains("Prompt") {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            assert!(
                output.contains("Header"),
                "expected pty output to contain Header, got: {output:?}"
            );

            for prompt in prompts {
                let mut line = prompt.as_bytes().to_vec();
                line.push(b'\r');
                writer
                    .write_all(&line)
                    .expect("failed to write prompt to pty");

                // Wait for the prompt to be echoed AND at least the reply to
                // land before submitting the next prompt, so turns don't race.
                let reply_deadline = Instant::now() + Duration::from_secs(10);
                let mut seen_prompt = false;
                while Instant::now() < reply_deadline {
                    while let Ok(chunk) = rx.try_recv() {
                        output.push_str(&String::from_utf8_lossy(&chunk));
                    }
                    if output.contains(prompt) {
                        seen_prompt = true;
                    }
                    // Count replies loosely by requiring the token to appear;
                    // between turns it will appear at least as many times as
                    // turns submitted so far. Just require it's present.
                    if seen_prompt && output.contains(assistant_reply_token) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                assert!(
                    output.contains(prompt),
                    "expected pty output to echo prompt {prompt:?}, got: {output:?}"
                );
                // Small settle so the session-writer append for this turn is
                // enqueued before the next prompt (and before quit).
                thread::sleep(Duration::from_millis(200));
            }

            writer.write_all(b"q").expect("failed to write q to pty");
            let exit_deadline = Instant::now() + Duration::from_secs(10);
            let status = loop {
                if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
                    break status;
                }
                if Instant::now() > exit_deadline {
                    let _ = child.kill();
                    panic!("rokr did not exit within timeout; output so far: {output:?}");
                }
                thread::sleep(Duration::from_millis(50));
            };
            assert!(
                status.success(),
                "expected rokr to exit cleanly, got: {status:?}"
            );
        }
    };

    // Process 1: two turns, then quit. Run on a blocking thread since the PTY
    // driving loop is synchronous.
    let session1 = run_session(false, vec!["firstpromptalpha", "secondpromptbeta"]);
    tokio::task::spawn_blocking(session1)
        .await
        .expect("session 1 blocking task panicked");

    // Sanity: exactly one session directory now exists, and its log holds the
    // assistant reply token (proving assistant messages are now persisted).
    let sessions_dir = xdg_data_home.join("rokr").join("sessions");
    let session_dirs: Vec<_> = std::fs::read_dir(&sessions_dir)
        .expect("sessions dir should exist")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    assert_eq!(session_dirs.len(), 1, "expected exactly one session dir");
    let log = std::fs::read_to_string(session_dirs[0].path().join("session.jsonl"))
        .expect("session.jsonl should exist");
    assert!(
        log.contains(assistant_reply_token),
        "expected the persisted session log to contain the assistant reply token (proving \
         schema v2 persists assistant messages), got: {log:?}"
    );

    // Process 2: resume via --continue, submit a third turn.
    let session2 = run_session(true, vec!["thirdpromptgamma"]);
    tokio::task::spawn_blocking(session2)
        .await
        .expect("session 2 blocking task panicked");

    // The LAST outgoing request (process 2's third turn) must carry the prior
    // assistant reply token -- proving it survived resume and was actually
    // sent back to the provider.
    let received = mock_server
        .received_requests()
        .await
        .expect("request recording should be enabled");
    assert!(
        !received.is_empty(),
        "expected at least one outgoing request across both processes"
    );
    let last_body = String::from_utf8_lossy(&received[received.len() - 1].body).into_owned();
    assert!(
        last_body.contains("thirdpromptgamma"),
        "sanity: the last request should be the third turn's, got: {last_body}"
    );
    assert!(
        last_body.contains(assistant_reply_token),
        "expected the resumed session's next outgoing request to contain the PRIOR assistant \
         reply token (proving the assistant reply survived resume and was re-sent to the \
         provider), got: {last_body}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&xdg_data_home);
}

/// Ticket 36 (session-index-list-jump) acceptance test, per the architect
/// scope-amendment ruling: end-to-end PTY proof that `/resume <id>` (no
/// `--yes`) warns without mutating, and `/resume <id> --yes` swaps the
/// running transcript AND repoints the active session writer to the target
/// session -- proven by checking that a post-jump submitted turn lands in
/// the TARGET session's log rather than the ORIGIN session's.
#[tokio::test]
async fn resume_without_confirm_warns_and_confirm_swaps_transcript_and_writer() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    let canned_response = "MockedAssistantReplyAfterJumpForTesting";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-post-jump",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": canned_response },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-resume-jump");
    let xdg_config_home = unique_temp_dir("xdg-config-home-resume-jump");
    let xdg_data_home = unique_temp_dir("xdg-data-home-resume-jump");

    let target_session_id = "01FIXTURERESUMEJUMPTARGET0";
    let sessions_root = xdg_data_home.join("rokr").join("sessions");
    let target_dir = sessions_root.join(target_session_id);
    std::fs::create_dir_all(&target_dir).expect("failed to create target fixture session dir");

    let target_header = rokr_session::SessionRecord::Header {
        schema_version: 1,
        session_id: target_session_id.to_string(),
        created_at: "2026-07-20T01:00:00Z".to_string(),
        project_path: "/tmp/fixture-project-target".to_string(),
        agent_tier: "plan".to_string(),
        provider: "openai".to_string(),
        model: "gpt-fixture-target".to_string(),
    };
    let target_turn0 = rokr_session::SessionRecord::Turn {
        messages: vec![rokr_core::Message::user_text("targetjumpturnzero")],
        usage: rokr_session::UsageRecord {
            input_tokens: 1,
            output_tokens: 1,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        timestamp: "2026-07-20T01:00:01Z".to_string(),
    };
    let target_turn1 = rokr_session::SessionRecord::Turn {
        messages: vec![rokr_core::Message::assistant_text("targetjumpturnone")],
        usage: rokr_session::UsageRecord {
            input_tokens: 2,
            output_tokens: 2,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        timestamp: "2026-07-20T01:00:02Z".to_string(),
    };
    let target_compaction = rokr_session::SessionRecord::Compaction {
        summary: "targetjumpcompactionsummary".to_string(),
        replaced_through: 1,
    };
    let target_turn2 = rokr_session::SessionRecord::Turn {
        messages: vec![rokr_core::Message::user_text("targetjumpturntwo")],
        usage: rokr_session::UsageRecord {
            input_tokens: 3,
            output_tokens: 3,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        timestamp: "2026-07-20T01:00:03Z".to_string(),
    };
    let target_session_contents = format!(
        "{}\n{}\n{}\n{}\n{}\n",
        serde_json::to_string(&target_header).unwrap(),
        serde_json::to_string(&target_turn0).unwrap(),
        serde_json::to_string(&target_turn1).unwrap(),
        serde_json::to_string(&target_compaction).unwrap(),
        serde_json::to_string(&target_turn2).unwrap(),
    );
    std::fs::write(target_dir.join("session.jsonl"), target_session_contents)
        .expect("failed to write target fixture session.jsonl");

    let target_index_entry = rokr_session::SessionIndexEntry {
        session_id: target_session_id.to_string(),
        project_path: "/tmp/fixture-project-target".to_string(),
        created_at: "2026-07-20T01:00:00Z".to_string(),
        updated_at: "2026-07-20T01:00:03Z".to_string(),
        title: "targetjumpturnzero".to_string(),
        turn_count: 3,
        last_model: "gpt-fixture-target".to_string(),
    };
    std::fs::write(
        sessions_root.join("index.jsonl"),
        format!("{}\n", serde_json::to_string(&target_index_entry).unwrap()),
    )
    .expect("failed to write fixture index.jsonl");

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
    cmd.env("XDG_DATA_HOME", &xdg_data_home);
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

    // Discover the freshly-created ORIGIN session's id: the only directory
    // under sessions/ that isn't the pre-seeded target fixture. The live
    // process creates its own session synchronously at startup, before the
    // TUI even starts drawing, so it's already present by the time "Header"
    // appears above.
    let origin_session_id = std::fs::read_dir(&sessions_root)
        .expect("failed to read sessions dir")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .find(|name| name != target_session_id)
        .expect("expected the live process to have created its own origin session directory");

    // Step 1: /resume without --yes must warn, naming the exact confirm
    // command, without mutating anything.
    writer
        .write_all(format!("/resume {target_session_id}\r").as_bytes())
        .expect("failed to write /resume (no confirm) to pty");
    let warn_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < warn_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("--yes") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    // Single unbroken tokens rather than one contiguous multi-word
    // substring: ratatui's diff-based redraw skips re-emitting a cell
    // whose content is unchanged from the prior frame (here, the session
    // id echoed earlier in the just-typed `/resume <id>` command line),
    // which splits an otherwise-contiguous phrase across cursor-jump
    // escape sequences in the raw pty byte stream -- see this file's other
    // tests (e.g. `typing_compact_command_compacts_transcript_immediately`)
    // for the same convention.
    assert!(
        output.contains("Switching")
            && output.contains("(targetjumpturnzero,")
            && output.contains("turns)")
            && output.contains("'/resume")
            && output.contains("--yes'")
            && output.contains("confirm."),
        "expected the warning to echo the exact confirm command, got: {output:?}"
    );

    // Step 2: confirm the jump.
    writer
        .write_all(format!("/resume {target_session_id} --yes\r").as_bytes())
        .expect("failed to write /resume --yes to pty");
    let confirm_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < confirm_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Resumed") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("Resumed"),
        "expected the confirmation message after --yes, got: {output:?}"
    );

    // Step 3: a subsequent submitted turn must land in the TARGET
    // session's log, not the origin's -- proving the writer was actually
    // repointed, not just the in-memory transcript.
    writer
        .write_all(b"postjumpsubmittedturn\r")
        .expect("failed to write post-jump prompt to pty");
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
        "expected the mocked reply after the post-jump prompt, got: {output:?}"
    );

    // Poll the filesystem (rather than reading once right after process
    // exit) so this doesn't race the writer task's async file write --
    // the process is still running at this point, so there's no shutdown
    // race to worry about.
    let write_deadline = Instant::now() + Duration::from_secs(10);
    let mut target_contents_after = String::new();
    while Instant::now() < write_deadline {
        target_contents_after =
            std::fs::read_to_string(target_dir.join("session.jsonl")).unwrap_or_default();
        if target_contents_after.contains("postjumpsubmittedturn") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        target_contents_after.contains("postjumpsubmittedturn"),
        "expected the post-jump turn to be appended to the TARGET session's log, got: {target_contents_after:?}"
    );

    let origin_contents_after =
        std::fs::read_to_string(sessions_root.join(&origin_session_id).join("session.jsonl"))
            .unwrap_or_default();
    assert!(
        !origin_contents_after.contains("postjumpsubmittedturn"),
        "the post-jump turn must NOT appear in the ORIGIN session's log, got: {origin_contents_after:?}"
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
    let _ = std::fs::remove_dir_all(&xdg_data_home);
}

/// Ticket 36 scope-amendment regression guard: `rokr-tui`'s `event_loop`
/// keeps its existing `if state.pending { continue; }` gate untouched (the
/// architect ruling explicitly rejected extending `rokr-tui`'s API to make
/// `/resume` reachable mid-tool-loop) -- this proves, from OUTSIDE
/// `rokr-tui`, that keystrokes typed while a submit call is pending are
/// still silently dropped rather than reaching `command`, so `/resume` and
/// a live tool loop remain mutually exclusive "for free". A future change
/// that accidentally reintroduces a bypass (e.g. letting slash-commands
/// through while pending) would fail this test.
#[tokio::test]
async fn pending_state_drops_keystrokes_so_resume_cannot_run_mid_tool_loop() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    let slow_reply_text = "SlowReplyAfterDelayForPendingGuardTesting";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "id": "chatcmpl-test-slow",
                    "object": "chat.completion",
                    "choices": [
                        {
                            "index": 0,
                            "message": { "role": "assistant", "content": slow_reply_text },
                            "finish_reason": "stop"
                        }
                    ]
                }))
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-pending-guard");
    let xdg_config_home = unique_temp_dir("xdg-config-home-pending-guard");
    let xdg_data_home = unique_temp_dir("xdg-data-home-pending-guard");

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
    cmd.env("XDG_DATA_HOME", &xdg_data_home);
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

    // Submit a prompt whose reply is deliberately delayed, putting the TUI
    // into `state.pending == true` for the next few seconds.
    writer
        .write_all(b"slowpendingprompt\r")
        .expect("failed to write slow prompt to pty");
    // Give the render loop a brief moment to actually flip into `pending`
    // (Enter's keypress handling runs synchronously in the same loop
    // iteration that spawns the submit future).
    thread::sleep(Duration::from_millis(200));

    // Attempt to type a `/resume` command WHILE still pending.
    writer
        .write_all(b"/resume nonexistentduringpending\r")
        .expect("failed to write /resume attempt to pty");

    // Drain output for a couple seconds -- nowhere near the mock's 3s
    // delay -- and confirm no `/resume`-produced reply shows up.
    let during_pending_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < during_pending_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        thread::sleep(Duration::from_millis(50));
    }
    // Checked as two separate single-word tokens rather than one
    // contiguous "no such session" substring: ratatui's diff-based redraw
    // can skip re-emitting a cell whose content is already correct (e.g. a
    // space that was already blank), which would otherwise split an
    // actually-rendered phrase across a cursor-jump escape sequence in the
    // raw pty byte stream -- see the other tests in this file (e.g.
    // `typing_compact_command_compacts_transcript_immediately`) for the
    // same convention. Both tokens must be absent to prove the message
    // never rendered at all.
    assert!(
        !output.contains("no such") && !output.contains("session"),
        "expected keystrokes typed while pending to be dropped entirely (no /resume reply while pending), got: {output:?}"
    );

    // Now wait for the slow reply to actually land, confirming the app is
    // no longer pending.
    let slow_reply_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < slow_reply_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(slow_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(slow_reply_text),
        "expected the delayed reply to eventually land, got: {output:?}"
    );

    // Proof the earlier keystrokes were dropped, not merely slow to
    // process: typing the SAME `/resume` command now (no longer pending)
    // DOES produce a reply.
    writer
        .write_all(b"/resume nonexistentduringpending\r")
        .expect("failed to write /resume attempt (post-pending) to pty");
    let after_pending_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < after_pending_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("no such") && output.contains("session") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    // Same single-token-pair convention as above: the rendered phrase is
    // "no such session <id>", but the space between "such" and "session"
    // may not be re-emitted if that screen cell already held a space from
    // an earlier frame, splitting the literal contiguous substring across
    // a cursor-jump escape sequence in the raw pty byte stream.
    assert!(
        output.contains("no such") && output.contains("session"),
        "expected /resume to work normally once no longer pending, got: {output:?}"
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
    let _ = std::fs::remove_dir_all(&xdg_data_home);
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

    // Wait for the command to actually finish — the "Transcript compacted."
    // confirmation `main.rs`'s `/compact` handler pushes once the async
    // compaction call resolves — not merely for "/compact" to be echoed
    // back. The echo appears the instant Enter is pressed, before the
    // command even starts running; racing ahead on it left `state.pending`
    // still true when the next prompt's keystrokes arrived, and the render
    // loop silently drops keystrokes typed while pending (see
    // `rokr-tui::event_loop`), so "thirdpromptunique" below could be typed
    // away before compaction ever completed.
    //
    // We match on "compacted." rather than the full "Transcript compacted."
    // phrase: the TUI's renderer diffs cells and skips repainting ones that
    // are already correct (e.g. a space that's already blank), emitting a
    // cursor-address escape instead of a literal space byte. That splits
    // "Transcript compacted." across an escape sequence in the raw PTY
    // stream, so the two-word phrase never appears as a contiguous
    // substring. "compacted." itself is one uninterrupted run of cells and
    // renders contiguously, and — like the rest of this file's assertions,
    // which all match single tokens for the same reason — is unambiguous
    // here (it doesn't collide with any other text the test produces, e.g.
    // "...BeforeCompactForTesting").
    let compact_confirmation_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < compact_confirmation_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("compacted.") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("compacted."),
        "expected pty output to contain the compaction completion confirmation after /compact, got: {output:?}"
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
    cmd.cwd(&temp_dir);
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

/// Ticket 77 (read-only-git-carveout, ADR 0019) acceptance test: closely
/// modeled on `bash_tool_call_renders_permission_prompt_and_runs_on_accept`
/// above, but for an unambiguously read-only git command (`git status`)
/// inside a real repo. The carve-out inside `PermissionPolicy::resolve`
/// must convert what would otherwise be a `Prompt` into an `Allow`, so no
/// permission prompt is ever rendered -- the PTY output goes straight from
/// the submitted prompt to the final assistant reply, with no keypress
/// needed in between.
#[tokio::test]
async fn read_only_git_status_executes_without_permission_prompt() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let final_reply_text = "FinalReplyAfterReadOnlyGitStatusForTesting";

    let project_dir = unique_temp_dir("read-only-git-status-project");
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&project_dir)
            .status()
            .expect("git command should spawn");
        assert!(status.success(), "git {args:?} should succeed");
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(project_dir.join("a.txt"), "hello").unwrap();
    git(&["add", "a.txt"]);
    git(&["commit", "-m", "first commit"]);

    // First call: the model asks to invoke the `bash` tool with an
    // unambiguously read-only git command.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-read-only-git-status",
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
                                    "arguments": serde_json::json!({ "command": "git status" }).to_string()
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
            "id": "chatcmpl-test-read-only-git-status-final",
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

    let home = unique_temp_dir("home-read-only-git-status");
    let xdg_config_home = unique_temp_dir("xdg-config-home-read-only-git-status");

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
        .write_all(b"checkgitstatus\r")
        .expect("failed to write prompt to pty");

    // No accept/reject keypress is written here at all: the carve-out
    // means no permission prompt should ever render, so the PTY output
    // should go straight to the final assistant reply.
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
        "expected pty output to contain the final assistant reply with no permission prompt \
         needed for a read-only `git status` invocation, got: {output:?}"
    );
    assert!(
        !output.contains("[y]") && !output.contains("[n]"),
        "expected no permission prompt to ever render for a read-only `git status` invocation, \
         got: {output:?}"
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
    let _ = std::fs::remove_dir_all(&project_dir);
}

/// Ticket 77 (read-only-git-carveout, ADR 0019) acceptance test: a bash
/// command that is NOT unambiguously read-only git -- here, a chained
/// command that starts with a read-only-looking `git status` but appends a
/// disqualifying shell metacharacter and a mutating command -- must still
/// render a permission prompt, proving the carve-out didn't over-widen.
#[tokio::test]
async fn ambiguous_or_mutating_git_invocations_still_prompt() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let final_reply_text = "FinalReplyAfterAmbiguousGitRejectForTesting";
    let ambiguous_command = "git status && rm -rf x";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-ambiguous-git",
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
                                    "arguments": serde_json::json!({ "command": ambiguous_command }).to_string()
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

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-ambiguous-git-final",
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

    let home = unique_temp_dir("home-ambiguous-git");
    let xdg_config_home = unique_temp_dir("xdg-config-home-ambiguous-git");
    let temp_dir = unique_temp_dir("ambiguous-git-target");

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
    cmd.cwd(&temp_dir);
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
        .write_all(b"runambiguousgit\r")
        .expect("failed to write prompt to pty");

    // The carve-out must NOT classify this command as read-only git (it
    // has a disqualifying `&&` and a mutating `rm -rf`), so a permission
    // prompt must render before anything executes.
    let prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("[y]") && output.contains("[n]") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    // The permission prompt's detail text word-wraps across PTY cell
    // positions with cursor-movement escape sequences between words, so a
    // literal multi-word substring like "git status" won't appear
    // contiguous in the raw buffer -- check individual word tokens instead,
    // matching how `bash_tool_call_renders_permission_prompt_and_runs_on_
    // accept` above checks for the tool name and a single-token marker
    // rather than the full command string.
    assert!(
        output.contains("bash"),
        "expected pty output to contain the tool name 'bash' in a permission prompt, got: {output:?}"
    );
    assert!(
        output.contains("status") && output.contains("-rf"),
        "expected pty output to contain the ambiguous command's distinguishing tokens in a \
         permission prompt, got: {output:?}"
    );
    assert!(
        output.contains("[y]") && output.contains("[n]"),
        "expected a permission prompt to render for a non-read-only-git bash command, got: \
         {output:?}"
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
        "expected pty output to contain the final assistant reply after rejecting the \
         ambiguous git tool call, got: {output:?}"
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

/// Ticket 49 (hooks-tracer-bullet) acceptance test, updated by ticket 50
/// (hooks-remaining-events-and-config) to configure the hook via a real
/// `rokr.json` `hooks.PreToolUse` entry (`write_hooks_config`) instead of
/// ticket 49's interim `ROKR_PRETOOLUSE_HOOK` env var, which ticket 50
/// removes entirely from `main.rs`: a real shell-command `PreToolUse` hook
/// that exits 2 must veto a `bash` tool call before the permission prompt
/// ever renders. Mirrors
/// `bash_tool_call_renders_permission_prompt_and_runs_on_accept`/
/// `bash_tool_call_skips_execution_on_reject`'s structure, but asserts the
/// ABSENCE of the permission prompt (the marker path text, which only ever
/// appears in that prompt's rendered command line) rather than waiting for
/// it and then accepting/rejecting interactively.
#[tokio::test]
async fn pretooluse_hook_script_denies_bash_call_before_permission_prompt_appears() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let final_reply_text = "FinalReplyAfterHookVetoForTesting";

    let temp_dir = unique_temp_dir("hook-veto-target");
    let marker_path = temp_dir.join("hookveto-marker");
    let marker_path_str = marker_path.to_string_lossy().into_owned();
    let bash_command = format!("touch {marker_path_str}");

    // First call: the model asks to invoke the `bash` tool -- the
    // PreToolUse hook below must veto this before any permission prompt
    // renders.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-hook-veto",
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

    // The loop must continue after the hook veto (same "loop continues"
    // shape as an interactive rejection): the model gets an error tool
    // result and still produces a final reply.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-hook-veto-final",
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

    let home = unique_temp_dir("home-hook-veto");
    let xdg_config_home = unique_temp_dir("xdg-config-home-hook-veto");
    // The hook: reads (and discards) its JSON stdin payload, then always
    // exits 2 -- a blocking veto regardless of which tool call it saw.
    write_hooks_config(
        &xdg_config_home,
        "PreToolUse",
        "cat >/dev/null; echo 'vetoed: no bash allowed' >&2; exit 2",
    );

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

    // No "y"/"n" keypress is ever sent: a hook veto must short-circuit
    // before the permission prompt runs at all, so the loop should reach
    // the final reply on its own.
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
        "expected pty output to contain the final assistant reply after the hook veto (the \
         loop must continue on its own, with no permission keypress needed), got: {output:?}"
    );
    assert!(
        !output.contains("hookveto-marker"),
        "the marker path only ever appears in a rendered permission-prompt command line -- its \
         presence would mean the prompt rendered despite the hook's veto, got: {output:?}"
    );
    assert!(
        !marker_path.exists(),
        "the bash command must never run after the PreToolUse hook vetoed it"
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

/// Ticket 50 (hooks-remaining-events-and-config) acceptance test: a
/// `rokr.json` with a `SessionStart` hook whose command echoes distinctive
/// text to stdout must have that text folded into the outgoing system
/// prompt at startup -- proven the same way
/// `agents_md_content_appears_in_outgoing_system_prompt` proves AGENTS.md's
/// project-context injection: by inspecting the FIRST request's raw body
/// via the mock server's request recording, not by scraping rendered pty
/// bytes (ratatui's own line-wrapping of a long system prompt would make a
/// literal on-screen substring match unreliable).
#[tokio::test]
async fn sessionstart_hook_stdout_appears_in_transcript_context_at_startup() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    let canned_response = "MockedAssistantReplyForSessionStartHookTesting";
    let session_start_marker = "DistinctiveSessionStartHookStdoutForTesting";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-sessionstart-hook",
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

    let home = unique_temp_dir("home-sessionstart-hook");
    let xdg_config_home = unique_temp_dir("xdg-config-home-sessionstart-hook");
    write_hooks_config(
        &xdg_config_home,
        "SessionStart",
        &format!("echo '{session_start_marker}'"),
    );

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
        first_request_body.contains(session_start_marker),
        "expected the outgoing request body to contain the SessionStart hook's stdout, proving \
         it was injected into the conversation context at startup, got: {first_request_body}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

/// Ticket 50 (hooks-remaining-events-and-config) acceptance test: a
/// `rokr.json` with a `UserPromptSubmit` hook whose command echoes
/// distinctive text to stdout must have that text injected into EVERY
/// submitted turn's outgoing request, not just the first -- proven across
/// two separate submissions (mirroring
/// `second_prompt_includes_prior_turn_in_request_body`'s two-turn PTY
/// structure), inspecting each turn's own raw request body via the mock
/// server's request recording.
#[tokio::test]
async fn userpromptsubmit_hook_injects_context_before_each_prompt_is_sent() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    let first_reply_text = "FirstReplyAfterUserPromptSubmitHookForTesting";
    let second_reply_text = "SecondReplyAfterUserPromptSubmitHookForTesting";
    let user_prompt_submit_marker = "DistinctiveUserPromptSubmitHookStdoutForTesting";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-userpromptsubmit-hook-first",
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
            "id": "chatcmpl-test-userpromptsubmit-hook-second",
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

    let home = unique_temp_dir("home-userpromptsubmit-hook");
    let xdg_config_home = unique_temp_dir("xdg-config-home-userpromptsubmit-hook");
    write_hooks_config(
        &xdg_config_home,
        "UserPromptSubmit",
        &format!("echo '{user_prompt_submit_marker}'"),
    );

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
        first_request_body.contains(user_prompt_submit_marker),
        "expected the first turn's outgoing request body to contain the UserPromptSubmit hook's \
         stdout, got: {first_request_body}"
    );
    // A plain `.contains()` check here would be tautological: turn 1's
    // injected marker is already persisted into the transcript as part of
    // that turn's user message, and turn 2's request body carries the
    // WHOLE transcript so far (proven separately by
    // `second_prompt_includes_prior_turn_in_request_body`) -- so the marker
    // would appear in the second body even if the hook never fired again.
    // Counting occurrences is what actually proves a SECOND, fresh
    // injection happened: one carried over from turn 1's transcript entry,
    // one newly injected into turn 2's own user message.
    let second_body_marker_count = second_request_body
        .matches(user_prompt_submit_marker)
        .count();
    assert!(
        second_body_marker_count >= 2,
        "expected the SECOND turn's outgoing request body to contain the UserPromptSubmit \
         hook's stdout TWICE (once carried over from turn 1's transcript entry, once freshly \
         injected into turn 2's own prompt) -- it must fire before EVERY submitted prompt, not \
         just the first; got {second_body_marker_count} occurrence(s) in: {second_request_body}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

/// Ticket 50 (hooks-remaining-events-and-config) acceptance test: the glob
/// matcher must only fire a `PreToolUse` hook for tool names it actually
/// matches. A hook configured with `matcher: "mcp__*"` (which always exits
/// 2, i.e. would veto everything if it fired) must NOT veto a `bash` call --
/// mirrors `bash_tool_call_renders_permission_prompt_and_runs_on_accept`'s
/// exact structure (permission prompt renders, accepting it lets the
/// command actually run), the opposite assertion from
/// `pretooluse_hook_script_denies_bash_call_before_permission_prompt_appears`
/// above, which uses no `matcher` (defaulting to match-everything) and DOES
/// veto.
#[tokio::test]
async fn pretooluse_hook_matcher_only_vetoes_matching_tool_names() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let final_reply_text = "FinalReplyAfterNonMatchingHookForTesting";

    let temp_dir = unique_temp_dir("hook-matcher-target");
    let marker_path = temp_dir.join("hookmatcher-marker");
    let marker_path_str = marker_path.to_string_lossy().into_owned();
    let bash_command = format!("touch {marker_path_str}");

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-hook-matcher",
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

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-hook-matcher-final",
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

    let home = unique_temp_dir("home-hook-matcher");
    let xdg_config_home = unique_temp_dir("xdg-config-home-hook-matcher");
    let config_dir = xdg_config_home.join("rokr");
    std::fs::create_dir_all(&config_dir).expect("failed to create rokr config dir for test");
    let config = serde_json::json!({
        "version": 1,
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": "mcp__*",
                    "command": "cat >/dev/null; echo 'should never veto bash' >&2; exit 2"
                }
            ]
        }
    });
    std::fs::write(
        config_dir.join("rokr.json"),
        serde_json::to_string_pretty(&config).expect("failed to serialize test rokr.json"),
    )
    .expect("failed to write test rokr.json");

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
    cmd.cwd(&temp_dir);
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

    // The non-matching hook must NOT veto: the permission prompt should
    // still render, exactly like the no-hook-configured case.
    let prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("bash") && output.contains("hookmatcher-marker") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("bash") && output.contains("hookmatcher-marker"),
        "expected the permission prompt to render (proving the non-matching 'mcp__*' hook did \
         NOT veto the bash call), got: {output:?}"
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
        "expected pty output to contain the final assistant reply, got: {output:?}"
    );
    assert!(
        marker_path.exists(),
        "the bash command must have run after permission was granted (the non-matching hook \
         must never have vetoed it)"
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
    cmd.cwd(&temp_dir);
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

/// Ticket 38 (checkpoint-pre-images) acceptance test: a Build-tier session's
/// ACCEPTED write tool call captures the file's pre-image under
/// `sessions/<id>/snapshots/`, keyed by `(turn_index, path)`, and appends a
/// correlating `SessionRecord::Checkpoint` record to `session.jsonl`.
/// Mirrors `write_tool_call_renders_diff_and_writes_file_on_accept`'s exact
/// PTY harness (wiremock two-step tool-call/final-reply mock, `y` to accept)
/// plus `submitting_a_prompt_persists_header_and_turn_records_to_session_jsonl`'s
/// `XDG_DATA_HOME` + session-directory-discovery convention, since this test
/// needs to inspect the persisted log AND the new snapshots directory.
#[tokio::test]
async fn write_tool_call_captures_pre_image_snapshot_and_appends_checkpoint_record() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let final_reply_text = "FinalReplyAfterWriteCheckpointForTesting";

    let temp_dir = unique_temp_dir("write-checkpoint-target");
    let target_file = temp_dir.join("checkpoint-target.txt");
    let old_content = "preimagecontentbeforewrite";
    let new_content = "postimagecontentafterwrite";
    std::fs::write(&target_file, old_content).unwrap();
    let target_path = target_file.to_string_lossy().into_owned();

    // First call: the model asks to invoke the `write` tool against the real
    // temp file, replacing its content.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-write-checkpoint",
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
            "id": "chatcmpl-test-write-checkpoint-final",
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

    let home = unique_temp_dir("home-write-checkpoint");
    let xdg_config_home = unique_temp_dir("xdg-config-home-write-checkpoint");
    let xdg_data_home = unique_temp_dir("xdg-data-home-write-checkpoint");

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
    cmd.env("XDG_DATA_HOME", &xdg_data_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");
    cmd.cwd(&temp_dir);
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
        .write_all(b"writecheckpointfile\r")
        .expect("failed to write prompt to pty");

    // Wait for the permission prompt to render the diff before granting
    // anything.
    let prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("write")
            && output.contains("-preimagecontentbeforewrite")
            && output.contains("+postimagecontentafterwrite")
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("-preimagecontentbeforewrite"),
        "expected pty output to contain the old-content diff line, got: {output:?}"
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

    // Locate the (single) ULID-named session directory, mirroring
    // `submitting_a_prompt_persists_header_and_turn_records_to_session_jsonl`'s
    // discovery convention.
    let sessions_dir = xdg_data_home.join("rokr").join("sessions");
    let session_dir_entries: Vec<std::fs::DirEntry> = std::fs::read_dir(&sessions_dir)
        .unwrap_or_else(|err| {
            panic!("expected sessions directory to exist at {sessions_dir:?}, got error: {err:?}")
        })
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .collect();
    assert_eq!(
        session_dir_entries.len(),
        1,
        "expected exactly one ULID-named session directory under {sessions_dir:?}, got: {:?}",
        session_dir_entries
            .iter()
            .map(|entry| entry.path())
            .collect::<Vec<_>>()
    );
    let session_dir = session_dir_entries[0].path();

    // (1) A snapshot file exists under sessions/<id>/snapshots/ whose
    // content matches the fixture's old_content -- keyed by
    // (turn_index, path), not just "a file exists somewhere": this is the
    // first (and only) submitted turn, so its turn_index is 0 (fold's
    // next_turn_index semantics: 0 prior Turn records at the time this
    // turn's tool loop ran).
    let snapshots_dir = session_dir.join("snapshots");
    let snapshot_entries: Vec<std::fs::DirEntry> = std::fs::read_dir(&snapshots_dir)
        .unwrap_or_else(|err| {
            panic!("expected snapshots directory to exist at {snapshots_dir:?}, got error: {err:?}")
        })
        .filter_map(|entry| entry.ok())
        .collect();
    assert_eq!(
        snapshot_entries.len(),
        1,
        "expected exactly one snapshot file under {snapshots_dir:?}, got: {:?}",
        snapshot_entries
            .iter()
            .map(|entry| entry.path())
            .collect::<Vec<_>>()
    );
    let snapshot_file_name = snapshot_entries[0]
        .file_name()
        .to_string_lossy()
        .into_owned();
    assert!(
        snapshot_file_name.starts_with("t0-"),
        "expected the snapshot id to be keyed by turn_index 0 (this session's first turn), \
         got filename: {snapshot_file_name:?}"
    );
    let snapshot_contents = std::fs::read_to_string(snapshot_entries[0].path())
        .expect("snapshot file should be readable");
    assert_eq!(
        snapshot_contents, old_content,
        "expected the snapshot's content to exactly match the pre-image (the old side of the \
         permission-preview diff)"
    );

    // (2) A `SessionRecord::Checkpoint { turn_index, snapshot_id }` record
    // appears in session.jsonl, its turn_index is 0 (this write's turn),
    // and its snapshot_id matches the snapshot file found in (1).
    let session_jsonl_path = session_dir.join("session.jsonl");
    let session_jsonl_contents = std::fs::read_to_string(&session_jsonl_path)
        .expect("failed to read session.jsonl contents");
    let checkpoint_records: Vec<rokr_session::SessionRecord> = session_jsonl_contents
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<rokr_session::SessionRecord>(line).ok())
        .filter(|record| matches!(record, rokr_session::SessionRecord::Checkpoint { .. }))
        .collect();
    assert_eq!(
        checkpoint_records.len(),
        1,
        "expected exactly one Checkpoint record in session.jsonl, got: {checkpoint_records:?}"
    );
    match &checkpoint_records[0] {
        rokr_session::SessionRecord::Checkpoint {
            turn_index,
            snapshot_id,
        } => {
            assert_eq!(
                *turn_index, 0,
                "expected the Checkpoint record's turn_index to be 0 for this session's first \
                 (write) turn"
            );
            assert_eq!(
                snapshot_id, &snapshot_file_name,
                "expected the Checkpoint record's snapshot_id to correspond to the snapshot \
                 file found under sessions/<id>/snapshots/"
            );
        }
        other => panic!("expected a Checkpoint record, got: {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&xdg_data_home);
    let _ = std::fs::remove_dir_all(&temp_dir);
}

/// Ticket 39 (rollback-command) acceptance test, updated for RULING 3's
/// `> target` boundary: `/rollback [turn]` restores every file snapshot
/// captured STRICTLY AFTER the target turn index to its pre-image content
/// (verified against the real filesystem), truncates the running transcript
/// to that turn's boundary (verified by inspecting the NEXT turn's actual
/// outgoing request body via `mock_server.received_requests()`, mirroring
/// `auto_compaction_triggers_once_usage_crosses_threshold_and_preserves_recent_turn`'s
/// exact technique for proving transcript content), and appends a
/// `SessionRecord::Rollback` record to `session.jsonl`. Three turns are
/// scripted: turn 0 and turn 1 each perform an ACCEPTED `write` tool call
/// against the same real temp file (mirroring
/// `write_tool_call_captures_pre_image_snapshot_and_appends_checkpoint_record`'s
/// exact write/diff/accept PTY sequence), and turn 2 is a plain
/// tool-call-free chat turn. `/rollback 1` = "world as of END of turn 1", so
/// turn 1's OWN written content must SURVIVE on disk (only turns strictly
/// after 1 are undone -- and turn 2 wrote nothing), while the transcript is
/// still truncated back to turn_index <= 1, discarding turn 2. A fourth turn
/// is submitted afterward to inspect what actually goes out on the wire.
#[tokio::test]
async fn rollback_command_restores_file_and_truncates_transcript_to_target_turn() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let temp_dir = unique_temp_dir("rollback-command-target");
    let target_file = temp_dir.join("rollback-target.txt");
    let initial_content = "rollbackpreimagebeforeanywrite";
    let turn0_content = "rollbackcontentafterturnzero";
    let turn1_content = "rollbackcontentafterturnone";
    std::fs::write(&target_file, initial_content).unwrap();
    let target_path = target_file.to_string_lossy().into_owned();

    let turn0_reply_text = "FinalReplyTurnZeroForRollbackTest";
    let turn1_reply_text = "FinalReplyTurnOneForRollbackTest";
    let turn2_reply_text = "FinalReplyTurnTwoForRollbackTest";
    let turn3_reply_text = "FinalReplyTurnThreeForRollbackTest";

    // Turn 0: the model writes turn0_content over the initial file content.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-rollback-turn0-toolcall",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "tool_calls": [
                            {
                                "id": "call_turn0",
                                "type": "function",
                                "function": {
                                    "name": "write",
                                    "arguments": serde_json::json!({
                                        "path": target_path,
                                        "content": turn0_content
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

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-rollback-turn0-final",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": turn0_reply_text },
                    "finish_reason": "stop"
                }
            ]
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Turn 1: the model writes turn1_content over turn0_content.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-rollback-turn1-toolcall",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "tool_calls": [
                            {
                                "id": "call_turn1",
                                "type": "function",
                                "function": {
                                    "name": "write",
                                    "arguments": serde_json::json!({
                                        "path": target_path,
                                        "content": turn1_content
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

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-rollback-turn1-final",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": turn1_reply_text },
                    "finish_reason": "stop"
                }
            ]
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Turn 2: a plain, tool-call-free chat turn -- this is the turn
    // `/rollback 1` must discard from the transcript.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-rollback-turn2",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": turn2_reply_text },
                    "finish_reason": "stop"
                }
            ]
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Turn 3 (post-rollback): catch-all, uncapped.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-rollback-turn3",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": turn3_reply_text },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-rollback-command");
    let xdg_config_home = unique_temp_dir("xdg-config-home-rollback-command");
    let xdg_data_home = unique_temp_dir("xdg-data-home-rollback-command");

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
    cmd.env("XDG_DATA_HOME", &xdg_data_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");
    cmd.cwd(&temp_dir);
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

    // Turn 0: write, wait for the diff, accept.
    writer
        .write_all(b"turnzerowriteprompt\r")
        .expect("failed to write turn0 prompt to pty");
    let turn0_diff_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < turn0_diff_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("-rollbackpreimagebeforeanywrite")
            && output.contains("+rollbackcontentafterturnzero")
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("-rollbackpreimagebeforeanywrite"),
        "expected pty output to contain turn 0's diff, got: {output:?}"
    );
    writer
        .write_all(b"y")
        .expect("failed to write turn0 accept keypress to pty");

    let turn0_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < turn0_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(turn0_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(turn0_reply_text),
        "expected pty output to contain turn 0's final reply, got: {output:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&target_file).unwrap(),
        turn0_content,
        "expected the file to have been written with turn 0's content after accepting"
    );

    // Turn 1: write again, wait for the diff, accept.
    writer
        .write_all(b"turnonewriteprompt\r")
        .expect("failed to write turn1 prompt to pty");
    let turn1_diff_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < turn1_diff_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("-rollbackcontentafterturnzero")
            && output.contains("+rollbackcontentafterturnone")
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("-rollbackcontentafterturnzero"),
        "expected pty output to contain turn 1's diff, got: {output:?}"
    );
    writer
        .write_all(b"y")
        .expect("failed to write turn1 accept keypress to pty");

    let turn1_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < turn1_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(turn1_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(turn1_reply_text),
        "expected pty output to contain turn 1's final reply, got: {output:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&target_file).unwrap(),
        turn1_content,
        "expected the file to have been written with turn 1's content after accepting"
    );

    // Turn 2: a plain chat turn with no tool call -- this is the turn
    // rollback must discard.
    writer
        .write_all(b"turntwochatprompt\r")
        .expect("failed to write turn2 prompt to pty");
    let turn2_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < turn2_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(turn2_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(turn2_reply_text),
        "expected pty output to contain turn 2's final reply, got: {output:?}"
    );

    // Roll back to turn 1: RULING 3's `> target` boundary means "world as of
    // END of turn 1", so turn 1's OWN write must SURVIVE on disk (only turns
    // strictly after 1 are undone -- and turn 2 did no file mutation). The
    // transcript is still truncated to discard turn 2.
    writer
        .write_all(b"/rollback 1\r")
        .expect("failed to write /rollback command to pty");
    let rollback_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < rollback_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Rolled") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("Rolled"),
        "expected pty output to contain a rollback confirmation, got: {output:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&target_file).unwrap(),
        turn1_content,
        "expected the file to STILL hold turn 1's own written content after rolling back to \
         turn 1 (RULING 3: turn N's own mutation survives a rollback to N)"
    );

    // Turn 3 (post-rollback): submitted so its OUTGOING request body can be
    // inspected for whether turn 2 was actually discarded from the running
    // transcript.
    writer
        .write_all(b"turnthreechatprompt\r")
        .expect("failed to write turn3 prompt to pty");
    let turn3_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < turn3_response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(turn3_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(turn3_reply_text),
        "expected pty output to contain turn 3's final reply, got: {output:?}"
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

    // (1) Filesystem proof.
    assert_eq!(
        std::fs::read_to_string(&target_file).unwrap(),
        turn1_content,
        "expected the file's final on-disk content (after process exit) to still be turn 1's \
         own written content (RULING 3: turn N's own mutation survives a rollback to N)"
    );

    // (2) session.jsonl proof: exactly one Rollback { target: 1 } record.
    let sessions_dir = xdg_data_home.join("rokr").join("sessions");
    let session_dir_entries: Vec<std::fs::DirEntry> = std::fs::read_dir(&sessions_dir)
        .unwrap_or_else(|err| {
            panic!("expected sessions directory to exist at {sessions_dir:?}, got error: {err:?}")
        })
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .collect();
    assert_eq!(
        session_dir_entries.len(),
        1,
        "expected exactly one ULID-named session directory under {sessions_dir:?}, got: {:?}",
        session_dir_entries
            .iter()
            .map(|entry| entry.path())
            .collect::<Vec<_>>()
    );
    let session_dir = session_dir_entries[0].path();
    let session_jsonl_contents = std::fs::read_to_string(session_dir.join("session.jsonl"))
        .expect("failed to read session.jsonl contents");
    let rollback_records: Vec<rokr_session::SessionRecord> = session_jsonl_contents
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<rokr_session::SessionRecord>(line).ok())
        .filter(|record| matches!(record, rokr_session::SessionRecord::Rollback { .. }))
        .collect();
    assert_eq!(
        rollback_records.len(),
        1,
        "expected exactly one Rollback record in session.jsonl, got: {rollback_records:?}"
    );
    match &rollback_records[0] {
        rokr_session::SessionRecord::Rollback { target } => {
            assert_eq!(*target, 1, "expected the Rollback record's target to be 1");
        }
        other => panic!("expected a Rollback record, got: {other:?}"),
    }

    // (3) Transcript-truncation proof: turn 3's actual outgoing request
    // body must still contain turns 0 and 1's prompts, but must NOT contain
    // turn 2's prompt.
    let received_requests = mock_server
        .received_requests()
        .await
        .expect("request recording should be enabled on the mock server by default");
    assert!(
        received_requests.len() >= 6,
        "expected at least 6 requests to /chat/completions (2 for turn0, 2 for turn1, 1 for \
         turn2, 1 for turn3), got {}: {received_requests:?}",
        received_requests.len()
    );
    let turn3_body =
        String::from_utf8_lossy(&received_requests[received_requests.len() - 1].body).into_owned();
    assert!(
        turn3_body.contains("turnzerowriteprompt"),
        "expected turn 3's request body to still contain turn 0's prompt (kept by rollback to \
         target 1), got: {turn3_body}"
    );
    assert!(
        turn3_body.contains("turnonewriteprompt"),
        "expected turn 3's request body to still contain turn 1's prompt (kept by rollback to \
         target 1), got: {turn3_body}"
    );
    assert!(
        !turn3_body.contains("turntwochatprompt"),
        "expected turn 3's request body to have turn 2's prompt discarded by rollback to \
         target 1, got: {turn3_body}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&xdg_data_home);
    let _ = std::fs::remove_dir_all(&temp_dir);
}

/// Ticket 38 (checkpoint-pre-images) deny-path regression test: a DENIED
/// write tool call must produce NO snapshot file and NO Checkpoint record --
/// mirrors `bash_tool_call_skips_execution_on_reject`'s reject-keypress
/// convention (`n` instead of `y`), extended to also assert the new
/// snapshot/Checkpoint side effects are absent, not just that the file
/// itself is untouched.
#[tokio::test]
async fn write_tool_call_skips_checkpoint_snapshot_and_record_on_reject() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let final_reply_text = "FinalReplyAfterWriteCheckpointRejectForTesting";

    let temp_dir = unique_temp_dir("write-checkpoint-reject-target");
    let target_file = temp_dir.join("checkpoint-reject-target.txt");
    let old_content = "preimagecontentbeforereject";
    let new_content = "postimagecontentafterreject";
    std::fs::write(&target_file, old_content).unwrap();
    let target_path = target_file.to_string_lossy().into_owned();

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-write-checkpoint-reject",
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

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-write-checkpoint-reject-final",
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

    let home = unique_temp_dir("home-write-checkpoint-reject");
    let xdg_config_home = unique_temp_dir("xdg-config-home-write-checkpoint-reject");
    let xdg_data_home = unique_temp_dir("xdg-data-home-write-checkpoint-reject");

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
    cmd.env("XDG_DATA_HOME", &xdg_data_home);
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
        .write_all(b"writecheckpointrejectfile\r")
        .expect("failed to write prompt to pty");

    let prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("write") && output.contains("-preimagecontentbeforereject") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("-preimagecontentbeforereject"),
        "expected pty output to contain the old-content diff line, got: {output:?}"
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
        "expected pty output to contain the final assistant reply after rejecting the write \
         tool call (the loop must continue, not crash), got: {output:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&target_file).unwrap(),
        old_content,
        "the file must not have been written after permission was rejected"
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

    let sessions_dir = xdg_data_home.join("rokr").join("sessions");
    let session_dir_entries: Vec<std::fs::DirEntry> = std::fs::read_dir(&sessions_dir)
        .unwrap_or_else(|err| {
            panic!("expected sessions directory to exist at {sessions_dir:?}, got error: {err:?}")
        })
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .collect();
    assert_eq!(
        session_dir_entries.len(),
        1,
        "expected exactly one ULID-named session directory under {sessions_dir:?}"
    );
    let session_dir = session_dir_entries[0].path();

    let snapshots_dir = session_dir.join("snapshots");
    assert!(
        !snapshots_dir.exists(),
        "a denied write must not create a snapshots directory at all, got: {snapshots_dir:?}"
    );

    let session_jsonl_contents =
        std::fs::read_to_string(session_dir.join("session.jsonl")).unwrap_or_default();
    let checkpoint_records: Vec<rokr_session::SessionRecord> = session_jsonl_contents
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<rokr_session::SessionRecord>(line).ok())
        .filter(|record| matches!(record, rokr_session::SessionRecord::Checkpoint { .. }))
        .collect();
    assert!(
        checkpoint_records.is_empty(),
        "a denied write must not append a Checkpoint record, got: {checkpoint_records:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&xdg_data_home);
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
    cmd.cwd(&temp_dir);
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

/// Ticket 61 (memory-file-loading-user-and-project-scope) acceptance test:
/// closely modeled on `agents_md_content_appears_in_outgoing_system_prompt`
/// above, but with a user-scope AGENTS.md (under
/// `$XDG_CONFIG_HOME/rokr/AGENTS.md` -- `default_config_dir`'s resolution)
/// ALSO present alongside the project-scope one. Both markers must appear in
/// the outgoing system prompt as separate segments, user-scope first.
#[tokio::test]
async fn user_and_project_scope_memory_both_appear_as_separate_segments_in_outgoing_system_prompt()
{
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    let canned_response = "MockedAssistantReplyForMemoryScopeTesting";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-memory-scope",
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

    let home = unique_temp_dir("home-memory-scope");
    let xdg_config_home = unique_temp_dir("xdg-config-home-memory-scope");
    let project_dir = unique_temp_dir("memory-scope-project");

    let user_marker = "DistinctiveUserScopeMemoryMarkerXYZ";
    let project_marker = "DistinctiveProjectScopeMemoryMarkerXYZ";

    // User scope: AGENTS.md under `$XDG_CONFIG_HOME/rokr/`, matching
    // `default_config_dir`'s `$XDG_CONFIG_HOME/rokr` resolution.
    let user_scope_config_dir = xdg_config_home.join("rokr");
    std::fs::create_dir_all(&user_scope_config_dir).unwrap();
    std::fs::write(user_scope_config_dir.join("AGENTS.md"), user_marker).unwrap();

    // Project scope: AGENTS.md under the project dir, same as the existing
    // agents_md_content_appears_in_outgoing_system_prompt test.
    std::fs::write(project_dir.join("AGENTS.md"), project_marker).unwrap();

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
        first_request_body.contains(user_marker),
        "expected the outgoing request body to contain the user-scope AGENTS.md marker, got: \
         {first_request_body}"
    );
    assert!(
        first_request_body.contains(project_marker),
        "expected the outgoing request body to contain the project-scope AGENTS.md marker, got: \
         {first_request_body}"
    );

    let user_marker_offset = first_request_body
        .find(user_marker)
        .expect("already asserted user_marker is present");
    let project_marker_offset = first_request_body
        .find(project_marker)
        .expect("already asserted project_marker is present");
    assert!(
        user_marker_offset < project_marker_offset,
        "expected the user-scope memory segment to appear BEFORE the project-scope segment in \
         the outgoing system prompt, got user offset {user_marker_offset} and project offset \
         {project_marker_offset} in: {first_request_body}"
    );

    assert!(
        first_request_body.contains("User memory"),
        "expected the outgoing request body to contain the 'User memory' segment label, got: \
         {first_request_body}"
    );
    assert!(
        first_request_body.contains("Project memory"),
        "expected the outgoing request body to contain the 'Project memory' segment label, got: \
         {first_request_body}"
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

    // RULING 2 done-when #1 (real PTY/integration path): the auto-compaction
    // that fired after turn 1 (raw_turn_count == 2 -> replaced_through == 0,
    // tail turn 1 retained) must have persisted exactly one Compaction record
    // carrying the RAW summary text (wrapper stripped).
    // This test doesn't set XDG_DATA_HOME, so session data lives under
    // $HOME/.local/share/rokr (default_data_dir's fallback).
    let sessions_dir = home.join(".local/share/rokr").join("sessions");
    let session_dir = std::fs::read_dir(&sessions_dir)
        .expect("sessions dir should exist")
        .filter_map(|e| e.ok())
        .find(|e| e.path().is_dir())
        .expect("expected one session directory")
        .path();
    let session_log = std::fs::read_to_string(session_dir.join("session.jsonl"))
        .expect("session.jsonl should exist");
    let compaction_records: Vec<rokr_session::SessionRecord> = session_log
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<rokr_session::SessionRecord>(line).ok())
        .filter(|record| matches!(record, rokr_session::SessionRecord::Compaction { .. }))
        .collect();
    // At least one Compaction record must have been persisted; the FIRST one
    // is the auto-compaction that fired after turn 1 (raw_turn_count == 2 ->
    // replaced_through == 0) carrying the RAW summary (wrapper stripped). A
    // subsequent turn may legitimately trigger a further compaction, so this
    // does not assert an exact count.
    assert!(
        !compaction_records.is_empty(),
        "expected at least one Compaction record persisted by auto-compaction"
    );
    match &compaction_records[0] {
        rokr_session::SessionRecord::Compaction {
            summary,
            replaced_through,
        } => {
            assert_eq!(
                *replaced_through, 0,
                "the first auto-compaction (after turn 1, raw_turn_count 2) -> replaced_through 0"
            );
            assert_eq!(
                summary, compaction_summary_text,
                "the persisted Compaction summary must be the RAW summary (wrapper stripped)"
            );
        }
        other => panic!("expected a Compaction record, got: {other:?}"),
    }

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

    // The compaction call fails outright. `build_provider` (ticket 32)
    // wraps every provider call -- this one included -- in
    // `ResilientProvider`, which retries a 5xx up to
    // `RetryPolicy::default().max_attempts` times before giving up. This
    // mock must stay 500 for all of those attempts: if it were capped at
    // fewer, the exhausted retries would fall through to the next mounted
    // mock (the unlimited `third_reply_text` 200 below) and the compaction
    // call would spuriously "succeed" instead of exhausting retries and
    // failing, so the failure notice this test asserts on would never be
    // emitted. Read from `RetryPolicy::default()` itself, not
    // hardcoded, so this stays correct if the policy's attempt count ever
    // changes.
    let compaction_failure_attempts = u64::from(rokr_provider::RetryPolicy::default().max_attempts);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(compaction_failure_attempts)
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

    // Wait for the command to actually finish — the "Transcript compacted."
    // confirmation `main.rs`'s `/compact` handler pushes once the async
    // compaction call resolves — not merely for "/compact" to be echoed
    // back. The echo appears the instant Enter is pressed, before the
    // command even starts running; racing ahead on it left `state.pending`
    // still true when the next prompt's keystrokes arrived, and the render
    // loop silently drops keystrokes typed while pending (see
    // `rokr-tui::event_loop`), so "thirdpromptunique" below could be typed
    // away before compaction ever completed.
    //
    // We match on "compacted." rather than the full "Transcript compacted."
    // phrase: the TUI's renderer diffs cells and skips repainting ones that
    // are already correct (e.g. a space that's already blank), emitting a
    // cursor-address escape instead of a literal space byte. That splits
    // "Transcript compacted." across an escape sequence in the raw PTY
    // stream, so the two-word phrase never appears as a contiguous
    // substring. "compacted." itself is one uninterrupted run of cells and
    // renders contiguously, and — like the rest of this file's assertions,
    // which all match single tokens for the same reason — is unambiguous
    // here (it doesn't collide with any other text the test produces, e.g.
    // "...BeforeCompactForTesting").
    let compact_confirmation_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < compact_confirmation_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("compacted.") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("compacted."),
        "expected pty output to contain the compaction completion confirmation after /compact, got: {output:?}"
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

/// Ticket 29 (model-session-switch): typing `/model anthropic` mid-session
/// switches which provider `submit` sends the *next* prompt to, without
/// touching `rokr.json` on disk. Proven by asserting the second prompt
/// lands on the Anthropic mock (not the OpenAI one the session started
/// with) and that the config file's bytes are unchanged before vs. after.
#[tokio::test]
async fn typing_model_command_switches_active_provider_for_next_turn() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let openai_mock_server = MockServer::start().await;
    let anthropic_mock_server = MockServer::start().await;

    let first_reply_text = "FirstReplyFromOpenAiForModelSwitchTesting";
    let second_reply_text = "SecondReplyFromAnthropicForModelSwitchTesting";

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
        .mount(&openai_mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "msg-test-second",
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "text", "text": second_reply_text }]
        })))
        .mount(&anthropic_mock_server)
        .await;

    let home = unique_temp_dir("home-model-command");
    let xdg_config_home = unique_temp_dir("xdg-config-home-model-command");

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
    cmd.env("ROKR_OPENAI_BASE_URL", openai_mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-openai-api-key");
    cmd.env("ROKR_ANTHROPIC_BASE_URL", anthropic_mock_server.uri());
    cmd.env("ROKR_ANTHROPIC_MODEL", "claude-3-5-sonnet-20241022");
    cmd.env("ROKR_ANTHROPIC_API_KEY", "test-anthropic-api-key");

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

    let config_file_path = xdg_config_home.join("rokr").join("rokr.json");
    let config_before = std::fs::read_to_string(&config_file_path)
        .expect("config file should exist after startup's load_or_init_default");

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
        "expected pty output to contain the first (OpenAI) mocked assistant response, got: {output:?}"
    );

    writer
        .write_all(b"/model anthropic\r")
        .expect("failed to write /model anthropic to pty");

    // Wait for the switch confirmation, not merely the echoed command — see
    // the `/compact` test's comment on this exact race. "switched." is a
    // single contiguous token that survives the diff-renderer's cell-skip
    // quirk and, unlike repeating the provider name in the confirmation
    // message would, never collides with the echoed "/model anthropic"
    // input line (which is why the confirmation message below is
    // deliberately name-agnostic).
    let switch_confirmation_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < switch_confirmation_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("switched.") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("switched."),
        "expected pty output to contain the model-switch confirmation after /model anthropic, got: {output:?}"
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
        "expected pty output to contain the second (Anthropic) mocked assistant response after switching providers, got: {output:?}"
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

    let openai_requests = openai_mock_server
        .received_requests()
        .await
        .expect("request recording should be enabled on the mock server by default");
    let anthropic_requests = anthropic_mock_server
        .received_requests()
        .await
        .expect("request recording should be enabled on the mock server by default");

    assert_eq!(
        openai_requests.len(),
        1,
        "expected exactly one request to the OpenAI mock (the first prompt only), got {}: {openai_requests:?}",
        openai_requests.len()
    );
    assert_eq!(
        anthropic_requests.len(),
        1,
        "expected exactly one request to the Anthropic mock (the second, post-switch prompt), got {}: {anthropic_requests:?}",
        anthropic_requests.len()
    );

    let config_after = std::fs::read_to_string(&config_file_path)
        .expect("config file should still exist after the session");
    assert_eq!(
        config_before, config_after,
        "expected rokr.json to be byte-identical before and after /model anthropic — the \
         active provider must never be persisted to disk"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

/// Ticket 36 (session-index-list-jump): `/sessions` lists prior sessions'
/// metadata as read from the shared `sessions/index.jsonl` file -- not by
/// scanning session directories. Hand-builds two fixture index entries
/// (via real `rokr_session::SessionIndexEntry` values + `serde_json`,
/// never a hand-typed JSON string, matching this file's established fixture
/// convention) directly into `index.jsonl` before spawning, then asserts
/// `/sessions`' PTY output contains each fixture's identifying fields.
#[test]
fn sessions_command_lists_prior_sessions_with_metadata() {
    let home = unique_temp_dir("home-sessions-list");
    let xdg_config_home = unique_temp_dir("xdg-config-home-sessions-list");
    let xdg_data_home = unique_temp_dir("xdg-data-home-sessions-list");

    let index_dir = xdg_data_home.join("rokr").join("sessions");
    std::fs::create_dir_all(&index_dir).expect("failed to create fixture sessions dir");

    let entry_alpha = rokr_session::SessionIndexEntry {
        session_id: "01FIXTURELISTSESSIONAAAA".to_string(),
        project_path: "/tmp/fixture-project-alpha".to_string(),
        created_at: "2026-07-20T00:00:00Z".to_string(),
        updated_at: "2026-07-20T00:05:00Z".to_string(),
        title: "alphasessiontitletoken".to_string(),
        turn_count: 3,
        last_model: "claude-fixture-alpha".to_string(),
    };
    let entry_beta = rokr_session::SessionIndexEntry {
        session_id: "01FIXTURELISTSESSIONBBBB".to_string(),
        project_path: "/tmp/fixture-project-beta".to_string(),
        created_at: "2026-07-20T01:00:00Z".to_string(),
        updated_at: "2026-07-20T01:02:00Z".to_string(),
        title: "betasessiontitletoken".to_string(),
        turn_count: 1,
        last_model: "gpt-fixture-beta".to_string(),
    };
    let fixture_contents = format!(
        "{}\n{}\n",
        serde_json::to_string(&entry_alpha).expect("serialize fixture entry alpha"),
        serde_json::to_string(&entry_beta).expect("serialize fixture entry beta"),
    );
    std::fs::write(index_dir.join("index.jsonl"), fixture_contents)
        .expect("failed to write fixture index.jsonl");

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
    cmd.env("XDG_DATA_HOME", &xdg_data_home);

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
        .write_all(b"/sessions\r")
        .expect("failed to write /sessions to pty");

    let listing_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < listing_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("alphasessiontitletoken") && output.contains("betasessiontitletoken") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("01FIXTURELISTSESSIONAAAA"),
        "expected /sessions output to contain session alpha's id, got: {output:?}"
    );
    assert!(
        output.contains("alphasessiontitletoken"),
        "expected /sessions output to contain session alpha's title, got: {output:?}"
    );
    assert!(
        output.contains("claude-fixture-alpha"),
        "expected /sessions output to contain session alpha's last model, got: {output:?}"
    );
    assert!(
        output.contains("01FIXTURELISTSESSIONBBBB"),
        "expected /sessions output to contain session beta's id, got: {output:?}"
    );
    assert!(
        output.contains("betasessiontitletoken"),
        "expected /sessions output to contain session beta's title, got: {output:?}"
    );
    assert!(
        output.contains("gpt-fixture-beta"),
        "expected /sessions output to contain session beta's last model, got: {output:?}"
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
    let _ = std::fs::remove_dir_all(&xdg_data_home);
}

/// Ticket 37 (session-search): `/search <term>` is a lazy, on-demand scan
/// of every session's own on-disk `session.jsonl` body (PRD decision 2) --
/// it must never consult `sessions/index.jsonl`, so a term that only
/// appears inside a `Compaction` summary (never surfaced in that cache)
/// must still be found. Hand-writes three session bodies directly under
/// `xdg_data_home/rokr/sessions/<id>/session.jsonl` (via real
/// `rokr_session::SessionRecord` values + `serde_json`, never a hand-typed
/// JSON string, matching this file's established fixture convention) --
/// deliberately does NOT write `index.jsonl` at all, so a search that
/// somehow depended on that cache would find nothing.
#[test]
fn search_command_returns_matching_session_ids_for_content_substring() {
    let home = unique_temp_dir("home-search");
    let xdg_config_home = unique_temp_dir("xdg-config-home-search");
    let xdg_data_home = unique_temp_dir("xdg-data-home-search");

    let sessions_dir = xdg_data_home.join("rokr").join("sessions");

    let turn_match_id = "searchturnmatchsession".to_string();
    let turn_match_dir = sessions_dir.join(&turn_match_id);
    std::fs::create_dir_all(&turn_match_dir).expect("failed to create turn-match session dir");
    let turn_match_records = vec![
        rokr_session::SessionRecord::Header {
            schema_version: 1,
            session_id: turn_match_id.clone(),
            created_at: "2026-07-20T00:00:00Z".to_string(),
            project_path: "/tmp/fixture-project-alpha".to_string(),
            agent_tier: "build".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-fixture".to_string(),
        },
        rokr_session::SessionRecord::Turn {
            messages: vec![rokr_core::Message::user_text(
                "please find zzyzxfindableterm in here",
            )],
            usage: rokr_session::UsageRecord {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            timestamp: "2026-07-20T00:00:01Z".to_string(),
        },
    ];
    let turn_match_contents = turn_match_records
        .iter()
        .map(|record| serde_json::to_string(record).expect("serialize fixture record"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(turn_match_dir.join("session.jsonl"), turn_match_contents)
        .expect("failed to write turn-match session.jsonl fixture");

    let compaction_match_id = "searchcompactionmatchsession".to_string();
    let compaction_match_dir = sessions_dir.join(&compaction_match_id);
    std::fs::create_dir_all(&compaction_match_dir)
        .expect("failed to create compaction-match session dir");
    let compaction_match_records = vec![
        rokr_session::SessionRecord::Header {
            schema_version: 1,
            session_id: compaction_match_id.clone(),
            created_at: "2026-07-20T01:00:00Z".to_string(),
            project_path: "/tmp/fixture-project-beta".to_string(),
            agent_tier: "build".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-fixture".to_string(),
        },
        rokr_session::SessionRecord::Turn {
            messages: vec![rokr_core::Message::user_text("unrelated live turn content")],
            usage: rokr_session::UsageRecord {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            timestamp: "2026-07-20T01:00:01Z".to_string(),
        },
        rokr_session::SessionRecord::Compaction {
            summary: "earlier discussion mentioned zzyzxfindableterm in passing".to_string(),
            replaced_through: 0,
        },
    ];
    let compaction_match_contents = compaction_match_records
        .iter()
        .map(|record| serde_json::to_string(record).expect("serialize fixture record"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(
        compaction_match_dir.join("session.jsonl"),
        compaction_match_contents,
    )
    .expect("failed to write compaction-match session.jsonl fixture");

    let no_match_id = "searchnomatchsession".to_string();
    let no_match_dir = sessions_dir.join(&no_match_id);
    std::fs::create_dir_all(&no_match_dir).expect("failed to create no-match session dir");
    let no_match_records = vec![
        rokr_session::SessionRecord::Header {
            schema_version: 1,
            session_id: no_match_id.clone(),
            created_at: "2026-07-20T02:00:00Z".to_string(),
            project_path: "/tmp/fixture-project-gamma".to_string(),
            agent_tier: "build".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-fixture".to_string(),
        },
        rokr_session::SessionRecord::Turn {
            messages: vec![rokr_core::Message::user_text(
                "completely unrelated content",
            )],
            usage: rokr_session::UsageRecord {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            timestamp: "2026-07-20T02:00:01Z".to_string(),
        },
    ];
    let no_match_contents = no_match_records
        .iter()
        .map(|record| serde_json::to_string(record).expect("serialize fixture record"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(no_match_dir.join("session.jsonl"), no_match_contents)
        .expect("failed to write no-match session.jsonl fixture");

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
    cmd.env("XDG_DATA_HOME", &xdg_data_home);

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
        .write_all(b"/search zzyzxfindableterm\r")
        .expect("failed to write /search to pty");

    let search_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < search_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(&turn_match_id) && output.contains(&compaction_match_id) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains(&turn_match_id),
        "expected /search output to contain the turn-match session id, got: {output:?}"
    );
    assert!(
        output.contains(&compaction_match_id),
        "expected /search output to contain the compaction-match session id (a term that \
         only appears inside a Compaction summary must still be found), got: {output:?}"
    );
    assert!(
        !output.contains(&no_match_id),
        "expected /search output to exclude the no-match session id, got: {output:?}"
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
    let _ = std::fs::remove_dir_all(&xdg_data_home);
}

/// Ticket 40 (prompt-history) acceptance test: a prompt submitted in one
/// process (Enter pressed on a non-empty prompt) is appended to the
/// cross-session prompt-history file at `$XDG_DATA_HOME/rokr/history` (PRD
/// decision 5) BEFORE that process exits; a second, entirely separate
/// process spawned afterward against the SAME `XDG_DATA_HOME` can recall it
/// by pressing Up in an empty prompt. This is deliberately NOT gated on the
/// mocked assistant reply landing -- `rokr-tui`'s history-append hook fires
/// synchronously at the Enter keypress, independent of whether the outgoing
/// provider call ever succeeds, so this test polls the history FILE
/// directly (mirroring
/// `resume_without_confirm_warns_and_confirm_swaps_transcript_and_writer`'s
/// same filesystem-polling convention) rather than waiting on pty output,
/// avoiding any race with the async submit call's own completion.
#[tokio::test]
async fn pressing_up_after_restart_recalls_previously_submitted_prompt_from_history_file() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    let canned_response = "MockedAssistantReplyForHistoryRecallTesting";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-history",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": canned_response },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-history");
    let xdg_config_home = unique_temp_dir("xdg-config-home-history");
    let xdg_data_home = unique_temp_dir("xdg-data-home-history");

    // Single token (no spaces), same reason as every other test in this
    // file: ratatui's diff-based redraw can leave cursor-jump gaps across
    // unchanged cells, so a literal multi-word substring match on raw pty
    // bytes can spuriously fail.
    let recalled_prompt = "recallthisuniqueprompttoken";

    // --- Run 1: submit the prompt, then wait for it to actually land in
    // the on-disk history file before exiting. ---
    {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("failed to open pty (run 1)");

        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
        cmd.env("HOME", &home);
        cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
        cmd.env("XDG_DATA_HOME", &xdg_data_home);
        cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
        cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
        cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .expect("failed to spawn rokr in pty (run 1)");
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .expect("failed to clone pty reader (run 1)");
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
            .expect("failed to take pty writer (run 1)");

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
            "expected pty output to contain Header (run 1), got: {output:?}"
        );

        writer
            .write_all(format!("{recalled_prompt}\r").as_bytes())
            .expect("failed to write prompt to pty (run 1)");

        let history_path = xdg_data_home.join("rokr").join("history");
        let write_deadline = Instant::now() + Duration::from_secs(10);
        let mut history_contents = String::new();
        while Instant::now() < write_deadline {
            history_contents = std::fs::read_to_string(&history_path).unwrap_or_default();
            if history_contents.contains(recalled_prompt) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            history_contents.contains(recalled_prompt),
            "expected the submitted prompt to be appended to {history_path:?}, got: \
             {history_contents:?}"
        );

        writer
            .write_all(b"\x03")
            .expect("failed to write Ctrl+C to pty (run 1)");
        let exit_deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child
                .try_wait()
                .expect("failed to poll rokr exit status (run 1)")
            {
                break status;
            }
            if Instant::now() > exit_deadline {
                let _ = child.kill();
                panic!("rokr (run 1) did not exit within timeout; output so far: {output:?}");
            }
            thread::sleep(Duration::from_millis(50));
        };
        assert!(
            status.success(),
            "expected rokr (run 1) to exit cleanly, got status: {status:?}"
        );
    }

    // --- Run 2: a fresh process against the SAME XDG_DATA_HOME. Pressing
    // Up in the (empty) prompt must recall run 1's submitted prompt. ---
    {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("failed to open pty (run 2)");

        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
        cmd.env("HOME", &home);
        cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
        cmd.env("XDG_DATA_HOME", &xdg_data_home);
        cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
        cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
        cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .expect("failed to spawn rokr in pty (run 2)");
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .expect("failed to clone pty reader (run 2)");
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
            .expect("failed to take pty writer (run 2)");

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
            "expected pty output to contain Header (run 2), got: {output:?}"
        );

        // Up-arrow: ESC [ A.
        writer
            .write_all(b"\x1b[A")
            .expect("failed to write Up arrow to pty (run 2)");

        let recall_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < recall_deadline {
            while let Ok(chunk) = rx.try_recv() {
                output.push_str(&String::from_utf8_lossy(&chunk));
            }
            if output.contains(recalled_prompt) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            output.contains(recalled_prompt),
            "expected pressing Up in an empty prompt (run 2, fresh process, same \
             XDG_DATA_HOME) to recall run 1's submitted prompt into the prompt buffer, got: \
             {output:?}"
        );

        writer
            .write_all(b"\x03")
            .expect("failed to write Ctrl+C to pty (run 2)");
        let exit_deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child
                .try_wait()
                .expect("failed to poll rokr exit status (run 2)")
            {
                break status;
            }
            if Instant::now() > exit_deadline {
                let _ = child.kill();
                panic!("rokr (run 2) did not exit within timeout; output so far: {output:?}");
            }
            thread::sleep(Duration::from_millis(50));
        };
        assert!(
            status.success(),
            "expected rokr (run 2) to exit cleanly, got status: {status:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&xdg_data_home);
}

/// Ticket 41 (multiline-input) acceptance test: composing a prompt across
/// two lines via Alt+Enter (crossterm's raw-PTY-portable equivalent of
/// Shift+Enter -- confirmed by reading crossterm 0.28.1's unix ANSI parser
/// at `event/sys/unix/parse.rs`: a lone `ESC` followed by any byte that
/// isn't `O`/`[`/`ESC` recursively parses that byte as a normal key and ORs
/// in `KeyModifiers::ALT`, so `\x1b\r` decodes to `KeyEvent { code: Enter,
/// modifiers: ALT }`; a bare `\r` -- which is all a real Shift+Enter sends
/// over a raw PTY without the kitty keyboard protocol enabled, which this
/// app doesn't opt into -- is indistinguishable from plain Enter at the
/// byte level, so Shift+Enter itself can't be driven from a PTY test; the
/// source under test checks `ALT || SHIFT` so a terminal that does send
/// SHIFT is still covered, just not exercised here), then pressing Enter,
/// must submit the WHOLE two-line buffer as a single outgoing request
/// rather than submitting on the first Alt+Enter.
///
/// Verified two ways:
/// 1. Exactly one request reaches the mock server (proving the first
///    Alt+Enter did not submit).
/// 2. That request's JSON body contains the two typed lines joined by a
///    literal newline (serialized as the two-character JSON escape `\n`),
///    proving the newline landed IN the submitted content rather than
///    being dropped or splitting the submission in two.
///
/// Single PTY spawn is enough here (unlike the two-process pattern in
/// `pressing_up_after_restart_recalls_previously_submitted_prompt_from_history_file`,
/// which specifically needed a restart to prove cross-session persistence).
#[tokio::test]
async fn multiline_prompt_composed_with_shift_enter_submits_as_single_prompt_on_enter() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    // Single tokens (no spaces), same reason as every other test in this
    // file: ratatui's diff-based redraw can leave cursor-jump gaps across
    // unchanged cells, so a literal multi-word substring match on raw pty
    // bytes can spuriously fail.
    let canned_response = "MockedAssistantReplyForMultilineInputTesting";
    let line_one = "linealphaunique";
    let line_two = "linebetaunique";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-multiline",
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

    let home = unique_temp_dir("home-multiline");
    let xdg_config_home = unique_temp_dir("xdg-config-home-multiline");
    let xdg_data_home = unique_temp_dir("xdg-data-home-multiline");

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
    cmd.env("XDG_DATA_HOME", &xdg_data_home);
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

    // Type line one, then Alt+Enter (`ESC` immediately followed by `\r` --
    // see the crossterm-decoding doc comment above) -- this must insert a
    // newline, NOT submit.
    writer
        .write_all(line_one.as_bytes())
        .expect("failed to write line one to pty");
    writer
        .write_all(b"\x1b\r")
        .expect("failed to write Alt+Enter to pty");

    // Type line two, then a plain Enter -- THIS must submit the whole
    // two-line buffer as a single prompt.
    writer
        .write_all(line_two.as_bytes())
        .expect("failed to write line two to pty");
    writer
        .write_all(b"\r")
        .expect("failed to write Enter to pty");

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
        output.contains(line_one),
        "expected pty output to contain the first typed line, got: {output:?}"
    );
    assert!(
        output.contains(line_two),
        "expected pty output to contain the second typed line, got: {output:?}"
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

    let received_requests = mock_server.received_requests().await.expect(
        "mock server should have recorded received requests \
         (make sure the request recorder wasn't disabled)",
    );
    assert_eq!(
        received_requests.len(),
        1,
        "expected exactly ONE outgoing request -- proving the first Alt+Enter did not \
         prematurely submit line one on its own -- got {} requests: {:?}",
        received_requests.len(),
        received_requests
            .iter()
            .map(|req| String::from_utf8_lossy(&req.body).into_owned())
            .collect::<Vec<_>>()
    );

    let request_body = String::from_utf8_lossy(&received_requests[0].body).into_owned();
    let expected_joined_content = format!("{line_one}\\n{line_two}");
    assert!(
        request_body.contains(&expected_joined_content),
        "expected the single outgoing request body to contain the two typed lines joined by a \
         literal newline (JSON-escaped as {expected_joined_content:?}), proving the whole \
         multi-line buffer was submitted as ONE prompt; got body: {request_body:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&xdg_data_home);
}

/// Ticket 42 (editor-integration) acceptance test: pressing the editor
/// keybinding (Ctrl+E) suspends the TUI, spawns the scripted `$EDITOR`
/// stand-in against a temp file containing the current prompt buffer, and
/// on the script's exit restores the terminal and loads the edited file's
/// contents back into the prompt buffer -- verified by submitting the
/// resulting buffer and checking the single outgoing request body contains
/// both the original typed text and the script's appended text, joined by
/// the newline the script inserts.
#[tokio::test]
async fn pressing_editor_keybinding_with_scripted_editor_command_updates_prompt_buffer_from_edited_file(
) {
    use std::os::unix::fs::PermissionsExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let canned_response = "MockedAssistantReplyForEditorIntegrationTesting";
    let typed_line = "linetypedbeforeeditoruniquetoken";
    let edited_line = "lineappendedbyscriptedituniquetoken";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-editor",
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

    let home = unique_temp_dir("home-editor");
    let xdg_config_home = unique_temp_dir("xdg-config-home-editor");
    let xdg_data_home = unique_temp_dir("xdg-data-home-editor");

    // Scripted `$EDITOR` stand-in: a tiny non-interactive shell script that
    // appends a known, unique line to whatever file path it's given (`$1`,
    // the temp file rokr-tui writes the current prompt buffer to), then
    // exits 0 immediately -- standing in for a real interactive editor
    // process without requiring one in CI.
    let editor_script_dir = unique_temp_dir("editor-script-editor");
    let editor_script_path = editor_script_dir.join("fake_editor.sh");
    std::fs::write(
        &editor_script_path,
        format!("#!/bin/sh\nprintf '\\n{edited_line}\\n' >> \"$1\"\n"),
    )
    .expect("failed to write fake editor script");
    let mut perms = std::fs::metadata(&editor_script_path)
        .expect("failed to stat fake editor script")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&editor_script_path, perms)
        .expect("failed to make fake editor script executable");

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
    cmd.env("XDG_DATA_HOME", &xdg_data_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");
    cmd.env(
        "EDITOR",
        editor_script_path
            .to_str()
            .expect("script path should be valid utf-8"),
    );

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
        .write_all(typed_line.as_bytes())
        .expect("failed to write typed line to pty");
    writer
        .write_all(b"\x05") // Ctrl+E
        .expect("failed to write Ctrl+E to pty");

    let edit_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < edit_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(edited_line) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(edited_line),
        "expected the prompt buffer to reflect the scripted editor's appended line after \
         Ctrl+E, got pty output: {output:?}"
    );

    writer
        .write_all(b"\r")
        .expect("failed to write Enter to pty");

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

    let received_requests = mock_server.received_requests().await.expect(
        "mock server should have recorded received requests \
         (make sure the request recorder wasn't disabled)",
    );
    assert_eq!(
        received_requests.len(),
        1,
        "expected exactly one outgoing request"
    );

    let request_body = String::from_utf8_lossy(&received_requests[0].body).into_owned();
    let expected_joined_content = format!("{typed_line}\\n{edited_line}");
    assert!(
        request_body.contains(&expected_joined_content),
        "expected the outgoing request body to contain the typed line and the scripted \
         editor's appended line joined by a literal newline (JSON-escaped as \
         {expected_joined_content:?}), proving the edited file's contents were loaded back \
         into the prompt buffer; got body: {request_body:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&xdg_data_home);
    let _ = std::fs::remove_dir_all(&editor_script_dir);
}

/// Ticket 43 (mouse-scroll-status-line) acceptance test: `TerminalGuard`
/// enables mouse capture on startup (asserted via the raw SGR-mode-enable
/// escape sequence crossterm's `EnableMouseCapture` writes), and once the
/// View pane's transcript exceeds one screen, sending raw SGR mouse-wheel
/// escape sequences over the PTY moves the scrollback offset -- revealing
/// content that was previously scrolled off the top of the pane -- without
/// requiring any keypress.
#[tokio::test]
async fn mouse_wheel_scroll_moves_view_offset_in_running_session() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    // 40 short, unique, whitespace-free tokens -- long enough that, once
    // rendered bottom-anchored in an ~16-row-tall View pane (24 pty rows -
    // 3-row Header - 3-row Prompt - 4 border rows), the earliest lines are
    // genuinely clipped off-screen and never written to the pty at all
    // until scrolled into view.
    //
    // Deliberately built from 40 DISTINCT REPEATED CHARACTERS (`"aaaa..."`,
    // `"bbbb..."`, ...) rather than a shared "scrollline" stem + a 2-digit
    // suffix (which is what this looked like originally): ratatui's
    // `CrosstermBackend` diffs the previous and next `Buffer` cell-by-cell
    // and only emits escape codes for the cells that actually changed. Two
    // lines sharing a common prefix (e.g. "scrollline03" -> "scrollline00")
    // only differ in their last digit, so scrolling from one into view over
    // another only ever rewrites that one differing cell -- the shared
    // "scrollline0" prefix is never retransmitted, so the full string
    // "scrollline00" never appears anywhere in the raw PTY byte stream even
    // though it is genuinely visible on screen (verified independently by
    // replaying a captured session through a `pyte`/vt100 terminal
    // emulator). Since every one of the 40 lines here uses an entirely
    // different repeated letter, ANY two of them differ at EVERY column,
    // so scrolling one into a row previously occupied by another always
    // forces a full-row rewrite, making `output.contains(...)` a reliable
    // check again.
    let letters: Vec<char> = ('a'..='z').chain('A'..='N').collect(); // 26 + 14 = 40
    let long_reply: String = letters
        .iter()
        .map(|c| c.to_string().repeat(12))
        .collect::<Vec<_>>()
        .join("\n");

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-scroll",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": long_reply
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-mouse-scroll");
    let xdg_config_home = unique_temp_dir("xdg-config-home-mouse-scroll");

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
        output.contains("?1006h"),
        "expected TerminalGuard to enable SGR mouse capture on startup \
         (crossterm's EnableMouseCapture sequence), got: {output:?}"
    );

    writer
        .write_all(b"gimme the list\r")
        .expect("failed to write prompt to pty");

    let last_line_owned = letters[39].to_string().repeat(12);
    let last_line = last_line_owned.as_str();
    let first_line_owned = letters[0].to_string().repeat(12);
    let first_line = first_line_owned.as_str();

    let response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(last_line) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(last_line),
        "expected the bottom-anchored View pane to show the tail of the long \
         reply, got: {output:?}"
    );
    assert!(
        !output.contains(first_line),
        "expected the earliest line of the long reply to be scrolled off \
         the top of the View pane before any scrolling, got: {output:?}"
    );

    // Raw SGR mouse-wheel-up escape sequences (crossterm 0.28's parser:
    // Cb=64 -> MouseEventKind::ScrollUp), sent 12 times (x3 lines/tick =
    // 36 lines) -- comfortably enough to walk the whole 40-line transcript
    // back into view given the pane's ~16-row inner height.
    let scroll_up = b"\x1b[<64;10;10M";
    for _ in 0..12 {
        writer
            .write_all(scroll_up)
            .expect("failed to write mouse scroll-up sequence to pty");
    }

    let scroll_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < scroll_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(first_line) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(first_line),
        "expected mouse-wheel scroll-up to reveal the earliest line of the \
         transcript, got: {output:?}"
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

/// Ticket 43 (mouse-scroll-status-line) acceptance test: once a turn's
/// usage is reported, the Header block renders a context-usage percentage
/// computed as `(input_tokens + output_tokens) / context_window_size`. The
/// default `context_window_size` is 200_000 (rokr-config's default), so a
/// mocked reply with prompt_tokens=90_000/completion_tokens=10_000 yields
/// exactly 50%.
#[tokio::test]
async fn header_shows_context_usage_percentage_after_a_turn_completes() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    let canned_response = "MockedReplyForContextPercentTest";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-context-percent",
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
            ],
            "usage": {
                "prompt_tokens": 90000,
                "completion_tokens": 10000,
                "total_tokens": 100000
            }
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-context-percent");
    let xdg_config_home = unique_temp_dir("xdg-config-home-context-percent");

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
        !output.contains("50%"),
        "expected no context-usage percentage before any turn completes, got: {output:?}"
    );

    writer
        .write_all(b"trigger the turn\r")
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
        "expected the mocked assistant reply to render, got: {output:?}"
    );

    let percent_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < percent_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("50%") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("50%"),
        "expected the Header to render a 50% context-usage figure after the \
         turn's usage (90_000 + 10_000 input+output tokens over the default \
         200_000 context_window_size) was reported, got: {output:?}"
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

/// The raw pty byte stream accumulated by these tests is a cell-diffed
/// terminal UPDATE log, not a screen snapshot: ratatui only re-transmits
/// cells that actually changed since the previous frame. The Header's
/// status line is `"{elapsed} | context {percent}%"` -- the single space
/// between the literal word "context" and the percentage figure is
/// frequently an unchanged cell (it was already a space in the prior
/// frame), so it gets skipped by the diff and a cursor-repositioning
/// escape sequence is emitted in its place instead of the space byte
/// itself. That means a literal contiguous `"context 50%"` (or `"context
/// 0%"`) substring essentially never appears in the raw byte stream, even
/// when that's exactly what's rendered on screen. What DOES appear
/// contiguously is the percentage token itself (e.g. `"50%"`, `"0%"`),
/// since every one of ITS cells changes together and is written as one
/// run. A bare substring search for `"0%"` is unsafe on its own though --
/// `"50%"` itself ends in the literal characters `"0%"` -- so this checks
/// for a `"0%"` occurrence that is NOT immediately preceded by a `'5'`
/// (which is how "50%"'s own tail would appear).
fn contains_bare_zero_percent(haystack: &str) -> bool {
    haystack
        .match_indices("0%")
        .any(|(idx, _)| idx == 0 || !haystack[..idx].ends_with('5'))
}

/// F-011 (argus review, phase-5-session-management): a turn whose reported
/// usage is all-zero (some OpenAI-compatible proxies intermittently omit
/// real usage figures rather than reporting them honestly) must not reset
/// the Header's context-usage percentage to 0% -- main.rs's `submit`
/// closure should fall back to the last REAL non-zero usage figure instead.
#[tokio::test]
async fn zero_usage_turn_leaves_context_percentage_at_previous_known_value() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    let first_reply_text = "FirstReplyWithRealUsageForZeroUsageTest";
    let second_reply_text = "SecondReplyWithZeroUsageForZeroUsageTest";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-real-usage",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": first_reply_text },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 90000, "completion_tokens": 10000, "total_tokens": 100000 }
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-zero-usage",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": second_reply_text },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 }
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-zero-usage-percent");
    let xdg_config_home = unique_temp_dir("xdg-config-home-zero-usage-percent");

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

    writer
        .write_all(b"firstpromptzerousagetest\r")
        .expect("failed to write first prompt to pty");

    let first_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < first_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(first_reply_text) && output.contains("50%") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(first_reply_text),
        "expected the first mocked reply to render, got: {output:?}"
    );
    assert!(
        output.contains("50%"),
        "expected the Header to show a 50% context-usage figure after the first turn's real \
         usage was reported, got: {output:?}"
    );

    writer
        .write_all(b"secondpromptzerousagetest\r")
        .expect("failed to write second prompt to pty");

    let second_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < second_deadline {
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
        "expected the second mocked reply (all-zero usage) to render, got: {output:?}"
    );

    // Give the header a moment to settle on whatever percentage it's going
    // to render post-second-turn before asserting on it.
    thread::sleep(Duration::from_millis(300));
    while let Ok(chunk) = rx.try_recv() {
        output.push_str(&String::from_utf8_lossy(&chunk));
    }

    assert!(
        !contains_bare_zero_percent(&output),
        "expected the Header to NEVER render a bare 0% context-usage figure -- a zero-usage \
         turn should fall back to the last known real usage rather than resetting the \
         percentage, got: {output:?}"
    );
    assert!(
        output.contains("50%"),
        "expected the Header to still show a 50% context-usage figure after the zero-usage \
         second turn, got: {output:?}"
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

/// The fake stdio MCP server fixture's fixed `tools/call` response text
/// (`crates/rokr-mcp/tests/fixtures/fake_mcp_server.rs::FIXED_RESPONSE_TEXT`).
/// A bin target's items aren't importable from another crate, so this is a
/// deliberate duplicate literal, not a shared constant -- keep both sides
/// in sync if either changes.
const FAKE_MCP_SERVER_FIXED_RESPONSE_TEXT: &str = "fake-mcp-server-echo-response-9f3c2a";

/// The fake stdio MCP server fixture's `[[bin]]` executable
/// (`crates/rokr-mcp/Cargo.toml`) lives in a sibling crate, so
/// `CARGO_BIN_EXE_fake_mcp_server` (only populated for bin targets of the
/// package under test) isn't available here. The whole workspace shares
/// one `target/<profile>/` directory, so deriving the path from the
/// already-available `CARGO_BIN_EXE_rokr` (same directory, different
/// filename) is reliable without depending on that env var. Requires the
/// fixture to have actually been built -- true whenever the fixture ran as
/// part of a full workspace `cargo test`, which is how this suite is meant
/// to be run.
fn fake_mcp_server_path() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_BIN_EXE_rokr"));
    path.set_file_name(if cfg!(windows) {
        "fake_mcp_server.exe"
    } else {
        "fake_mcp_server"
    });
    assert!(
        path.exists(),
        "fake_mcp_server fixture binary not found at {path:?} -- run a full workspace \
         `cargo test` (not `cargo test -p rokr` alone) so rokr-mcp's [[bin]] fixture gets built"
    );
    path
}

/// Writes a `rokr.json` declaring one ENABLED stdio MCP server into
/// `xdg_config_home/rokr/rokr.json` (`rokr_config::default_config_dir`'s
/// resolution when `XDG_CONFIG_HOME` is set) BEFORE the `rokr` binary is
/// spawned, so `load_or_init` takes its "existing file" branch and parses
/// this real `mcp` block -- ticket 45 (mcp-config-and-lifecycle) replaces
/// ticket 44's `ROKR_MCP_SERVER` env-var wiring with exactly this
/// config-driven path. Thin wrapper over `write_mcp_config_servers` below
/// (ticket 46, mcp-namespace-multi-server-freeze) for the common
/// one-server case every earlier test uses.
fn write_mcp_config(
    xdg_config_home: &std::path::Path,
    server_name: &str,
    command: &std::path::Path,
    env: serde_json::Value,
) {
    write_mcp_config_servers(xdg_config_home, &[(server_name, command, env)]);
}

/// Ticket 46 (mcp-namespace-multi-server-freeze): same as `write_mcp_config`
/// above, but declares MULTIPLE enabled stdio MCP servers in one
/// `rokr.json` -- needed for the two-server namespacing/collision
/// acceptance test, which must configure two independent fake servers in a
/// single session.
fn write_mcp_config_servers(
    xdg_config_home: &std::path::Path,
    servers: &[(&str, &std::path::Path, serde_json::Value)],
) {
    let config_dir = xdg_config_home.join("rokr");
    std::fs::create_dir_all(&config_dir).expect("failed to create rokr config dir for test");

    let mut mcp = serde_json::Map::new();
    for (server_name, command, env) in servers {
        mcp.insert(
            server_name.to_string(),
            serde_json::json!({
                "transport": {
                    "stdio": {
                        "command": command.to_string_lossy(),
                        "args": [],
                        "env": env
                    }
                },
                "enabled": true
            }),
        );
    }
    let config = serde_json::json!({ "version": 1, "mcp": mcp });

    std::fs::write(
        config_dir.join("rokr.json"),
        serde_json::to_string_pretty(&config).expect("failed to serialize test rokr.json"),
    )
    .expect("failed to write test rokr.json");
}

/// Ticket 50 (hooks-remaining-events-and-config): writes a `rokr.json`
/// declaring exactly one hook entry for `event` into
/// `xdg_config_home/rokr/rokr.json` BEFORE the `rokr` binary is spawned,
/// mirroring `write_mcp_config` above (same "existing file" `load_or_init`
/// branch, same user-scope-only trust boundary this ticket's config schema
/// shares with `mcp`'s). `command` is a real shell command string (run via
/// `sh -c`, same as `execute_hook`), not a script path -- matching how
/// `pretooluse_hook_script_denies_bash_call_before_permission_prompt_appears`
/// already inlines its hook logic as a one-line shell command rather than a
/// separate fixture file.
fn write_hooks_config(xdg_config_home: &std::path::Path, event: &str, command: &str) {
    let config_dir = xdg_config_home.join("rokr");
    std::fs::create_dir_all(&config_dir).expect("failed to create rokr config dir for test");

    let config = serde_json::json!({
        "version": 1,
        "hooks": {
            event: [
                { "command": command }
            ]
        }
    });

    std::fs::write(
        config_dir.join("rokr.json"),
        serde_json::to_string_pretty(&config).expect("failed to serialize test rokr.json"),
    )
    .expect("failed to write test rokr.json");
}

/// Ticket 44 (mcp-tracer-bullet) acceptance test: a model tool-call to a
/// fake stdio MCP server's tool, driven end-to-end through the running
/// rokr binary, produces a real `ToolResult` via the new `McpTool:
/// ExecutableTool` adapter after a permission prompt is accepted. Same PTY
/// + wiremock harness as `bash_tool_call_renders_permission_prompt_and_runs_on_accept`
/// above, with one added strictness: the SECOND mock (the provider's
/// post-tool-call reply) only matches if the outgoing request body
/// actually contains the fixture's fixed response text -- so unless
/// `McpTool::execute_boxed` really carried that exact text from the real
/// `rmcp` client, through the permission-gated tool loop, into the
/// `ToolResult` sent back on the wire, no mock matches, the loop errors,
/// and the PTY never renders `final_reply_text` -- the test times out and
/// fails instead of silently passing on a stub.
#[tokio::test]
async fn mcp_tool_call_renders_permission_prompt_and_returns_result_from_fake_stdio_server() {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let final_reply_text = "FinalReplyAfterMcpAcceptForTesting";

    // Ticket 44's interim wiring (crates/rokr/src/main.rs) hardcodes the
    // server name "interim" for the one env-var-configured MCP server;
    // `rokr_mcp::qualified_name` is the SAME function that wiring calls,
    // so this can't drift from what main.rs actually computes.
    let qualified_tool_name = rokr_mcp::qualified_name("interim", "echo");

    // First call: the model asks to invoke the MCP tool.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-mcp-accept",
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
                                    "name": qualified_tool_name,
                                    "arguments": serde_json::json!({ "message": "hi" }).to_string()
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

    // Second call: only matches if the tool result the loop fed back
    // actually contains the fixture's real response text -- see this
    // test's doc comment.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains(FAKE_MCP_SERVER_FIXED_RESPONSE_TEXT))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-mcp-accept-final",
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

    let home = unique_temp_dir("home-mcp-accept");
    let xdg_config_home = unique_temp_dir("xdg-config-home-mcp-accept");
    // Ticket 45 (mcp-config-and-lifecycle): configured via rokr.json now,
    // not the ROKR_MCP_SERVER env var ticket 44 used -- the server name
    // stays "interim" since `qualified_tool_name` above is computed from
    // it.
    write_mcp_config(
        &xdg_config_home,
        "interim",
        &fake_mcp_server_path(),
        serde_json::json!({}),
    );

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
        .write_all(b"call the mcp tool\r")
        .expect("failed to write prompt to pty");

    // Wait for the permission prompt to render, showing the qualified MCP
    // tool name -- every MCP call is gated (`McpTool::preview` always
    // returns `Some(...)`), before we've granted anything.
    let prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(&qualified_tool_name) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains(&qualified_tool_name),
        "expected pty output to contain the qualified MCP tool name '{qualified_tool_name}' in \
         a permission prompt, got: {output:?}"
    );
    assert!(
        !output.contains(FAKE_MCP_SERVER_FIXED_RESPONSE_TEXT),
        "the MCP tool must not have run before permission was granted, but its response text \
         already appears in the output: {output:?}"
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
        "expected pty output to contain the final assistant reply after accepting the MCP tool \
         call, got: {output:?}"
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

/// Ticket 48 (mcp-http-transport) stretch-scope: the `tools/call` fixed
/// response text a fake Streamable HTTP MCP server (below) returns.
/// Distinct from `FAKE_MCP_SERVER_FIXED_RESPONSE_TEXT` (the stdio
/// fixture's) so a test can't accidentally pass by matching the wrong
/// transport's marker.
const FAKE_HTTP_MCP_SERVER_FIXED_RESPONSE_TEXT: &str = "fake-http-mcp-server-echo-response-7e1b4d";

/// Ticket 48: a minimal wiremock-backed fake Streamable HTTP MCP server,
/// duplicated (not shared) from `rokr-mcp`'s own copy of this responder --
/// the two crates' test doubles can't share code across the crate
/// boundary, mirroring this file's existing
/// `FAKE_MCP_SERVER_FIXED_RESPONSE_TEXT` duplicate-literal precedent.
/// Mirrors `tests/fixtures/fake_mcp_server.rs`'s JSON-RPC result shapes
/// exactly, replying over HTTP instead of stdio. Only handles POST --
/// `StreamableHttpClientTransportConfig::allow_stateless` defaults to
/// `true` and no `Mcp-Session-Id` response header is set below, so the
/// real `rmcp` client never opens a GET/SSE stream.
struct FakeHttpMcpResponder;

impl wiremock::Respond for FakeHttpMcpResponder {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let body: serde_json::Value = request.body_json().unwrap_or(serde_json::Value::Null);
        let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let Some(id) = body.get("id").cloned() else {
            // A notification (e.g. `notifications/initialized`) gets no
            // JSON-RPC reply -- 202 Accepted with no body is what
            // `post_message`'s reqwest impl treats as
            // `StreamableHttpPostResponse::Accepted`.
            return wiremock::ResponseTemplate::new(202);
        };
        let result = match method {
            "initialize" => serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "fake-http-mcp-server", "version": "0.1.0" }
            }),
            "tools/list" => serde_json::json!({
                "tools": [
                    {
                        "name": "echo",
                        "description": "Echoes back a fixed marker string.",
                        "inputSchema": {
                            "type": "object",
                            "properties": { "message": { "type": "string" } }
                        }
                    }
                ]
            }),
            "tools/call" => serde_json::json!({
                "content": [
                    { "type": "text", "text": FAKE_HTTP_MCP_SERVER_FIXED_RESPONSE_TEXT }
                ],
                "isError": false
            }),
            _ => serde_json::json!({}),
        };
        wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        }))
    }
}

/// Writes a `rokr.json` declaring one ENABLED Streamable HTTP MCP server
/// (ticket 48) into `xdg_config_home/rokr/rokr.json` -- the `http`
/// transport variant's real config shape (`{"url": "...", "headers":
/// {...}}"`), analogous to `write_mcp_config`'s stdio block above.
fn write_http_mcp_config(
    xdg_config_home: &std::path::Path,
    server_name: &str,
    url: &str,
    bearer_token: &str,
) {
    let config_dir = xdg_config_home.join("rokr");
    std::fs::create_dir_all(&config_dir).expect("failed to create rokr config dir for test");

    let config = serde_json::json!({
        "version": 1,
        "mcp": {
            server_name: {
                "transport": {
                    "http": {
                        "url": url,
                        "headers": { "Authorization": format!("Bearer {bearer_token}") }
                    }
                },
                "enabled": true
            }
        }
    });

    std::fs::write(
        config_dir.join("rokr.json"),
        serde_json::to_string_pretty(&config).expect("failed to serialize test rokr.json"),
    )
    .expect("failed to write test rokr.json");
}

/// Ticket 48 (mcp-http-transport) acceptance test -- PRD "MCP permissions":
/// a configured Streamable HTTP MCP server with a static bearer token
/// completes `initialize` and exposes tools identically to a stdio server
/// (same `McpTool`/permission-gated path as
/// `mcp_tool_call_renders_permission_prompt_and_returns_result_from_fake_stdio_server`
/// above), with the ADDED strictness that the permission prompt text must
/// also contain the HTTP server's origin -- a data-exfiltration signal a
/// stdio server's prompt never carries. The fake MCP server's mock only
/// matches requests carrying the configured bearer header, so a missing
/// header on ANY request in the initialize/tools-list/tools-call sequence
/// 404s and the test times out instead of silently passing.
#[tokio::test]
async fn http_mcp_server_tool_call_surfaces_origin_in_permission_prompt() {
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let provider_mock_server = MockServer::start().await;
    let mcp_mock_server = MockServer::start().await;

    let final_reply_text = "FinalReplyAfterHttpMcpAcceptForTesting";
    let bearer_token = "test-static-bearer-token-for-http-mcp";
    let qualified_tool_name = rokr_mcp::qualified_name("remote", "echo");

    // The fake HTTP MCP server: only responds to a request carrying the
    // configured static bearer token -- proves the token is sent, not
    // just configured.
    Mock::given(method("POST"))
        .and(header("authorization", format!("Bearer {bearer_token}")))
        .respond_with(FakeHttpMcpResponder)
        .mount(&mcp_mock_server)
        .await;

    // First provider call: the model asks to invoke the HTTP MCP tool.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-http-mcp-accept",
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
                                    "name": qualified_tool_name,
                                    "arguments": serde_json::json!({ "message": "hi" }).to_string()
                                }
                            }
                        ]
                    },
                    "finish_reason": "tool_calls"
                }
            ]
        })))
        .up_to_n_times(1)
        .mount(&provider_mock_server)
        .await;

    // Second provider call: only matches if the tool result the loop fed
    // back actually contains the fake HTTP MCP server's real response
    // text -- proving the HTTP-transport tool call really ran end-to-end.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains(
            FAKE_HTTP_MCP_SERVER_FIXED_RESPONSE_TEXT,
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-http-mcp-accept-final",
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
        .mount(&provider_mock_server)
        .await;

    let home = unique_temp_dir("home-http-mcp-accept");
    let xdg_config_home = unique_temp_dir("xdg-config-home-http-mcp-accept");
    write_http_mcp_config(
        &xdg_config_home,
        "remote",
        &mcp_mock_server.uri(),
        bearer_token,
    );

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
    cmd.env("ROKR_OPENAI_BASE_URL", provider_mock_server.uri());
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
        .write_all(b"call the mcp tool\r")
        .expect("failed to write prompt to pty");

    // Wait for the permission prompt to render, showing the qualified MCP
    // tool name -- every MCP call is gated (`McpTool::preview` always
    // returns `Some(...)`), before we've granted anything.
    let prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(&qualified_tool_name) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains(&qualified_tool_name),
        "expected pty output to contain the qualified MCP tool name '{qualified_tool_name}' in \
         a permission prompt, got: {output:?}"
    );
    // PRD "MCP permissions": a remote (HTTP) server's origin is a
    // data-exfiltration signal, so it's surfaced in the permission-prompt
    // text -- a stdio server's prompt never carries this. The wiremock
    // server's own URI IS that origin, so this is the strictest possible
    // check: the exact configured URL, not just some origin-shaped text.
    let mcp_origin = mcp_mock_server.uri();
    assert!(
        output.contains(&mcp_origin),
        "expected pty output to contain the HTTP MCP server's origin '{mcp_origin}' in the \
         permission prompt, got: {output:?}"
    );
    assert!(
        !output.contains(FAKE_HTTP_MCP_SERVER_FIXED_RESPONSE_TEXT),
        "the HTTP MCP tool must not have run before permission was granted, but its response \
         text already appears in the output: {output:?}"
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
        "expected pty output to contain the final assistant reply after accepting the HTTP MCP \
         tool call, got: {output:?}"
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

/// Ticket 45 (mcp-config-and-lifecycle) acceptance test: a `rokr.json` with
/// one enabled stdio server (configured via the real `mcp` config schema,
/// not ticket 44's `ROKR_MCP_SERVER` env var) produces that server's tools
/// after startup, and first paint (Header/View/Prompt rendering) is not
/// delayed by MCP startup -- the render loop appears well before the model
/// ever gets a chance to call the MCP tool, since submitting the prompt
/// that triggers the tool call is itself gated on first paint having
/// already happened.
#[tokio::test]
async fn mcp_server_configured_via_rokr_json_appears_in_tool_set_after_startup() {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let final_reply_text = "FinalReplyAfterConfigDrivenMcpForTesting";
    let qualified_tool_name = rokr_mcp::qualified_name("scripted", "echo");

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-mcp-config",
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
                                    "name": qualified_tool_name,
                                    "arguments": serde_json::json!({ "message": "hi" }).to_string()
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

    // Only matches if the tool result the loop fed back actually contains
    // the fixture's real response text -- proving the configured server's
    // real tool ran, not a stub.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains(FAKE_MCP_SERVER_FIXED_RESPONSE_TEXT))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-mcp-config-final",
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

    let home = unique_temp_dir("home-mcp-config");
    let xdg_config_home = unique_temp_dir("xdg-config-home-mcp-config");
    write_mcp_config(
        &xdg_config_home,
        "scripted",
        &fake_mcp_server_path(),
        serde_json::json!({}),
    );

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
    // First paint must not be delayed by MCP startup: this deadline is the
    // SAME bound used everywhere else in this file for a render that has
    // no MCP server configured at all -- if MCP init were on the render
    // path, a real (if fast) subprocess spawn + initialize handshake would
    // still show up as added latency here.
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
        .write_all(b"call the mcp tool\r")
        .expect("failed to write prompt to pty");

    let prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(&qualified_tool_name) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(&qualified_tool_name),
        "expected the configured server's tool ('{qualified_tool_name}') to be in the \
         submitted turn's tool set (rendered in the permission prompt), got: {output:?}"
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
        "expected pty output to contain the final assistant reply, got: {output:?}"
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

/// Ticket 45 (mcp-config-and-lifecycle) acceptance test: a configured stdio
/// server that exits before ever responding to `initialize` contributes
/// zero tools, a one-line status notice becomes visible in the header
/// status line, and the session is otherwise unaffected -- first paint
/// still succeeds promptly and an ordinary (non-MCP) turn still completes.
#[tokio::test]
async fn failed_mcp_server_shows_status_notice_and_session_continues_without_its_tools() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    let ordinary_reply_text = "OrdinaryReplyAfterMcpFailureForTesting";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-mcp-failure",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": ordinary_reply_text
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-mcp-failure");
    let xdg_config_home = unique_temp_dir("xdg-config-home-mcp-failure");
    // The `FAKE_MCP_SERVER_FAIL_INIT` env var (rokr-mcp/tests/fixtures/
    // fake_mcp_server.rs) makes the fixture exit immediately instead of
    // responding to `initialize` -- exercising the genuine failed-handshake
    // path, not a faked JSON-RPC error.
    write_mcp_config(
        &xdg_config_home,
        "flaky",
        &fake_mcp_server_path(),
        serde_json::json!({ "FAKE_MCP_SERVER_FAIL_INIT": "1" }),
    );

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
    // First paint must still succeed promptly even though the configured
    // server is about to fail -- proving startup isn't blocked/wedged by
    // it.
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

    // The bounded retry+backoff in rokr_mcp::run_lifecycle adds real
    // wall-clock delay (a handful of short backoffs) before the notice
    // fires -- well under this 10s deadline.
    //
    // Checked as separate single-word tokens, not one contiguous phrase:
    // the header Paragraph renders unwrapped, and ratatui/crossterm's
    // cell-diff rendering skips redrawing a cell whose content is
    // unchanged from the previous frame (e.g. a space at a column that was
    // already blank) -- so a multi-word phrase like "failed to start" gets
    // its inter-word spaces skipped and its cursor hopped between words,
    // meaning it never appears as one contiguous run of bytes in the raw
    // PTY stream even though it's genuinely rendered on screen (see the
    // `/model anthropic` test's own comment on this exact quirk, elsewhere
    // in this file, for precedent).
    let notice_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < notice_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("flaky") && output.contains("failed") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("flaky") && output.contains("failed"),
        "expected a one-line status notice mentioning the failed server 'flaky', got: {output:?}"
    );

    // The session must otherwise still work: an ordinary, non-MCP turn
    // completes normally.
    writer
        .write_all(b"say something ordinary\r")
        .expect("failed to write prompt to pty");

    let response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(ordinary_reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(ordinary_reply_text),
        "expected the session to complete an ordinary turn despite the failed MCP server, \
         got: {output:?}"
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

/// Ticket 51 (mcp-hooks-introspection) acceptance test: `/mcp` lists every
/// configured server's connection state and (for a connected server) its
/// current tool list -- one server reaches `Ready` normally, the other is
/// configured with `FAKE_MCP_SERVER_FAIL_INIT` (same knob
/// `failed_mcp_server_shows_status_notice_and_session_continues_without_its_tools`
/// above uses) so it exhausts its bounded retry and lands on `Degraded`.
/// No live model turn is needed here -- MCP servers spawn unconditionally
/// at startup regardless of agent tier (`main.rs`), so this is a plain PTY
/// command-dispatch check, mirroring `/sessions`'s/`/search`'s own tests
/// rather than the wiremock-backed MCP tool-call tests elsewhere in this
/// file.
#[test]
fn mcp_command_lists_servers_connection_state_and_tools() {
    let home = unique_temp_dir("home-mcp-list");
    let xdg_config_home = unique_temp_dir("xdg-config-home-mcp-list");

    write_mcp_config_servers(
        &xdg_config_home,
        &[
            ("healthy", &fake_mcp_server_path(), serde_json::json!({})),
            (
                "flaky",
                &fake_mcp_server_path(),
                serde_json::json!({ "FAKE_MCP_SERVER_FAIL_INIT": "1" }),
            ),
        ],
    );

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

    // Give the flaky server's bounded retry+backoff time to exhaust into
    // Degraded before asking for the listing -- otherwise /mcp could race
    // it and observe a transient Starting state instead.
    let degrade_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < degrade_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("flaky") && output.contains("failed") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    writer
        .write_all(b"/mcp\r")
        .expect("failed to write /mcp to pty");

    let listing_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < listing_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("state=connected") && output.contains("state=degraded") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("healthy"),
        "expected /mcp output to mention 'healthy', got: {output:?}"
    );
    assert!(
        output.contains("flaky"),
        "expected /mcp output to mention 'flaky', got: {output:?}"
    );
    assert!(
        output.contains("state=connected"),
        "expected /mcp output to show the healthy server as state=connected, got: {output:?}"
    );
    assert!(
        output.contains("state=degraded"),
        "expected /mcp output to show the flaky server as state=degraded, got: {output:?}"
    );
    assert!(
        output.contains("echo"),
        "expected /mcp output to list the connected server's 'echo' tool, got: {output:?}"
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

/// Ticket 51 (mcp-hooks-introspection) acceptance test: `/hooks` lists
/// every configured hook per event plus whether that event is actually
/// wired anywhere in `main.rs` (`state=active`) or not (`state=inactive`
/// -- e.g. a hook configured under `PreCompact`, one of the PRD's events
/// explicitly deferred to Phase 7, which `matching_hook_entries`'s call
/// sites never look up). `write_hooks_config` only writes one event/command
/// pair, so this writes `rokr.json` directly, mirroring
/// `write_mcp_config_servers`'s inline JSON-building style.
#[test]
fn hooks_command_lists_configured_hooks_per_event_and_active_state() {
    let home = unique_temp_dir("home-hooks-list");
    let xdg_config_home = unique_temp_dir("xdg-config-home-hooks-list");

    let config_dir = xdg_config_home.join("rokr");
    std::fs::create_dir_all(&config_dir).expect("failed to create rokr config dir for test");
    let config = serde_json::json!({
        "version": 1,
        "hooks": {
            "PreToolUse": [
                { "matcher": "bash", "command": "activehooktoken.sh" }
            ],
            "PreCompact": [
                { "command": "inactivehooktoken.sh" }
            ]
        }
    });
    std::fs::write(
        config_dir.join("rokr.json"),
        serde_json::to_string_pretty(&config).expect("failed to serialize test rokr.json"),
    )
    .expect("failed to write test rokr.json");

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
        .write_all(b"/hooks\r")
        .expect("failed to write /hooks to pty");

    let listing_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < listing_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("activehooktoken.sh") && output.contains("inactivehooktoken.sh") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("PreToolUse"),
        "expected /hooks output to mention event 'PreToolUse', got: {output:?}"
    );
    assert!(
        output.contains("activehooktoken.sh"),
        "expected /hooks output to mention the PreToolUse hook's command, got: {output:?}"
    );
    assert!(
        output.contains("PreCompact"),
        "expected /hooks output to mention event 'PreCompact', got: {output:?}"
    );
    assert!(
        output.contains("inactivehooktoken.sh"),
        "expected /hooks output to mention the PreCompact hook's command, got: {output:?}"
    );
    assert!(
        output.contains("state=active"),
        "expected /hooks output to mark the wired PreToolUse hook state=active, got: {output:?}"
    );
    assert!(
        output.contains("state=inactive"),
        "expected /hooks output to mark the unwired PreCompact hook state=inactive, \
         got: {output:?}"
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

/// Ticket 51 (mcp-hooks-introspection) acceptance test: `/mcp reconnect
/// <server>` restarts a `Degraded` server's connect+`list_tools` retry
/// loop, and a SUBSEQUENT `/mcp` shows it `state=connected` again with its
/// tool list restored -- proving the reconnect actually re-spawned and
/// succeeded, not merely accepted the command text. Uses the
/// `FAKE_MCP_SERVER_FAIL_UNTIL_FILE` fixture flag (ticket 51 addition,
/// `fake_mcp_server.rs`): the server fails every attempt while the marker
/// file is absent (driving it to `Degraded`, same shape as
/// `FAKE_MCP_SERVER_FAIL_INIT` elsewhere in this file), then succeeds once
/// the test creates the marker file and issues `/mcp reconnect` --
/// `FAKE_MCP_SERVER_FAIL_INIT` alone can't express this since it fails for
/// the entire life of the env var, with no way to later flip a live
/// subprocess's env out from under it.
#[test]
fn mcp_reconnect_command_restarts_a_degraded_server() {
    let home = unique_temp_dir("home-mcp-reconnect");
    let xdg_config_home = unique_temp_dir("xdg-config-home-mcp-reconnect");
    let marker_dir = unique_temp_dir("marker-mcp-reconnect");
    let marker_path = marker_dir.join("recover.marker");

    write_mcp_config_servers(
        &xdg_config_home,
        &[(
            "flaky",
            &fake_mcp_server_path(),
            serde_json::json!({ "FAKE_MCP_SERVER_FAIL_UNTIL_FILE": marker_path.to_string_lossy() }),
        )],
    );

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

    // Let the bounded retry+backoff exhaust into Degraded first.
    let degrade_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < degrade_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("flaky") && output.contains("failed") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("flaky") && output.contains("failed"),
        "expected a one-line status notice mentioning the failed server 'flaky', got: {output:?}"
    );

    writer
        .write_all(b"/mcp\r")
        .expect("failed to write /mcp to pty");
    let degraded_listing_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < degraded_listing_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("state=degraded") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("state=degraded"),
        "expected /mcp to show 'flaky' as state=degraded before reconnect, got: {output:?}"
    );

    // Now let the fixture succeed on its next spawn, and trigger it.
    std::fs::create_dir_all(&marker_dir).expect("failed to create marker dir");
    std::fs::write(&marker_path, "").expect("failed to write recovery marker file");

    writer
        .write_all(b"/mcp reconnect flaky\r")
        .expect("failed to write /mcp reconnect to pty");

    // The freshly spawned subprocess (marker file now present) succeeds on
    // its very first attempt -- no backoff delay -- but still needs a beat
    // for the real connect+list_tools round-trip to complete before a
    // fresh `/mcp` reflects it.
    thread::sleep(Duration::from_millis(500));
    writer
        .write_all(b"/mcp\r")
        .expect("failed to write /mcp to pty");

    let reconnect_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < reconnect_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("state=connected") && output.contains("echo") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("state=connected"),
        "expected 'flaky' to show state=connected after /mcp reconnect, got: {output:?}"
    );
    assert!(
        output.contains("echo"),
        "expected 'flaky's tool list to be restored (containing 'echo') after reconnect, \
         got: {output:?}"
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
    let _ = std::fs::remove_dir_all(&marker_dir);
}

/// Ticket 46 (mcp-namespace-multi-server-freeze) acceptance test: two
/// configured stdio servers ("server_a", "server_b") each expose a tool
/// with the identical RAW name "search" (`FAKE_MCP_SERVER_TOOL_NAME`, a
/// ticket-46 fixture addition -- see `fake_mcp_server.rs`). The model
/// issues both tool calls in ONE assistant turn; both must be reachable
/// and individually executable via their namespaced names
/// (`mcp__server_a__search` / `mcp__server_b__search`) without either
/// colliding with or shadowing the other -- the whole point of
/// `qualified_name`'s per-server namespacing (PRD "Namespacing").
#[tokio::test]
async fn two_mcp_servers_with_colliding_tool_name_both_reachable_by_namespaced_name() {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let final_reply_text = "FinalReplyAfterBothMcpServersForTesting";
    let qualified_a = rokr_mcp::qualified_name("server_a", "search");
    let qualified_b = rokr_mcp::qualified_name("server_b", "search");

    // The model calls BOTH servers' "search" tool in the same assistant
    // turn -- `run_tool_loop` executes tool calls in order, so this drives
    // two sequential permission prompts within one submit.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-mcp-two-servers",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "tool_calls": [
                            {
                                "id": "call_a",
                                "type": "function",
                                "function": {
                                    "name": qualified_a,
                                    "arguments": serde_json::json!({ "message": "hi" }).to_string()
                                }
                            },
                            {
                                "id": "call_b",
                                "type": "function",
                                "function": {
                                    "name": qualified_b,
                                    "arguments": serde_json::json!({ "message": "hi" }).to_string()
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

    // Only matches once BOTH tool results have round-tripped back into the
    // request body -- proving both namespaced calls actually ran against
    // their own (real, separately-spawned) fixture server, not a stub.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains(FAKE_MCP_SERVER_FIXED_RESPONSE_TEXT))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-mcp-two-servers-final",
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

    let home = unique_temp_dir("home-mcp-two-servers");
    let xdg_config_home = unique_temp_dir("xdg-config-home-mcp-two-servers");
    let fixture = fake_mcp_server_path();
    write_mcp_config_servers(
        &xdg_config_home,
        &[
            (
                "server_a",
                fixture.as_path(),
                serde_json::json!({ "FAKE_MCP_SERVER_TOOL_NAME": "search" }),
            ),
            (
                "server_b",
                fixture.as_path(),
                serde_json::json!({ "FAKE_MCP_SERVER_TOOL_NAME": "search" }),
            ),
        ],
    );

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
        .write_all(b"call both mcp servers\r")
        .expect("failed to write prompt to pty");

    // First permission prompt: server_a's namespaced tool name.
    let prompt_a_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_a_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(&qualified_a) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(&qualified_a),
        "expected pty output to contain '{qualified_a}' in the first permission prompt, got: \
         {output:?}"
    );

    writer
        .write_all(b"y")
        .expect("failed to write accept keypress for server_a to pty");

    // Second permission prompt: server_b's namespaced tool name -- must be
    // distinguishable from server_a's despite both wrapping the identical
    // raw tool name "search".
    let prompt_b_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_b_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(&qualified_b) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(&qualified_b),
        "expected pty output to contain '{qualified_b}' in the second permission prompt, got: \
         {output:?}"
    );

    writer
        .write_all(b"y")
        .expect("failed to write accept keypress for server_b to pty");

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
        "expected pty output to contain the final assistant reply after accepting both \
         namespaced MCP tool calls, got: {output:?}"
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

/// PC-1 ruling acceptance test (supersedes ticket 46's whole-session
/// `OnceLock` freeze and this test's original "must NOT retroactively add
/// its tool" semantics): a server's tool contribution joins the session
/// snapshot EXACTLY ONCE, at its own first `Ready` -- never re-frozen for
/// the whole session up front. A server held in `Starting` (via the
/// `FAKE_MCP_SERVER_READY_GATE_FILE` fixture knob) contributes zero tools
/// to turn 1's outgoing tool list; releasing the gate and letting it reach
/// `Ready` before turn 2 means it DOES join and appear in turn 2's outgoing
/// tool list -- this is the intended one-time auto-join, not a forbidden
/// "turn-to-turn mutation" (an already-joined server's own contribution
/// staying byte-for-byte identical turn-to-turn is the invariant that
/// still holds; a not-yet-joined server joining for the first time is not
/// a mutation of anything).
#[tokio::test]
async fn slow_server_joins_snapshot_on_first_ready_reached_between_turns() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let first_reply_text = "OrdinaryFirstReplyBeforeSlowServerReadyForTesting";
    let second_reply_text = "OrdinarySecondReplyAfterSlowServerReadyForTesting";
    let slow_server_qualified_name = rokr_mcp::qualified_name("slow", "echo");

    // Turn 1: ordinary reply, no tool call -- this is the submit whose
    // (empty, "slow" isn't Ready yet) MCP snapshot gets frozen.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-mcp-freeze-turn1",
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

    // Turn 2: also an ordinary reply -- the interesting assertion is on the
    // outgoing REQUEST body (captured via `received_requests()` below),
    // not this response.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-mcp-freeze-turn2",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": second_reply_text },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-mcp-freeze");
    let xdg_config_home = unique_temp_dir("xdg-config-home-mcp-freeze");
    let ready_gate_dir = unique_temp_dir("mcp-freeze-ready-gate");
    let ready_gate_file = ready_gate_dir.join("ready");
    write_mcp_config(
        &xdg_config_home,
        "slow",
        &fake_mcp_server_path(),
        serde_json::json!({
            "FAKE_MCP_SERVER_READY_GATE_FILE": ready_gate_file.to_string_lossy()
        }),
    );

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
    // First paint must not be delayed by the still-gated "slow" server.
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

    // Turn 1, submitted while "slow" is still gated (never responded to
    // `initialize`, so it's stuck in `Starting`) -- this is when the
    // session's MCP tool snapshot is taken and frozen, empty.
    writer
        .write_all(b"say something ordinary\r")
        .expect("failed to write turn1 prompt to pty");

    let turn1_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < turn1_deadline {
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
        "expected pty output to contain turn 1's reply, got: {output:?}"
    );

    // Release the gate: "slow" can now complete `initialize` + `tools/list`
    // and reach `Ready` with its "echo" tool -- a genuine mid-session
    // tool-availability change happening strictly AFTER turn 1's snapshot
    // was already taken.
    std::fs::write(&ready_gate_file, b"go").expect("failed to write mcp ready-gate file");
    // Give the background lifecycle task real wall-clock time to finish
    // the handshake and flip status to `Ready` before turn 2 submits --
    // generous relative to the fixture's own 25ms poll interval.
    thread::sleep(Duration::from_millis(500));

    // Turn 2: if the tool snapshot were (incorrectly) recomputed fresh per
    // submit, "slow"'s now-Ready "echo" tool would appear in this turn's
    // outgoing tool-spec list.
    writer
        .write_all(b"say something else ordinary\r")
        .expect("failed to write turn2 prompt to pty");

    let turn2_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < turn2_deadline {
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
        "expected pty output to contain turn 2's reply, got: {output:?}"
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

    let received_requests = mock_server.received_requests().await.expect(
        "mock server should have recorded received requests \
         (make sure the request recorder wasn't disabled)",
    );
    assert!(
        received_requests.len() >= 2,
        "expected at least two outgoing requests (one per turn), got: {}",
        received_requests.len()
    );
    let first_request_body = String::from_utf8_lossy(&received_requests[0].body).into_owned();
    assert!(
        !first_request_body.contains(&slow_server_qualified_name),
        "expected turn 1's outgoing request to NOT contain '{slow_server_qualified_name}' -- \
         'slow' was still gated (Starting), never having reached Ready, so it had not yet \
         joined the session's tool set; got body: {first_request_body:?}"
    );
    let second_request_body = String::from_utf8_lossy(&received_requests[1].body).into_owned();
    assert!(
        second_request_body.contains(&slow_server_qualified_name),
        "expected turn 2's outgoing request to CONTAIN '{slow_server_qualified_name}' -- PC-1: \
         'slow' reached Ready (and therefore joined) between turn 1 and turn 2, and a server's \
         first Ready is exactly when it's supposed to join the session's tool set, not \
         something a stale whole-session freeze should hold back until a future session; got \
         body: {second_request_body:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&ready_gate_dir);
}

/// Ticket 47 (mcp-permission-polish): same as `write_mcp_config` above but
/// also sets a per-server `auto_approve` list, needed for the allowlist
/// bypass acceptance test below. A separate helper (rather than adding a
/// parameter to `write_mcp_config`/`write_mcp_config_servers`) so this
/// doesn't touch either existing helper's signature or any of their many
/// existing call sites above.
fn write_mcp_config_with_auto_approve(
    xdg_config_home: &std::path::Path,
    server_name: &str,
    command: &std::path::Path,
    env: serde_json::Value,
    auto_approve: &[&str],
) {
    let config_dir = xdg_config_home.join("rokr");
    std::fs::create_dir_all(&config_dir).expect("failed to create rokr config dir for test");

    let config = serde_json::json!({
        "version": 1,
        "mcp": {
            server_name: {
                "transport": {
                    "stdio": {
                        "command": command.to_string_lossy(),
                        "args": [],
                        "env": env
                    }
                },
                "enabled": true,
                "auto_approve": auto_approve
            }
        }
    });

    std::fs::write(
        config_dir.join("rokr.json"),
        serde_json::to_string_pretty(&config).expect("failed to serialize test rokr.json"),
    )
    .expect("failed to write test rokr.json");
}

/// Ticket 47 (mcp-permission-polish) acceptance test: an MCP tool call for
/// a server/tool NOT on that server's (empty, here -- `write_mcp_config`
/// sets no `auto_approve` at all) allowlist renders a permission prompt
/// whose text carries the server name, tool name, and pretty-printed input
/// JSON via the new `PermissionPayload::ToolCall` -> `PermissionDetail::Text`
/// bridge, rather than ticket 44's interim opaque `Command(String)` blob.
/// Checks individual word-tokens against the accumulated PTY output rather
/// than one contiguous phrase (this suite's PTY-assertion convention --
/// rendering can wrap/space content unpredictably).
#[tokio::test]
async fn mcp_tool_call_renders_server_and_tool_in_permission_prompt_text() {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let final_reply_text = "FinalReplyAfterMcpPromptTextForTesting";
    let qualified_tool_name = rokr_mcp::qualified_name("interim", "echo");

    // First call: the model asks to invoke the MCP tool.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-mcp-prompt-text",
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
                                    "name": qualified_tool_name,
                                    "arguments": serde_json::json!({ "message": "hi" }).to_string()
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

    // Second call: only matches once the tool result the loop fed back
    // actually contains the fixture's real response text -- proves the
    // tool really executed after the granted permission, not before.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains(FAKE_MCP_SERVER_FIXED_RESPONSE_TEXT))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-mcp-prompt-text-final",
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

    let home = unique_temp_dir("home-mcp-prompt-text");
    let xdg_config_home = unique_temp_dir("xdg-config-home-mcp-prompt-text");
    // No `auto_approve` set at all -- defaults to empty, so "echo" is NOT
    // on the allowlist and the call must be gated through a prompt.
    write_mcp_config(
        &xdg_config_home,
        "interim",
        &fake_mcp_server_path(),
        serde_json::json!({}),
    );

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
        .write_all(b"call the mcp tool\r")
        .expect("failed to write prompt to pty");

    // Wait for the permission prompt to render, then check that its text
    // carries the server name, the tool name, and the pretty-printed input
    // -- individually, since ratatui wrapping can split/space them
    // unpredictably across rendered rows.
    let prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("permission needed") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("interim"),
        "expected pty output to contain the server name 'interim' in the permission prompt, \
         got: {output:?}"
    );
    assert!(
        output.contains("echo"),
        "expected pty output to contain the tool name 'echo' in the permission prompt, \
         got: {output:?}"
    );
    assert!(
        output.contains("message"),
        "expected pty output to contain the pretty-printed input's key 'message' in the \
         permission prompt, got: {output:?}"
    );
    assert!(
        output.contains("hi"),
        "expected pty output to contain the pretty-printed input's value 'hi' in the \
         permission prompt, got: {output:?}"
    );
    assert!(
        !output.contains(FAKE_MCP_SERVER_FIXED_RESPONSE_TEXT),
        "the MCP tool must not have run before permission was granted, but its response text \
         already appears in the output: {output:?}"
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
        "expected pty output to contain the final assistant reply after granting permission, \
         got: {output:?}"
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

/// Ticket 47 (mcp-permission-polish) acceptance test: an MCP tool ON its
/// server's `auto_approve` list executes with NO permission prompt shown at
/// all -- the config-driven allowlist pre-approves it, the same way an
/// interactively-granted gated tool would run, just without the interactive
/// step.
#[tokio::test]
async fn mcp_tool_on_auto_approve_list_executes_without_permission_prompt() {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let final_reply_text = "FinalReplyAfterMcpAutoApproveForTesting";
    let qualified_tool_name = rokr_mcp::qualified_name("interim", "echo");

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-mcp-auto-approve",
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
                                    "name": qualified_tool_name,
                                    "arguments": serde_json::json!({ "message": "hi" }).to_string()
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

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains(FAKE_MCP_SERVER_FIXED_RESPONSE_TEXT))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-mcp-auto-approve-final",
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

    let home = unique_temp_dir("home-mcp-auto-approve");
    let xdg_config_home = unique_temp_dir("xdg-config-home-mcp-auto-approve");
    write_mcp_config_with_auto_approve(
        &xdg_config_home,
        "interim",
        &fake_mcp_server_path(),
        serde_json::json!({}),
        &["echo"],
    );

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
        .write_all(b"call the mcp tool\r")
        .expect("failed to write prompt to pty");

    // No permission prompt should ever appear: the allowlisted tool call
    // should sail straight through to the final reply without pausing for
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
        "expected pty output to contain the final assistant reply after the allowlisted MCP \
         tool call ran without a prompt, got: {output:?}"
    );
    assert!(
        !output.contains("permission needed"),
        "expected no permission prompt to ever be shown for an auto_approve-listed MCP tool, \
         got: {output:?}"
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

/// Ticket 57 (cost-command-and-headless-reporting) acceptance test: `/cost`
/// must print a token-by-type breakdown, cache-hit rate, and estimated
/// dollar cost folded from the current session's `UsageRecord`s. Mocks a
/// single turn against `gpt-4o-mini` (a model with non-zero default pricing
/// -- see `rokr_config::default_model_pricing`) reporting an EXACT,
/// hand-picked usage (input=800_000, cache-read=200_000, output=500_000,
/// cache-write=0) chosen so the cache-hit rate (200_000 / (800_000 +
/// 200_000) = 20.0%) and dollar cost (800_000*$0.00000015 +
/// 500_000*$0.0000006 + 200_000*$0.000000075 = $0.4350) both land on clean,
/// unambiguous decimal values -- no floating-point rounding-boundary risk
/// in the assertions below.
#[tokio::test]
async fn cost_command_prints_token_breakdown_cache_hit_rate_and_dollar_estimate() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    let reply_text = "CostCommandReplyMarker8834";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-cost",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": reply_text },
                    "finish_reason": "stop"
                }
            ],
            "usage": {
                "prompt_tokens": 800000,
                "completion_tokens": 500000,
                "prompt_tokens_details": { "cached_tokens": 200000 }
            }
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-cost-command");
    let xdg_config_home = unique_temp_dir("xdg-config-home-cost-command");

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
        .write_all(b"trigger the turn\r")
        .expect("failed to write prompt to pty");

    let response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < response_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(reply_text) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(reply_text),
        "expected pty output to contain the mocked assistant reply, got: {output:?}"
    );

    writer
        .write_all(b"/cost\r")
        .expect("failed to write /cost to pty");

    let cost_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < cost_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Cache hit rate") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("800000"),
        "expected /cost output to contain the input token count, got: {output:?}"
    );
    assert!(
        output.contains("500000"),
        "expected /cost output to contain the output token count, got: {output:?}"
    );
    assert!(
        output.contains("200000"),
        "expected /cost output to contain the cache-read token count, got: {output:?}"
    );
    assert!(
        output.contains("20.0%"),
        "expected /cost output to contain the cache-hit-rate figure, got: {output:?}"
    );
    assert!(
        output.contains("0.4350"),
        "expected /cost output to contain the estimated dollar cost, got: {output:?}"
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

/// Ticket 57 (cost-command-and-headless-reporting) acceptance test: `/cost
/// --all` must fold usage across EVERY session on disk, not just the
/// currently active one. Pre-seeds two fixture sessions (A and B) directly
/// under `<data_dir>/sessions/`, each with its own `session.jsonl` AND a
/// combined `sessions/index.jsonl` entry (mirrors
/// `resume_without_confirm_warns_and_confirm_swaps_transcript_and_writer`'s
/// own fixture-seeding pattern -- `SessionStore::list_sessions`, which
/// `/cost --all` uses to enumerate every session, reads only the index, not
/// a directory scan). Both fixtures use `gpt-4o-mini` (non-zero default
/// pricing). Neither fixture's individual totals could satisfy the
/// assertions below alone -- only their SUM (input=4000, output=600,
/// cache-read=400, cache_hit_rate=400/4400=9.1%, cost=$0.0010) proves both
/// contributed. No prompt is submitted in this test (the freshly-created
/// active session therefore contributes zero usage of its own, and a mock
/// server is wired up only so provider startup has a syntactically valid
/// base URL to point at -- it never receives a request).
#[tokio::test]
async fn cost_all_flag_folds_every_session_on_disk() {
    use wiremock::MockServer;

    let mock_server = MockServer::start().await;

    let home = unique_temp_dir("home-cost-all");
    let xdg_config_home = unique_temp_dir("xdg-config-home-cost-all");
    let xdg_data_home = unique_temp_dir("xdg-data-home-cost-all");

    let session_a_id = "01FIXTURECOSTALLSESSIONAAA";
    let session_b_id = "01FIXTURECOSTALLSESSIONBBB";
    let sessions_root = xdg_data_home.join("rokr").join("sessions");
    std::fs::create_dir_all(&sessions_root).expect("failed to create fixture sessions root");

    let session_a_header = rokr_session::SessionRecord::Header {
        schema_version: 2,
        session_id: session_a_id.to_string(),
        created_at: "2026-07-21T01:00:00Z".to_string(),
        project_path: "/tmp/fixture-project-a".to_string(),
        agent_tier: "plan".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
    };
    let session_a_turn = rokr_session::SessionRecord::Turn {
        messages: vec![rokr_core::Message::user_text("sessionaprompt")],
        usage: rokr_session::UsageRecord {
            input_tokens: 1000,
            output_tokens: 200,
            cache_read_tokens: 100,
            cache_write_tokens: 0,
        },
        timestamp: "2026-07-21T01:00:01Z".to_string(),
    };
    let session_a_dir = sessions_root.join(session_a_id);
    std::fs::create_dir_all(&session_a_dir).expect("failed to create session A fixture dir");
    std::fs::write(
        session_a_dir.join("session.jsonl"),
        format!(
            "{}\n{}\n",
            serde_json::to_string(&session_a_header).unwrap(),
            serde_json::to_string(&session_a_turn).unwrap(),
        ),
    )
    .expect("failed to write session A fixture session.jsonl");

    let session_b_header = rokr_session::SessionRecord::Header {
        schema_version: 2,
        session_id: session_b_id.to_string(),
        created_at: "2026-07-21T02:00:00Z".to_string(),
        project_path: "/tmp/fixture-project-b".to_string(),
        agent_tier: "plan".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
    };
    let session_b_turn = rokr_session::SessionRecord::Turn {
        messages: vec![rokr_core::Message::user_text("sessionbprompt")],
        usage: rokr_session::UsageRecord {
            input_tokens: 3000,
            output_tokens: 400,
            cache_read_tokens: 300,
            cache_write_tokens: 0,
        },
        timestamp: "2026-07-21T02:00:01Z".to_string(),
    };
    let session_b_dir = sessions_root.join(session_b_id);
    std::fs::create_dir_all(&session_b_dir).expect("failed to create session B fixture dir");
    std::fs::write(
        session_b_dir.join("session.jsonl"),
        format!(
            "{}\n{}\n",
            serde_json::to_string(&session_b_header).unwrap(),
            serde_json::to_string(&session_b_turn).unwrap(),
        ),
    )
    .expect("failed to write session B fixture session.jsonl");

    let session_a_index_entry = rokr_session::SessionIndexEntry {
        session_id: session_a_id.to_string(),
        project_path: "/tmp/fixture-project-a".to_string(),
        created_at: "2026-07-21T01:00:00Z".to_string(),
        updated_at: "2026-07-21T01:00:01Z".to_string(),
        title: "sessionaprompt".to_string(),
        turn_count: 1,
        last_model: "gpt-4o-mini".to_string(),
    };
    let session_b_index_entry = rokr_session::SessionIndexEntry {
        session_id: session_b_id.to_string(),
        project_path: "/tmp/fixture-project-b".to_string(),
        created_at: "2026-07-21T02:00:00Z".to_string(),
        updated_at: "2026-07-21T02:00:01Z".to_string(),
        title: "sessionbprompt".to_string(),
        turn_count: 1,
        last_model: "gpt-4o-mini".to_string(),
    };
    std::fs::write(
        sessions_root.join("index.jsonl"),
        format!(
            "{}\n{}\n",
            serde_json::to_string(&session_a_index_entry).unwrap(),
            serde_json::to_string(&session_b_index_entry).unwrap(),
        ),
    )
    .expect("failed to write fixture index.jsonl");

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
    cmd.env("XDG_DATA_HOME", &xdg_data_home);
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
        .write_all(b"/cost --all\r")
        .expect("failed to write /cost --all to pty");

    let cost_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < cost_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Cache hit rate") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("4000"),
        "expected /cost --all output to contain the combined input token count (1000 + 3000 \
         from both fixture sessions), got: {output:?}"
    );
    assert!(
        output.contains("600"),
        "expected /cost --all output to contain the combined output token count (200 + 400 \
         from both fixture sessions), got: {output:?}"
    );
    assert!(
        output.contains("400"),
        "expected /cost --all output to contain the combined cache-read token count (100 + 300 \
         from both fixture sessions), got: {output:?}"
    );
    assert!(
        output.contains("9.1%"),
        "expected /cost --all output to contain the combined cache-hit-rate figure \
         (400 / (4000 + 400) = 9.1%), got: {output:?}"
    );
    assert!(
        output.contains("0.0010"),
        "expected /cost --all output to contain the combined estimated dollar cost, got: \
         {output:?}"
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
    let _ = std::fs::remove_dir_all(&xdg_data_home);
}

/// F-004 (pre-ship review) acceptance test: "built-in wins over custom
/// command" must be enforced by the command NAME, not by exact-string
/// comparison of the whole input against a hardcoded built-in literal. A
/// user-scope `cost.md` custom command is discovered (same fixture shape as
/// `custom_command_...` above), but typing `/cost --today` -- a real
/// builtin name with an argument the builtin's own dispatch doesn't
/// recognize -- must never fall through to submitting the discovered
/// template's content as a prompt: proven two ways, (1) the custom
/// template's distinctive marker text never appears anywhere in the
/// rendered output, and (2) the mock provider never receives a single
/// request (the built-in `/cost` dispatch never talks to the provider at
/// all, so ANY request landing there would itself prove the custom
/// template's `$ARGUMENTS`-expanded body got submitted instead).
#[tokio::test]
async fn builtin_command_wins_over_same_named_custom_command_even_with_unrecognized_args() {
    use wiremock::MockServer;

    let mock_server = MockServer::start().await;

    let home = unique_temp_dir("home-builtin-wins-by-name");
    let xdg_config_home = unique_temp_dir("xdg-config-home-builtin-wins-by-name");

    // rokr_config::default_config_dir() resolves to `$XDG_CONFIG_HOME/rokr`
    // -- same convention the other custom-command fixtures in this file
    // use. "cost" collides with the real built-in `/cost` handler in
    // `crates/rokr/src/main.rs`'s `command` closure.
    let commands_dir = xdg_config_home.join("rokr").join("commands");
    std::fs::create_dir_all(&commands_dir).expect("failed to create fixture commands directory");
    std::fs::write(
        commands_dir.join("cost.md"),
        "---\ndescription: test\n---\nCUSTOM-COST-TEMPLATE-SHOULD-NEVER-BE-SUBMITTED $ARGUMENTS",
    )
    .expect("failed to write fixture cost.md");

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
    cmd.env("ROKR_OPENAI_API_KEY", "test-openai-api-key");

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
        .write_all(b"/cost --today\r")
        .expect("failed to write /cost --today to pty");

    // The built-in `/cost` dispatch responds synchronously with no
    // provider round trip, so waiting for the ordinary "unknown command"
    // wording (still the built-in dispatcher's own generic fallthrough
    // text for an unrecognized `/cost` invocation -- see
    // `is_builtin_command`'s doc comment in main.rs) is the deterministic
    // signal that dispatch has settled, one way or the other.
    let settle_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < settle_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("unknown command")
            || output.contains("CUSTOM-COST-TEMPLATE-SHOULD-NEVER-BE-SUBMITTED")
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        !output.contains("CUSTOM-COST-TEMPLATE-SHOULD-NEVER-BE-SUBMITTED"),
        "expected the discovered custom 'cost' command to NEVER be submitted as a prompt just \
         because the built-in /cost dispatch didn't recognize '--today' -- built-ins must win \
         by command NAME, not by exact-string match against the full input; got: {output:?}"
    );

    let received_requests = mock_server.received_requests().await.expect(
        "mock server should have recorded received requests \
         (make sure the request recorder wasn't disabled)",
    );
    assert!(
        received_requests.is_empty(),
        "expected NO outgoing request to the provider -- the built-in /cost dispatch never \
         talks to the provider, so any request proves the custom template was wrongly \
         submitted instead; got requests: {received_requests:?}"
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

/// Ticket 62 (memory-slash-command-opens-editor) acceptance test: typing
/// `/memory` and pressing Enter suspends the TUI (mirroring ticket 42's
/// Ctrl+E editor keybinding), spawns the scripted `$EDITOR` stand-in
/// directly against the project-scope memory file (`<cwd>/AGENTS.md`,
/// creating it first since `project_dir` deliberately has no pre-existing
/// AGENTS.md), and on the script's exit restores the terminal -- verified
/// both by the scripted editor's marker line landing in the real on-disk
/// file (not routed back through the prompt buffer/rendered output, unlike
/// Ctrl+E's scratch-buffer round trip) and by the session still quitting
/// cleanly on a subsequent `q`, proving the terminal was left in a normal,
/// responsive state.
#[tokio::test]
async fn memory_command_suspends_tui_and_opens_project_memory_file_in_scripted_editor() {
    use std::os::unix::fs::PermissionsExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-memory",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "unused-memory-command-never-hits-the-provider"
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-memory-cmd");
    let xdg_config_home = unique_temp_dir("xdg-config-home-memory-cmd");
    let xdg_data_home = unique_temp_dir("xdg-data-home-memory-cmd");
    let project_dir = unique_temp_dir("memory-cmd-project");

    let memory_path = project_dir.join("AGENTS.md");
    assert!(
        !memory_path.exists(),
        "test setup should start with no pre-existing AGENTS.md, to also exercise \
         create-if-absent"
    );

    // Scripted `$EDITOR` stand-in: appends a known, unique marker line to
    // whatever file path it's given (`$1`) and exits 0 immediately, standing
    // in for a real interactive editor without requiring one in CI.
    let edited_marker = "MemoryCommandScriptedEditorMarkerUniqueToken";
    let editor_script_dir = unique_temp_dir("editor-script-memory-cmd");
    let editor_script_path = editor_script_dir.join("fake_editor.sh");
    std::fs::write(
        &editor_script_path,
        format!("#!/bin/sh\nprintf '\\n{edited_marker}\\n' >> \"$1\"\n"),
    )
    .expect("failed to write fake editor script");
    let mut perms = std::fs::metadata(&editor_script_path)
        .expect("failed to stat fake editor script")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&editor_script_path, perms)
        .expect("failed to make fake editor script executable");

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
    cmd.env("XDG_DATA_HOME", &xdg_data_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");
    cmd.env(
        "EDITOR",
        editor_script_path
            .to_str()
            .expect("script path should be valid utf-8"),
    );
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
        .write_all(b"/memory\r")
        .expect("failed to write /memory + Enter to pty");

    // The scripted editor writes straight to the real on-disk memory file,
    // not back through the prompt buffer/rendered output (unlike Ctrl+E's
    // scratch-buffer round trip) -- so this polls the file directly rather
    // than watching the pty output stream.
    let edit_deadline = Instant::now() + Duration::from_secs(10);
    let mut memory_contents = String::new();
    while Instant::now() < edit_deadline {
        if let Ok(contents) = std::fs::read_to_string(&memory_path) {
            if contents.contains(edited_marker) {
                memory_contents = contents;
                break;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        memory_contents.contains(edited_marker),
        "expected the scripted editor's marker line to have been written to the project-scope \
         AGENTS.md file at {memory_path:?} after /memory + Enter; contents were: \
         {memory_contents:?}"
    );

    // Confirm the terminal was restored to a normal, responsive state: `q`
    // (with an empty prompt, matching `should_quit`'s contract) should still
    // cleanly quit the session, the same way ticket 42's Ctrl+E test proves
    // terminal restoration by continuing to interact normally afterward.
    writer.write_all(b"q").expect("failed to write q to pty");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll rokr exit status") {
            break status;
        }
        if Instant::now() > exit_deadline {
            let _ = child.kill();
            panic!(
                "rokr did not exit within timeout after pressing q following /memory; output so \
                 far: {output:?}"
            );
        }
        thread::sleep(Duration::from_millis(50));
    };
    assert!(
        status.success(),
        "expected rokr to exit cleanly after q following /memory, proving the terminal was \
         restored; got status: {status:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&xdg_data_home);
    let _ = std::fs::remove_dir_all(&project_dir);
    let _ = std::fs::remove_dir_all(&editor_script_dir);
}

/// Ticket 63 (custom-command-discovery-and-registry) acceptance test: a
/// user-scope `commands/my-command.md` template containing `$ARGUMENTS`,
/// when invoked as `/my-command foo bar`, must expand and be submitted
/// through the SAME path an ordinary typed prompt takes -- not merely
/// displayed as a command-status string. Mirrors
/// `typing_model_command_switches_active_provider_for_next_turn`'s harness
/// (PTY spawn, wiremock mock server, ROKR_OPENAI_* env vars), differing
/// only in: (1) pre-seeding a fixture `commands/my-command.md` file under
/// `XDG_CONFIG_HOME/rokr/` before spawning, and (2) asserting on the
/// captured request body (via `mock_server.received_requests()`) that the
/// EXPANDED template text ("Handle: foo bar"), not the raw typed command
/// string, is what went out over the wire -- the strongest possible signal
/// that expansion actually routed through the ordinary submit path (which
/// alone talks to the provider) rather than the `command` status-string
/// path (which never does).
#[tokio::test]
async fn typing_discovered_user_command_expands_template_and_submits_through_ordinary_path() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let canned_response = "MockedAssistantReplyForCustomCommandTesting";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-custom-command",
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

    let home = unique_temp_dir("home-custom-command");
    let xdg_config_home = unique_temp_dir("xdg-config-home-custom-command");

    // rokr_config::default_config_dir() resolves to `$XDG_CONFIG_HOME/rokr`
    // -- the user-scope `commands/` subdirectory this ticket discovers from
    // lives directly under that same root, not under a new convention.
    let commands_dir = xdg_config_home.join("rokr").join("commands");
    std::fs::create_dir_all(&commands_dir).expect("failed to create fixture commands directory");
    std::fs::write(
        commands_dir.join("my-command.md"),
        "---\ndescription: test\n---\nHandle: $ARGUMENTS",
    )
    .expect("failed to write fixture my-command.md");

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
    cmd.env("ROKR_OPENAI_API_KEY", "test-openai-api-key");

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
        .write_all(b"/my-command foo bar\r")
        .expect("failed to write custom command invocation to pty");

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
        "expected pty output to contain the mocked assistant response, proving a real \
         request/response round trip happened through the ordinary submit path (a status-only \
         `command` response never talks to the provider); got: {output:?}"
    );
    assert!(
        !output.contains("unknown command: /my-command"),
        "expected the discovered custom command to be recognized rather than falling through \
         to the built-in dispatcher's \"unknown command\" arm; got: {output:?}"
    );

    let received_requests = mock_server.received_requests().await.expect(
        "mock server should have recorded received requests \
         (make sure the request recorder wasn't disabled)",
    );
    assert!(
        !received_requests.is_empty(),
        "expected at least one outgoing request to /chat/completions"
    );
    let first_request_body = String::from_utf8_lossy(&received_requests[0].body).into_owned();
    assert!(
        first_request_body.contains("Handle: foo bar"),
        "expected the outgoing request body to contain the EXPANDED template text \
         ('Handle: foo bar'), not the raw '/my-command foo bar' input, proving template \
         expansion happened before submission; got body: {first_request_body:?}"
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

/// Ticket 64 (custom-command-project-scope-and-trust-boundary) acceptance
/// test: a project-scope `.rokr/commands/deploy.md` template, discovered
/// from the spawned process's cwd, must expand and submit through the same
/// ordinary path ticket 63 proved for user-scope commands -- ALONGSIDE a
/// user-scope command (`my-command.md`, mirroring
/// `typing_discovered_user_command_expands_template_and_submits_through_ordinary_path`),
/// proving project-scope discovery happens in ADDITION to user-scope, not
/// instead of it. Asserts on the captured request bodies (via
/// `mock_server.received_requests()`) that each command's EXPANDED template
/// text went out over the wire, in submission order.
#[tokio::test]
async fn project_scope_command_discovered_from_dot_rokr_commands_directory_alongside_user_scope() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let canned_response = "MockedAssistantReplyForProjectScopeCommandTesting";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-project-scope-command",
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

    let home = unique_temp_dir("home-project-scope-command");
    let xdg_config_home = unique_temp_dir("xdg-config-home-project-scope-command");
    let project_dir = unique_temp_dir("project-scope-command-project");

    // Project-scope fixture: `.rokr/commands/deploy.md` under the project
    // directory the spawned process's cwd will be set to.
    let project_commands_dir = project_dir.join(".rokr").join("commands");
    std::fs::create_dir_all(&project_commands_dir)
        .expect("failed to create fixture project-scope commands directory");
    std::fs::write(
        project_commands_dir.join("deploy.md"),
        "---\ndescription: test\n---\nDeploying: $ARGUMENTS",
    )
    .expect("failed to write fixture deploy.md");

    // User-scope fixture: a DIFFERENTLY-named command, to prove project
    // scope is discovered ALONGSIDE user scope, not instead of it.
    let user_commands_dir = xdg_config_home.join("rokr").join("commands");
    std::fs::create_dir_all(&user_commands_dir)
        .expect("failed to create fixture user-scope commands directory");
    std::fs::write(
        user_commands_dir.join("my-command.md"),
        "---\ndescription: test\n---\nHandle: $ARGUMENTS",
    )
    .expect("failed to write fixture my-command.md");

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
    cmd.env("ROKR_OPENAI_API_KEY", "test-openai-api-key");
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
        .write_all(b"/deploy prod\r")
        .expect("failed to write /deploy invocation to pty");

    let first_response_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < first_response_deadline {
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
        "expected pty output to contain the mocked assistant response after /deploy prod; got: \
         {output:?}"
    );

    writer
        .write_all(b"/my-command foo\r")
        .expect("failed to write /my-command invocation to pty");

    // Wait for a SECOND outgoing request to have landed at the mock server,
    // proving /my-command also round-tripped through the ordinary submit
    // path (not just re-rendering the first response already in `output`).
    // Keeps draining `rx` into `output` on every poll, same as the
    // render/response wait loops above -- otherwise the pty's output buffer
    // fills up while nobody reads it and the child blocks on its own
    // stdout write, wedging it before it ever processes the later `q`.
    let second_request_deadline = Instant::now() + Duration::from_secs(10);
    let mut received_requests = Vec::new();
    while Instant::now() < second_request_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        received_requests = mock_server.received_requests().await.expect(
            "mock server should have recorded received requests \
             (make sure the request recorder wasn't disabled)",
        );
        if received_requests.len() >= 2 {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        !output.contains("unknown command: /deploy"),
        "expected the project-scope discovered /deploy command to be recognized rather than \
         falling through to the built-in dispatcher's \"unknown command\" arm; got: {output:?}"
    );
    assert!(
        !output.contains("unknown command: /my-command"),
        "expected the user-scope discovered /my-command command to still be recognized \
         alongside project-scope discovery; got: {output:?}"
    );

    assert_eq!(
        received_requests.len(),
        2,
        "expected exactly two outgoing requests to /chat/completions -- one for /deploy prod, \
         one for /my-command foo; got: {received_requests:?}"
    );
    let first_request_body = String::from_utf8_lossy(&received_requests[0].body).into_owned();
    assert!(
        first_request_body.contains("Deploying: prod"),
        "expected the first outgoing request body to contain the EXPANDED project-scope \
         template text ('Deploying: prod'); got body: {first_request_body:?}"
    );
    let second_request_body = String::from_utf8_lossy(&received_requests[1].body).into_owned();
    assert!(
        second_request_body.contains("Handle: foo"),
        "expected the second outgoing request body to contain the EXPANDED user-scope \
         template text ('Handle: foo'); got body: {second_request_body:?}"
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
    let _ = std::fs::remove_dir_all(&project_dir);
}

/// Ticket 64 (custom-command-project-scope-and-trust-boundary) acceptance
/// test locking in ADR 0014's trust boundary at the full-binary level: a
/// project-scope command body containing a `!`-prefixed line that LOOKS
/// like inline shell-execution syntax must never actually spawn a
/// subprocess -- `CommandRegistry::expand_template` has no shell-execution
/// semantics, so the text is expected to reach the outgoing request body
/// unexpanded, byte-for-byte. Proves this two ways: (1) a marker file the
/// embedded `!touch ...` would have created is asserted absent, and (2) the
/// literal `!touch ...` text is asserted present, unexpanded, in the
/// captured outgoing request body -- ruling out the (wrong) alternate
/// explanation that `/deploy` was simply unrecognized.
#[tokio::test]
async fn project_scope_command_containing_bang_prefixed_syntax_expands_to_inert_text_with_no_process_spawned(
) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let canned_response = "MockedAssistantReplyForBangPrefixTrustBoundaryTesting";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-bang-prefix-trust-boundary",
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

    let home = unique_temp_dir("home-bang-prefix-trust-boundary");
    let xdg_config_home = unique_temp_dir("xdg-config-home-bang-prefix-trust-boundary");
    let project_dir = unique_temp_dir("bang-prefix-trust-boundary-project");
    let marker_path = unique_temp_dir("bang-marker-parent").join("should-never-be-created.marker");

    let project_commands_dir = project_dir.join(".rokr").join("commands");
    std::fs::create_dir_all(&project_commands_dir)
        .expect("failed to create fixture project-scope commands directory");
    std::fs::write(
        project_commands_dir.join("deploy.md"),
        format!(
            "---\ndescription: test\n---\nDeploy step: !touch {} && echo pwned",
            marker_path.display()
        ),
    )
    .expect("failed to write fixture deploy.md");

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
    cmd.env("ROKR_OPENAI_API_KEY", "test-openai-api-key");
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
        .write_all(b"/deploy\r")
        .expect("failed to write /deploy invocation to pty");

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
        "expected pty output to contain the mocked assistant response after /deploy, proving \
         the normal submit path still completed; got: {output:?}"
    );

    assert!(
        !marker_path.exists(),
        "expected the '!touch ...' text in the command template to NEVER have been executed as \
         a subprocess -- found marker file at {marker_path:?}, proving a shell command was \
         spawned"
    );

    let received_requests = mock_server.received_requests().await.expect(
        "mock server should have recorded received requests \
         (make sure the request recorder wasn't disabled)",
    );
    assert!(
        !received_requests.is_empty(),
        "expected at least one outgoing request to /chat/completions"
    );
    let first_request_body = String::from_utf8_lossy(&received_requests[0].body).into_owned();
    assert!(
        first_request_body.contains("Deploy step: !touch")
            && first_request_body.contains("&& echo pwned"),
        "expected the outgoing request body to contain the LITERAL, unexpanded '!'-prefixed \
         text, proving it reached the model as inert text rather than being executed or \
         stripped; got body: {first_request_body:?}"
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
    let _ = std::fs::remove_dir_all(&project_dir);
    let _ = std::fs::remove_dir_all(marker_path.parent().unwrap());
}

/// Ticket 65 (skills-instruction-bundle-loading) acceptance test: a
/// project-scope command template containing an `@skill:<name>` mention
/// must have that mention resolved to the NAMED skill's full markdown file
/// contents, inlined in place of the mention, in the outgoing prompt --
/// proven end-to-end through the real binary (PTY), not just at the
/// `CommandRegistry` unit level (`commands.rs`'s
/// `skill_mention_resolves_to_named_skill_file_contents_from_scoped_directory`).
/// The skill file lives in the project-scope `skills/` directory
/// (`<project_dir>/.rokr/skills/code-style.md`), sibling to
/// `.rokr/commands/`, mirroring how project-scope commands are discovered.
#[tokio::test]
async fn command_template_with_skill_mention_inlines_skill_file_contents_in_outgoing_prompt() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let canned_response = "MockedAssistantReplyForSkillMentionTesting";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-skill-mention",
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

    let home = unique_temp_dir("home-skill-mention");
    let xdg_config_home = unique_temp_dir("xdg-config-home-skill-mention");
    let project_dir = unique_temp_dir("skill-mention-project");

    let project_commands_dir = project_dir.join(".rokr").join("commands");
    std::fs::create_dir_all(&project_commands_dir)
        .expect("failed to create fixture project-scope commands directory");
    std::fs::write(
        project_commands_dir.join("review.md"),
        "---\ndescription: test\n---\nFollow @skill:code-style",
    )
    .expect("failed to write fixture review.md");

    let project_skills_dir = project_dir.join(".rokr").join("skills");
    std::fs::create_dir_all(&project_skills_dir)
        .expect("failed to create fixture project-scope skills directory");
    let skill_content = "SKILL-CONTENT-MARKER: use 4-space indentation and trailing commas.";
    std::fs::write(project_skills_dir.join("code-style.md"), skill_content)
        .expect("failed to write fixture code-style.md");

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
    cmd.env("ROKR_OPENAI_API_KEY", "test-openai-api-key");
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
        .write_all(b"/review\r")
        .expect("failed to write /review invocation to pty");

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
        "expected pty output to contain the mocked assistant response after /review, proving \
         the normal submit path still completed; got: {output:?}"
    );

    let received_requests = mock_server.received_requests().await.expect(
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
         in place of the @skill:code-style mention; got body: {first_request_body:?}"
    );
    assert!(
        !first_request_body.contains("@skill:code-style"),
        "expected the @skill:code-style mention to have been replaced, not left literal; got \
         body: {first_request_body:?}"
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
    let _ = std::fs::remove_dir_all(&project_dir);
}

/// F-007 (PRD story 10, pre-ship review) acceptance test: `@skill:<name>`
/// mentions must resolve in a PLAIN, directly-typed prompt too, not just
/// inside a command template's expansion (proven for the template case
/// above by `command_template_with_skill_mention_inlines_skill_file_contents_in_outgoing_prompt`).
/// Types a bare prompt (no leading `/`, never touches `CommandRegistry::expand`
/// at all) containing an `@skill:code-style` mention, and inspects the
/// wire-level outgoing request body the mock provider transport actually
/// received to confirm the skill file's full contents landed there.
#[tokio::test]
async fn plain_prompt_with_skill_mention_inlines_skill_file_contents_in_outgoing_request() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let canned_response = "MockedAssistantReplyForPlainPromptSkillMentionTesting";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-plain-prompt-skill-mention",
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

    let home = unique_temp_dir("home-plain-prompt-skill-mention");
    let xdg_config_home = unique_temp_dir("xdg-config-home-plain-prompt-skill-mention");

    // User-scope skills/ directory (config_dir/skills/), NOT project-scope
    // -- a plain typed prompt has no command template or project cwd
    // involved at all, so this exercises the user-scope skill discovery
    // path on its own.
    let user_skills_dir = xdg_config_home.join("rokr").join("skills");
    std::fs::create_dir_all(&user_skills_dir)
        .expect("failed to create fixture user-scope skills directory");
    let skill_content = "SKILL-CONTENT-MARKER: plain-prompt skill resolution, 4-space indent.";
    std::fs::write(user_skills_dir.join("code-style.md"), skill_content)
        .expect("failed to write fixture code-style.md");

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
    cmd.env("ROKR_OPENAI_API_KEY", "test-openai-api-key");

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
        .write_all(b"Please follow @skill:code-style\r")
        .expect("failed to write plain prompt with skill mention to pty");

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
        "expected pty output to contain the mocked assistant response, proving the plain prompt \
         still submitted through the ordinary path; got: {output:?}"
    );

    let received_requests = mock_server.received_requests().await.expect(
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
         in place of the @skill:code-style mention in a PLAIN (non-/command) prompt; got body: \
         {first_request_body:?}"
    );
    assert!(
        !first_request_body.contains("@skill:code-style"),
        "expected the @skill:code-style mention to have been replaced, not left literal; got \
         body: {first_request_body:?}"
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

/// Ticket 72 (`tui-session-allowlist-grant`): pressing `r` ("allow and
/// remember") at a gated `bash` call's permission prompt must record a
/// session grant (via `rokr_app::permission_policy::SessionGrants`,
/// threaded through `SessionRunner`) so a SECOND gated call to the SAME
/// tool -- chained within the same submission's `run_tool_loop`, once the
/// first tool result feeds back -- is auto-allowed without ever rendering a
/// second permission prompt. Mirrors
/// `bash_tool_call_renders_permission_prompt_and_runs_on_accept`'s PTY
/// structure exactly, with three chained mock responses instead of two: one
/// submitted prompt can trigger multiple sequential tool calls before the
/// final reply, since `run_tool_loop` keeps calling the provider until it
/// gets a tool-call-free response -- so this is ONE submission with two
/// chained `bash` calls, not two separate prompts.
#[tokio::test]
async fn second_gated_call_to_same_tool_after_remember_choice_never_reprompts() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let final_reply_text = "FinalReplyAfterRememberedSecondBashForTesting";

    let temp_dir = unique_temp_dir("bash-remember-target");
    let marker_one_path = temp_dir.join("remember-marker-one");
    let marker_two_path = temp_dir.join("remember-marker-two");
    let marker_one_str = marker_one_path.to_string_lossy().into_owned();
    let marker_two_str = marker_two_path.to_string_lossy().into_owned();
    let bash_command_one = format!("touch {marker_one_str}");
    let bash_command_two = format!("touch {marker_two_str}");

    // 1st request: the model asks to invoke `bash` for the FIRST marker.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-remember-bash-1",
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
                                    "arguments": serde_json::json!({ "command": bash_command_one }).to_string()
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

    // 2nd request: the first tool result feeds back, and the SAME
    // submission's tool loop immediately asks to invoke `bash` again, this
    // time for the SECOND marker -- chained within one `run_tool_loop`
    // call, not a second submitted prompt.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-remember-bash-2",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "tool_calls": [
                            {
                                "id": "call_2",
                                "type": "function",
                                "function": {
                                    "name": "bash",
                                    "arguments": serde_json::json!({ "command": bash_command_two }).to_string()
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

    // 3rd request: the second tool result feeds back, and the model gives
    // a final, tool-call-free reply.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-remember-bash-final",
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

    let home = unique_temp_dir("home-bash-remember");
    let xdg_config_home = unique_temp_dir("xdg-config-home-bash-remember");

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
    cmd.cwd(&temp_dir);
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

    // Wait for the FIRST permission prompt to render.
    let prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("bash") && output.contains("remember-marker-one") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains("bash") && output.contains("remember-marker-one"),
        "expected pty output to contain the first permission prompt (tool name + first \
         marker's command), got: {output:?}"
    );
    assert!(
        !marker_one_path.exists(),
        "the first bash command must not have run before permission was granted"
    );

    // Press `r`: allow AND remember, not plain `y`.
    writer
        .write_all(b"r")
        .expect("failed to write allow-and-remember keypress to pty");

    // Poll in a bounded window until the final reply appears, WHILE
    // actively asserting the second call's tell-tale text (its distinct
    // marker filename, which would only ever appear inside a rendered
    // permission-prompt line) never shows up -- proving the second bash
    // call never re-prompted, not merely that the run eventually finished.
    let final_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        assert!(
            !output.contains("remember-marker-two"),
            "the second bash call's command must never appear in the rendered output -- that \
             would mean a second permission prompt was shown instead of the grant \
             auto-allowing it. Output so far: {output:?}"
        );
        if output.contains(final_reply_text) {
            break;
        }
        if Instant::now() > final_deadline {
            panic!(
                "final reply did not appear within timeout waiting for the auto-allowed \
                 second bash call to complete; output so far: {output:?}"
            );
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        output.contains(final_reply_text),
        "expected pty output to contain the final assistant reply after the chained, \
         auto-allowed second bash call, got: {output:?}"
    );
    assert!(
        marker_one_path.exists(),
        "expected the first bash command to have run (marker file created) after pressing r"
    );
    assert!(
        marker_two_path.exists(),
        "expected the second bash command to have actually run (marker file created), proving \
         the tool loop executed it via the auto-allowed grant rather than silently skipping it"
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

/// Ticket 74 (`subagent-permission-queue-serialization`), acceptance test,
/// second half of the acceptance criterion (the first half -- "no
/// cross-talk between two concurrent subagents' own requests" -- is covered
/// by `crates/rokr-app/src/subagent.rs`'s
/// `concurrent_permission_requests_from_two_subagents_each_receive_their_
/// own_correct_response`): under a session-wide auto-accept grant (ticket
/// 72), two CONCURRENT subagent gated-tool calls to the SAME tool must
/// never populate the permission-prompt queue at all.
///
/// **Deviation from the ticket's literal "real PTY-driven TUI run" sketch**
/// -- documented here per this ticket's own escape hatch, mirroring
/// `subagent.rs`'s `two_concurrent_subagent_tool_calls_in_one_reply_are_
/// dispatched_concurrently` precedent (ADR 0017 decision 5) for the general
/// "explain the deviation" norm:
///
/// The real, unmodified `SubagentTool::execute_boxed` hardcodes a
/// subagent's tool set to exactly `[read, glob, grep, ls]` (see that
/// struct's own doc comment in `crates/rokr-app/src/subagent.rs`) -- NONE
/// of which is `PreviewableTool`/gated. A real subagent invoked through the
/// compiled `rokr` binary can therefore structurally never trigger ANY
/// permission prompt today, PTY or otherwise -- there is no gated tool in
/// its roster to call. This ticket's own files-touched list and process
/// notes are explicit that widening that production roster to include a
/// gated tool by default is OUT of scope (a materially bigger behavior
/// change than this ticket asks for). A consequence: a PTY-driven version
/// of this test would be unable to go RED even before this ticket's fix --
/// it would show zero prompts both before and after, since there would
/// never be anything for a subagent to gate on either way. That provides no
/// signal, so it isn't a meaningful acceptance test regardless of how it's
/// written.
///
/// Instead, this test drives `rokr_app::subagent::run_subagent` directly --
/// made `pub` by this ticket specifically for this purpose (see its own doc
/// comment) -- with a small injected gated test tool, the exact same
/// pattern this ticket's sibling test in `subagent.rs` uses for the
/// "no cross-talk" half. This still exercises the REAL production
/// `run_subagent` / `PermissionPolicy` / `SessionGrants` code that
/// `runner.rs` wires a live `SubagentTool` up to (see `SubagentTool`'s and
/// `run_subagent`'s own doc comments for how `session_grants` flows through
/// unchanged from `SessionRunner`) -- only the render loop's PTY rendering
/// itself is out of reach here, and that portion (a session-wide grant
/// suppressing a SECOND prompt for the PARENT's own gated calls) is already
/// covered end-to-end over a real PTY by
/// `second_gated_call_to_same_tool_after_remember_choice_never_reprompts`
/// immediately above, which shares this exact same `PermissionPolicy`/
/// `SessionGrants` machinery.
///
/// Structured as two phases sharing one responder/channel so the test
/// carries its own red evidence inline, per this ticket's process notes
/// ("Red: force the policy short-circuit off... to show prompts WOULD
/// render absent the fix"): phase A (no grant established yet) proves the
/// channel genuinely DOES receive one request per concurrent subagent call
/// absent a grant -- i.e. this harness is real, not a tautology that always
/// reads zero. Phase B (the SAME tool now granted session-wide, mirroring
/// what pressing 'r' at a real prompt records) then proves the request
/// count on that SAME channel does NOT increase for two more concurrent
/// subagent calls to the same tool, while both calls still complete
/// successfully -- proving the tool actually ran (auto-approved), not that
/// the calls silently failed or hung.
#[tokio::test]
async fn concurrent_subagents_under_session_wide_auto_accept_grant_never_populate_the_permission_prompt_queue(
) {
    #[derive(Debug)]
    struct AcceptanceStubError;

    impl std::fmt::Display for AcceptanceStubError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "acceptance stub error")
        }
    }

    /// Mirrors `subagent.rs`'s own private `ScriptedProvider` test fake,
    /// duplicated here rather than shared (it's `#[cfg(test)]`-private to
    /// that crate, unreachable from this integration test binary).
    struct AcceptanceScriptedProvider {
        replies: std::sync::Mutex<std::collections::VecDeque<rokr_core::Message>>,
    }

    impl rokr_core::Provider for AcceptanceScriptedProvider {
        type Error = AcceptanceStubError;

        async fn send(
            &self,
            _messages: &[rokr_core::Message],
            _tools: &[rokr_core::ToolSpec],
        ) -> Result<(rokr_core::Message, rokr_core::Usage), AcceptanceStubError> {
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(AcceptanceStubError)
                .map(|message| (message, rokr_core::Usage::default()))
        }
    }

    /// Mirrors `subagent.rs`'s own private `FakeGatedTool` test fake,
    /// implemented directly against `rokr_core::ExecutableTool` (skipping
    /// the `rokr_tools::Tool`/`PreviewableTool` bridge that fake uses,
    /// since that bridge buys nothing extra for this test).
    struct AcceptanceFakeGatedTool;

    impl rokr_core::ExecutableTool for AcceptanceFakeGatedTool {
        fn name(&self) -> &str {
            "fake_gated"
        }

        fn to_tool_spec(&self) -> rokr_core::ToolSpec {
            rokr_core::ToolSpec {
                name: "fake_gated".to_string(),
                description: "fake gated tool for acceptance test".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                cache_control: None,
            }
        }

        fn execute_boxed<'a>(
            &'a self,
            _input: serde_json::Value,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<String, rokr_tools::ToolError>> + Send + 'a,
            >,
        > {
            Box::pin(async move { Ok("executed".to_string()) })
        }

        fn preview(
            &self,
            _input: serde_json::Value,
        ) -> Option<Result<rokr_core::PermissionPayload, rokr_tools::ToolError>> {
            Some(Ok(rokr_core::PermissionPayload::Command(
                "fake command".to_string(),
            )))
        }
    }

    fn tool_call_reply(call_id: &str) -> rokr_core::Message {
        rokr_core::Message {
            role: rokr_core::Role::Assistant,
            content: vec![rokr_core::ContentBlock::ToolUse {
                id: call_id.to_string(),
                name: "fake_gated".to_string(),
                input: serde_json::json!({}),
                cache_control: None,
            }],
        }
    }

    let gated_tool = AcceptanceFakeGatedTool;
    let tools: [&dyn rokr_core::ExecutableTool; 1] = [&gated_tool];

    let (tx, mut rx) = tokio::sync::mpsc::channel::<(
        rokr_core::PermissionRequest,
        tokio::sync::oneshot::Sender<bool>,
    )>(8);

    // A single responder drains the whole channel for the life of the
    // test, recording every request it ever sees and auto-approving it --
    // so "how many requests arrived" is just this vec's length at any
    // point, regardless of which phase produced them.
    let received: std::sync::Arc<std::sync::Mutex<Vec<rokr_core::PermissionRequest>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let received_writer = received.clone();
    tokio::spawn(async move {
        while let Some((request, responder)) = rx.recv().await {
            received_writer.lock().unwrap().push(request);
            let _ = responder.send(true);
        }
    });

    let request_permission: rokr_app::subagent::PermissionCallback = Box::new(move |request| {
        let tx = tx.clone();
        Box::pin(async move {
            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
            if tx.send((request, resp_tx)).await.is_err() {
                return false;
            }
            resp_rx.await.unwrap_or(false)
        })
    });
    // R-002 (post-round-1 re-critique, major): this acceptance test never
    // exercises a `Deny`-mode denial (both phases use `permission_mode:
    // None`), so a no-op is correct here -- `note_denied_without_prompt` is
    // exercised by `subagent::tests::subagent_deny_mode_never_reaches_
    // request_permission_callback` instead.
    let note_denied: rokr_app::subagent::NoteDeniedCallback = Box::new(|| {});

    // Phase A: no grant established yet.
    let ungranted_session_grants = std::sync::Arc::new(std::sync::Mutex::new(
        rokr_app::permission_policy::SessionGrants::new(),
    ));

    let provider_a1 = AcceptanceScriptedProvider {
        replies: std::sync::Mutex::new(std::collections::VecDeque::from([
            tool_call_reply("call_a1"),
            rokr_core::Message::assistant_text("a1 done"),
        ])),
    };
    let provider_a2 = AcceptanceScriptedProvider {
        replies: std::sync::Mutex::new(std::collections::VecDeque::from([
            tool_call_reply("call_a2"),
            rokr_core::Message::assistant_text("a2 done"),
        ])),
    };

    let (a1_result, a2_result) = tokio::join!(
        rokr_app::subagent::run_subagent(
            &provider_a1,
            "you are a test subagent",
            "phase a, subagent 1".to_string(),
            &tools,
            "phase-a-one",
            &request_permission,
            &note_denied,
            &ungranted_session_grants,
            None,
        ),
        rokr_app::subagent::run_subagent(
            &provider_a2,
            "you are a test subagent",
            "phase a, subagent 2".to_string(),
            &tools,
            "phase-a-two",
            &request_permission,
            &note_denied,
            &ungranted_session_grants,
            None,
        ),
    );

    assert_eq!(
        a1_result.expect("phase a subagent 1 should succeed"),
        "a1 done"
    );
    assert_eq!(
        a2_result.expect("phase a subagent 2 should succeed"),
        "a2 done"
    );

    let phase_a_count = received.lock().unwrap().len();
    assert_eq!(
        phase_a_count, 2,
        "red evidence: absent a session-wide grant, both concurrent subagents' gated calls \
         must reach the permission-prompt channel -- got {phase_a_count} (expected 2). If this \
         is 0, this test harness itself isn't exercising the permission path at all, and the \
         phase B assertion below would be meaningless."
    );

    // Phase B: the SAME tool now granted session-wide, mirroring what
    // pressing 'r' at a real prompt records (ticket 72).
    let granted_session_grants = std::sync::Arc::new(std::sync::Mutex::new({
        let mut grants = rokr_app::permission_policy::SessionGrants::new();
        grants.grant("fake_gated");
        grants
    }));

    let provider_b1 = AcceptanceScriptedProvider {
        replies: std::sync::Mutex::new(std::collections::VecDeque::from([
            tool_call_reply("call_b1"),
            rokr_core::Message::assistant_text("b1 done"),
        ])),
    };
    let provider_b2 = AcceptanceScriptedProvider {
        replies: std::sync::Mutex::new(std::collections::VecDeque::from([
            tool_call_reply("call_b2"),
            rokr_core::Message::assistant_text("b2 done"),
        ])),
    };

    let (b1_result, b2_result) = tokio::join!(
        rokr_app::subagent::run_subagent(
            &provider_b1,
            "you are a test subagent",
            "phase b, subagent 1".to_string(),
            &tools,
            "phase-b-one",
            &request_permission,
            &note_denied,
            &granted_session_grants,
            None,
        ),
        rokr_app::subagent::run_subagent(
            &provider_b2,
            "you are a test subagent",
            "phase b, subagent 2".to_string(),
            &tools,
            "phase-b-two",
            &request_permission,
            &note_denied,
            &granted_session_grants,
            None,
        ),
    );

    assert_eq!(
        b1_result.expect("phase b subagent 1 should succeed once the grant auto-approves it"),
        "b1 done"
    );
    assert_eq!(
        b2_result.expect("phase b subagent 2 should succeed once the grant auto-approves it"),
        "b2 done"
    );

    let phase_b_count = received.lock().unwrap().len();
    assert_eq!(
        phase_b_count, phase_a_count,
        "under a session-wide auto-accept grant for the SAME tool, neither concurrent \
         subagent's gated call should ever reach the permission-prompt channel -- expected the \
         count to stay at {phase_a_count}, got {phase_b_count}"
    );
}

// ---------------------------------------------------------------------
// Ticket 75 (executable-skill-invocation), ADR 0018.
// ---------------------------------------------------------------------

/// ADR 0018 decision 7: the consent prompt shown before a project-scope
/// executable skill's `run:` command ever executes must show the LITERAL
/// `run:` command, the skill's path, and its scope -- no dry-run, no output
/// preview. Mirrors `bash_tool_call_renders_permission_prompt_and_runs_on_accept`'s
/// PTY-driven structure exactly, but the trigger is a plain `@skill:<name>`
/// mention typed directly (mention resolution is pre-submission,
/// deterministic text substitution -- it runs and shows its prompt BEFORE
/// the mock model is ever consulted at all).
#[tokio::test]
async fn executable_skill_consent_prompt_shows_exact_command_before_execution() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    let final_reply_text = "FinalReplyAfterSkillConsentAcceptForTesting";

    let temp_dir = unique_temp_dir("skill-consent-target");
    let marker_path = temp_dir.join("skill-consent-marker");
    let skills_dir = temp_dir.join(".rokr").join("skills");
    std::fs::create_dir_all(&skills_dir).expect("failed to create fixture skills dir");
    let run_command = format!("touch {}", marker_path.to_string_lossy());
    std::fs::write(
        skills_dir.join("deploy.md"),
        format!("---\nrun: {run_command}\n---\nNEVER-PREVIEW-BEFORE-CONSENT-MARKER"),
    )
    .expect("failed to write fixture deploy.md");

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-skill-consent",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": final_reply_text },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-skill-consent");
    let xdg_config_home = unique_temp_dir("xdg-config-home-skill-consent");

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
    cmd.cwd(&temp_dir);
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
        .write_all(b"@skill:deploy\r")
        .expect("failed to write prompt to pty");

    let prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("deploy.md") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    // NOTE: ratatui's terminal-diff renderer positions each wrapped line via
    // its own cursor-move escape sequence, including hard-wrapping a single
    // long token (e.g. the marker's full absolute path) mid-word at the pane
    // width -- so asserting on the full `"touch {full_path}"` string as one
    // contiguous substring of the raw pty byte stream is not reliable, the
    // same reason the pre-existing bash tests in this file check "bash" and
    // the marker's distinctive basename SEPARATELY rather than the whole
    // rendered line. Each check below is a short, single-token fragment
    // unlikely to itself straddle a wrap boundary.
    assert!(
        output.contains("run:"),
        "expected the consent prompt to show the run: field, got: {output:?}"
    );
    assert!(
        output.contains("touch"),
        "expected the consent prompt to show the literal run: command's verb, got: {output:?}"
    );
    assert!(
        output.contains("skill-consent-marker"),
        "expected the consent prompt to show the run: command's target path, got: {output:?}"
    );
    assert!(
        output.contains("deploy.md"),
        "expected the consent prompt to show the skill's path, got: {output:?}"
    );
    assert!(
        output.contains("scope:"),
        "expected the consent prompt to show the skill's scope field, got: {output:?}"
    );
    assert!(
        output.contains("project"),
        "expected the consent prompt to show the skill's scope, got: {output:?}"
    );
    assert!(
        !output.contains("NEVER-PREVIEW-BEFORE-CONSENT-MARKER"),
        "no dry-run/output preview of the skill's markdown body should appear before a decision \
         is made (ADR 0018 decision 7), got: {output:?}"
    );
    assert!(
        !marker_path.exists(),
        "the run: command must not have executed before consent was granted"
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
        "expected pty output to contain the final assistant reply after accepting the skill \
         consent prompt, got: {output:?}"
    );
    assert!(
        marker_path.exists(),
        "expected the run: command to have executed after accepting consent"
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

/// A consented, in-workspace, no-network `run:` command's captured stdout
/// must be inlined into the OUTGOING request in place of the `@skill:<name>`
/// mention -- inspects the real wire-level request body the mock provider
/// transport received (mirrors `headless_test.rs`'s
/// `print_flag_prompt_with_skill_mention_inlines_skill_file_contents_in_outgoing_request`),
/// so the assertion is precise about what actually got sent to the model,
/// not just that something rendered on screen.
#[tokio::test]
async fn executable_skill_command_output_is_inlined_in_place_of_the_mention() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    let final_reply_text = "FinalReplyAfterSkillOutputInlinedForTesting";

    let temp_dir = unique_temp_dir("skill-inline-target");
    let skills_dir = temp_dir.join(".rokr").join("skills");
    std::fs::create_dir_all(&skills_dir).expect("failed to create fixture skills dir");
    let stdout_marker = "SKILL-STDOUT-MARKER-12345";
    std::fs::write(
        skills_dir.join("greet.md"),
        format!("---\nrun: printf {stdout_marker}\n---\nGreet skill body (must not be inlined)."),
    )
    .expect("failed to write fixture greet.md");

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-skill-inline",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": final_reply_text },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-skill-inline");
    let xdg_config_home = unique_temp_dir("xdg-config-home-skill-inline");

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
    cmd.cwd(&temp_dir);
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
        .write_all(b"@skill:greet\r")
        .expect("failed to write prompt to pty");

    let prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("greet.md") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("greet.md"),
        "expected the consent prompt to render before accepting, got: {output:?}"
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
        "expected pty output to contain the final assistant reply, got: {output:?}"
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

    let received_requests = mock_server.received_requests().await.expect(
        "mock server should have recorded received requests (make sure the request recorder \
         wasn't disabled)",
    );
    assert!(
        !received_requests.is_empty(),
        "expected at least one outgoing request to /chat/completions"
    );
    let first_request_body = String::from_utf8_lossy(&received_requests[0].body).into_owned();
    assert!(
        first_request_body.contains(stdout_marker),
        "expected the outgoing request body to contain the run: command's captured stdout \
         inlined in place of the @skill:greet mention; got body: {first_request_body:?}"
    );
    assert!(
        !first_request_body.contains("@skill:greet"),
        "expected the @skill:greet mention to have been replaced, not left literal; got body: \
         {first_request_body:?}"
    );
    assert!(
        !first_request_body.contains("Greet skill body"),
        "expected the skill's markdown BODY (as opposed to its run: command's stdout) to never \
         be inlined for an executable skill; got body: {first_request_body:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&temp_dir);
}

/// Ticket 76 (git-context-snapshot) acceptance test: closely modeled on
/// `agents_md_content_appears_in_outgoing_system_prompt` above -- a real
/// `git init`-ed `project_dir` fixture with a commit and a dirty
/// (untracked) file must produce a "# Git Context" segment in the outgoing
/// system prompt containing the branch name, a dirty indicator, and the
/// recent commit's subject.
#[tokio::test]
async fn git_context_segment_appears_in_system_prompt_inside_a_repo() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    let canned_response = "MockedAssistantReplyForGitContextTesting";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-git-context",
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

    let home = unique_temp_dir("home-git-context");
    let xdg_config_home = unique_temp_dir("xdg-config-home-git-context");
    let project_dir = unique_temp_dir("git-context-project");

    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&project_dir)
            .status()
            .expect("git command should spawn");
        assert!(status.success(), "git {args:?} should succeed");
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(project_dir.join("a.txt"), "hello").unwrap();
    git(&["add", "a.txt"]);
    git(&[
        "commit",
        "-m",
        "DistinctiveGitContextCommitSubjectForTesting",
    ]);
    std::fs::write(project_dir.join("untracked.txt"), "dirty").unwrap();

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
        first_request_body.contains("# Git Context"),
        "expected the outgoing request body to contain a Git Context segment, got: \
         {first_request_body}"
    );
    assert!(
        first_request_body.contains("main"),
        "expected the outgoing request body to contain the branch name 'main', got: \
         {first_request_body}"
    );
    assert!(
        first_request_body.contains("dirty"),
        "expected the outgoing request body to contain a dirty indicator, got: \
         {first_request_body}"
    );
    assert!(
        first_request_body.contains("DistinctiveGitContextCommitSubjectForTesting"),
        "expected the outgoing request body to contain the recent commit subject, got: \
         {first_request_body}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&project_dir);
}

/// Ticket 79 (commit-command) acceptance test: a scripted TUI session where
/// the model writes to a real file this session (mirroring
/// `write_tool_call_captures_pre_image_snapshot_and_appends_checkpoint_record`'s
/// exact write/diff/accept sequence, which is what populates the checkpoint
/// manifest `CommitCandidateSet::distinct_touched_paths` reads from), then
/// the user runs `/commit` and approves. `git log`/`git show` verify the
/// resulting commit contains EXACTLY the candidate path (not the
/// pre-existing tracked file from the initial commit), a Conventional
/// Commits message, and no Co-Authored-By line ever.
#[tokio::test]
async fn commit_command_commits_exactly_the_candidate_paths_with_conventional_commits_message() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    let final_reply_text = "FinalReplyAfterCommitWriteForTesting";

    let project_dir = unique_temp_dir("commit-command-project");
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&project_dir)
            .status()
            .expect("git command should spawn");
        assert!(status.success(), "git {args:?} should succeed");
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(project_dir.join("tracked.txt"), "already committed").unwrap();
    git(&["add", "tracked.txt"]);
    git(&["commit", "-m", "initial commit"]);

    let target_file = project_dir.join("agent-touched.txt");
    let old_content = "preimagebeforeagentwrite";
    let new_content = "postimageafteragentwrite";
    std::fs::write(&target_file, old_content).unwrap();
    let target_path = target_file.to_string_lossy().into_owned();

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-commit-command-write",
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

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-commit-command-write-final",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": final_reply_text },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-commit-command");
    let xdg_config_home = unique_temp_dir("xdg-config-home-commit-command");
    let xdg_data_home = unique_temp_dir("xdg-data-home-commit-command");

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
    cmd.env("XDG_DATA_HOME", &xdg_data_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");
    cmd.cwd(&project_dir);
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
        .write_all(b"writeagenttouchedfile\r")
        .expect("failed to write prompt to pty");

    let prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("-preimagebeforeagentwrite")
            && output.contains("+postimageafteragentwrite")
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("-preimagebeforeagentwrite"),
        "expected pty output to contain the write tool's diff, got: {output:?}"
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
        "expected pty output to contain the final assistant reply after accepting the write, \
         got: {output:?}"
    );

    writer
        .write_all(b"/commit\r")
        .expect("failed to write /commit to pty");

    let commit_prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < commit_prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("agent-touched.txt") && output.contains("chore:") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("agent-touched.txt"),
        "expected the /commit confirmation prompt to list the candidate file, got: {output:?}"
    );
    assert!(
        output.contains("chore:"),
        "expected the /commit confirmation prompt to show a drafted Conventional Commits \
         message, got: {output:?}"
    );

    writer
        .write_all(b"y")
        .expect("failed to write accept keypress for /commit to pty");

    let committed_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < committed_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Committed") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("Committed"),
        "expected /commit to report success after being approved, got: {output:?}"
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

    let log_output = std::process::Command::new("git")
        .args(["log", "--pretty=%s"])
        .current_dir(&project_dir)
        .output()
        .expect("git log should spawn");
    let subjects: Vec<String> = String::from_utf8_lossy(&log_output.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect();
    assert_eq!(
        subjects.len(),
        2,
        "expected exactly one new commit beyond the initial commit, got subjects: {subjects:?}"
    );
    assert!(
        subjects[0].starts_with("chore:"),
        "expected the new commit's subject to be a Conventional Commits message, got: {:?}",
        subjects[0]
    );

    let show_output = std::process::Command::new("git")
        .args(["show", "--name-only", "--pretty=format:", "HEAD"])
        .current_dir(&project_dir)
        .output()
        .expect("git show should spawn");
    let committed_files: Vec<String> = String::from_utf8_lossy(&show_output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();
    assert_eq!(
        committed_files,
        vec!["agent-touched.txt".to_string()],
        "expected the new commit to contain EXACTLY the candidate path, not tracked.txt"
    );

    let full_message_output = std::process::Command::new("git")
        .args(["log", "-1", "--pretty=%B"])
        .current_dir(&project_dir)
        .output()
        .expect("git log should spawn");
    let full_message = String::from_utf8_lossy(&full_message_output.stdout).to_string();
    assert!(
        !full_message.to_ascii_lowercase().contains("co-authored-by"),
        "expected the commit message to never contain a Co-Authored-By line, got: {full_message:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&xdg_data_home);
    let _ = std::fs::remove_dir_all(&project_dir);
}

/// Ticket 79 (commit-command) acceptance test: a file staged BEFORE this
/// session (via a plain `git add`, standing in for a stale `git add` from
/// before rokr started) that is NOT part of this session's candidate set
/// must produce a warning in `/commit`'s confirmation prompt, but must NOT
/// block the commit, and must NOT itself end up in the resulting commit.
#[tokio::test]
async fn commit_command_pre_staged_mismatch_warns_without_blocking() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    let final_reply_text = "MismatchWriteReplyDone";

    let project_dir = unique_temp_dir("commit-command-mismatch-project");
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&project_dir)
            .status()
            .expect("git command should spawn");
        assert!(status.success(), "git {args:?} should succeed");
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(project_dir.join("tracked.txt"), "already committed").unwrap();
    git(&["add", "tracked.txt"]);
    git(&["commit", "-m", "initial commit"]);

    // A file staged BEFORE rokr's session starts -- never touched by rokr,
    // so it will never appear in the candidate set.
    std::fs::write(
        project_dir.join("stale-staged.txt"),
        "staged before session",
    )
    .unwrap();
    git(&["add", "stale-staged.txt"]);

    let target_file = project_dir.join("agent-touched.txt");
    let old_content = "preimagebeforeagentwrite";
    let new_content = "postimageafteragentwrite";
    std::fs::write(&target_file, old_content).unwrap();
    let target_path = target_file.to_string_lossy().into_owned();

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-commit-command-mismatch-write",
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

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-commit-command-mismatch-write-final",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": final_reply_text },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-commit-command-mismatch");
    let xdg_config_home = unique_temp_dir("xdg-config-home-commit-command-mismatch");
    let xdg_data_home = unique_temp_dir("xdg-data-home-commit-command-mismatch");

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
    cmd.env("XDG_DATA_HOME", &xdg_data_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");
    cmd.cwd(&project_dir);
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
        .write_all(b"writeagenttouchedfile\r")
        .expect("failed to write prompt to pty");

    let prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("-preimagebeforeagentwrite")
            && output.contains("+postimageafteragentwrite")
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("-preimagebeforeagentwrite"),
        "expected pty output to contain the write tool's diff, got: {output:?}"
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
        "expected pty output to contain the final assistant reply after accepting the write, \
         got: {output:?}"
    );

    writer
        .write_all(b"/commit\r")
        .expect("failed to write /commit to pty");

    let commit_prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < commit_prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("warning:") && output.contains("stale-staged.txt") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("warning:"),
        "expected /commit's confirmation prompt to warn about the pre-staged mismatch, \
         got: {output:?}"
    );
    assert!(
        output.contains("stale-staged.txt"),
        "expected the mismatch warning to name the pre-staged file, got: {output:?}"
    );
    assert!(
        output.contains("agent-touched.txt"),
        "expected the /commit confirmation prompt to still list the real candidate file \
         alongside the warning, got: {output:?}"
    );

    writer
        .write_all(b"y")
        .expect("failed to write accept keypress for /commit to pty");

    let committed_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < committed_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Committed") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("Committed"),
        "expected /commit to report success (the warning must not block the commit), \
         got: {output:?}"
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

    let show_output = std::process::Command::new("git")
        .args(["show", "--name-only", "--pretty=format:", "HEAD"])
        .current_dir(&project_dir)
        .output()
        .expect("git show should spawn");
    let committed_files: Vec<String> = String::from_utf8_lossy(&show_output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();
    assert_eq!(
        committed_files,
        vec!["agent-touched.txt".to_string()],
        "expected the new commit to contain EXACTLY the candidate path, never the pre-staged \
         mismatch file"
    );

    let staged_output = std::process::Command::new("git")
        .args(["diff", "--cached", "--name-only"])
        .current_dir(&project_dir)
        .output()
        .expect("git diff --cached should spawn");
    let still_staged: Vec<String> = String::from_utf8_lossy(&staged_output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();
    assert_eq!(
        still_staged,
        vec!["stale-staged.txt".to_string()],
        "expected the pre-staged mismatch file to remain staged and uncommitted afterward"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&xdg_data_home);
    let _ = std::fs::remove_dir_all(&project_dir);
}

/// Ticket 80 (pr-command): resolves the ABSOLUTE path to the real `git`
/// binary via the ambient `PATH` at test-run time (`which git`) -- used to
/// build PATH-shim directories below that stay independent of whatever
/// `gh` install state the machine running this suite happens to have.
fn real_git_binary_path() -> PathBuf {
    let output = std::process::Command::new("which")
        .arg("git")
        .output()
        .expect("`which` should spawn");
    assert!(
        output.status.success(),
        "`which git` should find a real git binary on this machine"
    );
    PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
}

/// Ticket 80 (pr-command): builds a temp directory containing ONLY a
/// symlink to the real `git` binary. Setting a spawned `rokr` process's
/// `PATH` to EXACTLY this directory (not appended to the ambient `PATH`)
/// guarantees `gh` is unresolvable regardless of whether the machine
/// running this suite has a real `gh` installed -- a deterministic "gh
/// truly absent" environment, not a hope about CI/dev machine state.
fn path_shim_without_gh() -> PathBuf {
    let shim_dir = unique_temp_dir("pr-command-path-shim-no-gh");
    #[cfg(unix)]
    std::os::unix::fs::symlink(real_git_binary_path(), shim_dir.join("git"))
        .expect("failed to symlink git into PATH shim dir");
    shim_dir
}

/// Ticket 80 (pr-command): the fixed fake PR URL the `gh` stub script
/// (below) prints on success -- distinctive enough it can't plausibly
/// appear in rendered TUI chrome by accident.
const FAKE_GH_PR_URL: &str = "https://github.com/example/fake-repo/pull/42";

/// Ticket 80 (pr-command): same PATH-shim mechanism as
/// `path_shim_without_gh`, but the shim directory ALSO contains a `gh`
/// stub -- a plain, hand-written shell script (not a compiled fixture
/// binary; this ticket's chosen mechanism so no new Cargo.toml [[bin]]
/// target or fixture crate is needed) that recognizes exactly the `pr
/// create --title <t> --body <b>` invocation shape `gh.rs`'s `create_pr`
/// makes and always succeeds, printing `FAKE_GH_PR_URL` to stdout -- so the
/// stubbed-gh acceptance test below proves `/pr` actually reaches and
/// invokes `gh`, without ever touching the network or creating a real PR.
fn path_shim_with_stubbed_gh() -> PathBuf {
    let shim_dir = unique_temp_dir("pr-command-path-shim-stubbed-gh");
    #[cfg(unix)]
    std::os::unix::fs::symlink(real_git_binary_path(), shim_dir.join("git"))
        .expect("failed to symlink git into PATH shim dir");
    let gh_stub_path = shim_dir.join("gh");
    std::fs::write(
        &gh_stub_path,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"pr\" ] && [ \"$2\" = \"create\" ]; then\n  echo \"{FAKE_GH_PR_URL}\"\n  exit 0\nfi\nexit 1\n"
        ),
    )
    .expect("failed to write gh stub script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&gh_stub_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&gh_stub_path, perms).unwrap();
    }
    shim_dir
}

/// Ticket 80 (pr-command) acceptance test: on a protected branch (`main`),
/// `/pr` must refuse outright rather than silently opening a PR against no
/// sensible base, and instead offer to create a new branch (a single
/// confirmation).
#[tokio::test]
async fn pr_command_on_main_refuses_and_offers_branch_creation() {
    let mock_server = wiremock::MockServer::start().await;

    let project_dir = unique_temp_dir("pr-command-main-project");
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&project_dir)
            .status()
            .expect("git command should spawn");
        assert!(status.success(), "git {args:?} should succeed");
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(project_dir.join("tracked.txt"), "initial content").unwrap();
    git(&["add", "tracked.txt"]);
    git(&["commit", "-m", "initial commit"]);

    let short_sha_output = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(&project_dir)
        .output()
        .expect("git rev-parse should spawn");
    let short_sha = String::from_utf8_lossy(&short_sha_output.stdout)
        .trim()
        .to_string();
    let expected_suggested_branch = format!("pr/{short_sha}");

    let home = unique_temp_dir("home-pr-command-main");
    let xdg_config_home = unique_temp_dir("xdg-config-home-pr-command-main");
    let xdg_data_home = unique_temp_dir("xdg-data-home-pr-command-main");
    let path_shim = path_shim_without_gh();

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
    cmd.env("XDG_DATA_HOME", &xdg_data_home);
    cmd.env("PATH", &path_shim);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");
    cmd.cwd(&project_dir);
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
        .write_all(b"/pr\r")
        .expect("failed to write /pr to pty");

    let refusal_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < refusal_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(&expected_suggested_branch) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("main"),
        "expected /pr's refusal prompt to mention the protected branch 'main', got: {output:?}"
    );
    assert!(
        output.contains(&expected_suggested_branch),
        "expected /pr's refusal prompt to offer the suggested branch name {expected_suggested_branch:?}, got: {output:?}"
    );

    writer
        .write_all(b"y")
        .expect("failed to write accept keypress to pty");

    let created_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < created_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("Created") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("Created"),
        "expected /pr to report the new branch was created after accepting, got: {output:?}"
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

    let branch_output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&project_dir)
        .output()
        .expect("git rev-parse should spawn");
    let branch = String::from_utf8_lossy(&branch_output.stdout)
        .trim()
        .to_string();
    assert_eq!(
        branch, expected_suggested_branch,
        "expected the repo to actually be checked out onto the newly created branch"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&xdg_data_home);
    let _ = std::fs::remove_dir_all(&project_dir);
    let _ = std::fs::remove_dir_all(&path_shim);
}

/// Ticket 80 (pr-command) acceptance test: on a feature branch with `gh`
/// stubbed (see `path_shim_with_stubbed_gh`), `/pr` drafts a title/body
/// from the commits since the branch's merge-base with `main`, shows both
/// in the confirmation prompt, and -- once approved -- actually invokes
/// `gh pr create`, reporting the URL the (stubbed) `gh` printed back.
#[tokio::test]
async fn pr_command_drafts_and_confirms_pr_on_feature_branch_with_gh_stubbed() {
    let mock_server = wiremock::MockServer::start().await;

    let project_dir = unique_temp_dir("pr-command-feature-project");
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&project_dir)
            .status()
            .expect("git command should spawn");
        assert!(status.success(), "git {args:?} should succeed");
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(project_dir.join("tracked.txt"), "initial content").unwrap();
    git(&["add", "tracked.txt"]);
    git(&["commit", "-m", "initial commit"]);
    git(&["checkout", "-b", "feature-branch-for-pr-test"]);
    std::fs::write(project_dir.join("feature-one.txt"), "one").unwrap();
    git(&["add", "feature-one.txt"]);
    git(&[
        "commit",
        "-m",
        "DistinctiveFeatureCommitSubjectOneForPrTest",
    ]);
    std::fs::write(project_dir.join("feature-two.txt"), "two").unwrap();
    git(&["add", "feature-two.txt"]);
    git(&[
        "commit",
        "-m",
        "DistinctiveFeatureCommitSubjectTwoForPrTest",
    ]);

    let home = unique_temp_dir("home-pr-command-feature");
    let xdg_config_home = unique_temp_dir("xdg-config-home-pr-command-feature");
    let xdg_data_home = unique_temp_dir("xdg-data-home-pr-command-feature");
    let path_shim = path_shim_with_stubbed_gh();

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
    cmd.env("XDG_DATA_HOME", &xdg_data_home);
    cmd.env("PATH", &path_shim);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");
    cmd.cwd(&project_dir);
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
        .write_all(b"/pr\r")
        .expect("failed to write /pr to pty");

    let confirm_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < confirm_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("DistinctiveFeatureCommitSubjectOneForPrTest")
            && output.contains("DistinctiveFeatureCommitSubjectTwoForPrTest")
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("DistinctiveFeatureCommitSubjectOneForPrTest"),
        "expected /pr's confirmation prompt to draft a title from the first commit since \
         merge-base, got: {output:?}"
    );
    assert!(
        output.contains("DistinctiveFeatureCommitSubjectTwoForPrTest"),
        "expected /pr's confirmation prompt to draft a body listing every commit since \
         merge-base, got: {output:?}"
    );

    writer
        .write_all(b"y")
        .expect("failed to write accept keypress to pty");

    let created_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < created_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains(FAKE_GH_PR_URL) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains(FAKE_GH_PR_URL),
        "expected /pr to report the URL the stubbed gh printed after approval, got: {output:?}"
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
    let _ = std::fs::remove_dir_all(&xdg_data_home);
    let _ = std::fs::remove_dir_all(&project_dir);
    let _ = std::fs::remove_dir_all(&path_shim);
}

/// Ticket 80 (pr-command) acceptance test: with `gh` entirely absent from
/// `PATH` (see `path_shim_without_gh`), `/pr` still drafts and confirms a
/// title/body from real commits, then -- once approved -- must print the
/// drafted title/body plus the EXACT manual `gh pr create` command the
/// user can run themselves, exiting gracefully rather than crashing.
#[tokio::test]
async fn pr_command_with_gh_absent_prints_manual_fallback() {
    let mock_server = wiremock::MockServer::start().await;

    let project_dir = unique_temp_dir("pr-command-gh-absent-project");
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&project_dir)
            .status()
            .expect("git command should spawn");
        assert!(status.success(), "git {args:?} should succeed");
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(project_dir.join("tracked.txt"), "initial content").unwrap();
    git(&["add", "tracked.txt"]);
    git(&["commit", "-m", "initial commit"]);
    git(&["checkout", "-b", "feature-branch-gh-absent"]);
    std::fs::write(project_dir.join("feature-only.txt"), "content").unwrap();
    git(&["add", "feature-only.txt"]);
    git(&["commit", "-m", "DistinctiveGhAbsentCommitSubjectForPrTest"]);

    let home = unique_temp_dir("home-pr-command-gh-absent");
    let xdg_config_home = unique_temp_dir("xdg-config-home-pr-command-gh-absent");
    let xdg_data_home = unique_temp_dir("xdg-data-home-pr-command-gh-absent");
    let path_shim = path_shim_without_gh();

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
    cmd.env("XDG_DATA_HOME", &xdg_data_home);
    cmd.env("PATH", &path_shim);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-api-key");
    cmd.cwd(&project_dir);
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
        .write_all(b"/pr\r")
        .expect("failed to write /pr to pty");

    let confirm_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < confirm_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("DistinctiveGhAbsentCommitSubjectForPrTest") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("DistinctiveGhAbsentCommitSubjectForPrTest"),
        "expected /pr's confirmation prompt to draft a title from the commit since merge-base \
         even with gh absent, got: {output:?}"
    );

    writer
        .write_all(b"y")
        .expect("failed to write accept keypress to pty");

    let fallback_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < fallback_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("installed") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("installed"),
        "expected /pr to report gh as not installed rather than crash, got: {output:?}"
    );
    assert!(
        output.contains("--title"),
        "expected /pr to print the exact manual gh pr create invocation (--title flag) as a fallback, got: {output:?}"
    );
    assert!(
        output.contains("DistinctiveGhAbsentCommitSubjectForPrTest"),
        "expected /pr's manual-fallback output to still include the drafted title/body, got: {output:?}"
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
        "expected rokr to exit cleanly (not crash) after gh-absent fallback, got status: {status:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&xdg_data_home);
    let _ = std::fs::remove_dir_all(&project_dir);
    let _ = std::fs::remove_dir_all(&path_shim);
}

/// Ticket 81 (pre-edit-divergence-note) acceptance test: closely modeled on
/// `edit_tool_call_renders_partial_diff_and_applies_on_accept` above, but the
/// target file is hand-dirtied (an uncommitted change diverging from HEAD,
/// standing in for a user editing the file by hand mid-session) BEFORE the
/// agent's own `edit` tool call. The diff-review permission prompt must
/// carry a one-line divergence note in addition to the usual diff.
#[tokio::test]
async fn edit_permission_prompt_shows_divergence_note_for_user_dirtied_file() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    let final_reply_text = "FinalReplyAfterDivergenceNoteRejectForTesting";

    let project_dir = unique_temp_dir("divergence-note-project");
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&project_dir)
            .status()
            .expect("git command should spawn");
        assert!(status.success(), "git {args:?} should succeed");
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    let target_file = project_dir.join("tracked.txt");
    std::fs::write(&target_file, "headcontentvalue\n").unwrap();
    git(&["add", "tracked.txt"]);
    git(&["commit", "-m", "initial commit"]);

    // Hand-dirty the tracked file AFTER the commit, uncommitted -- this is
    // the "user separately edited by hand mid-session" case (PRD
    // "Divergence safety"), never through rokr's own write/edit tools.
    // Single-token values throughout (no spaces): ratatui only redraws
    // cells that actually changed, so a multi-word phrase's raw ANSI byte
    // stream can have cursor-jump gaps where a space cell was already
    // blank and thus never rewritten -- a literal substring match on the
    // raw pty bytes would then spuriously fail even though the rendered
    // screen is correct (see `agents_md_content_appears_in_outgoing_system_prompt`'s
    // doc comment above for the same convention).
    std::fs::write(&target_file, "userdirtiedvalue\n").unwrap();
    let target_path = target_file.to_string_lossy().into_owned();
    let old_str = "userdirtiedvalue";
    let new_str = "agentreplacedvalue";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-divergence-note",
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

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-divergence-note-final",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": final_reply_text },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-divergence-note");
    let xdg_config_home = unique_temp_dir("xdg-config-home-divergence-note");

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
        .write_all(b"editthedirtiedfile\r")
        .expect("failed to write prompt to pty");

    let prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("-userdirtiedvalue") && output.contains("+agentreplacedvalue") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("-userdirtiedvalue"),
        "expected pty output to contain the edit tool's diff, got: {output:?}"
    );
    assert!(
        output.contains("diverges"),
        "expected the permission prompt to carry a one-line divergence note for a \
         user-dirtied file, got: {output:?}"
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
        "expected pty output to contain the final assistant reply after rejecting the edit, \
         got: {output:?}"
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
    let _ = std::fs::remove_dir_all(&project_dir);
}

/// Ticket 81 (pre-edit-divergence-note) acceptance test: the converse of
/// `edit_permission_prompt_shows_divergence_note_for_user_dirtied_file`
/// above -- the target file is UNTOUCHED since the initial commit (no
/// hand-edit, no divergence from HEAD), so the diff-review permission
/// prompt must show the usual diff with NO divergence note.
#[tokio::test]
async fn edit_permission_prompt_shows_no_note_for_clean_file() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    let final_reply_text = "FinalReplyAfterCleanFileRejectForTesting";

    let project_dir = unique_temp_dir("clean-file-note-project");
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&project_dir)
            .status()
            .expect("git command should spawn");
        assert!(status.success(), "git {args:?} should succeed");
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    let target_file = project_dir.join("tracked.txt");
    std::fs::write(&target_file, "headcontentvalue\n").unwrap();
    git(&["add", "tracked.txt"]);
    git(&["commit", "-m", "initial commit"]);

    // NOT dirtied -- the file on disk still matches HEAD exactly.
    let target_path = target_file.to_string_lossy().into_owned();
    let old_str = "headcontentvalue";
    let new_str = "agenteditedvalue";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-clean-file-note",
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

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test-clean-file-note-final",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": final_reply_text },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("home-clean-file-note");
    let xdg_config_home = unique_temp_dir("xdg-config-home-clean-file-note");

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
        .write_all(b"editthecleanfile\r")
        .expect("failed to write prompt to pty");

    let prompt_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < prompt_deadline {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        if output.contains("-headcontentvalue") && output.contains("+agenteditedvalue") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        output.contains("-headcontentvalue"),
        "expected pty output to contain the edit tool's diff, got: {output:?}"
    );
    assert!(
        !output.contains("diverges"),
        "expected NO divergence note for a clean file that matches HEAD, got: {output:?}"
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
        "expected pty output to contain the final assistant reply after rejecting the edit, \
         got: {output:?}"
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
    let _ = std::fs::remove_dir_all(&project_dir);
}
