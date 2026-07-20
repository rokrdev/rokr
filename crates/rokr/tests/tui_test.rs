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
            panic!(
                "expected sessions directory to exist at {sessions_dir:?}, got error: {err:?}"
            )
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
        rokr_session::SessionRecord::Turn { message, .. } => {
            assert_eq!(
                message.text(),
                "persisttestprompt",
                "expected the persisted Turn record's message text to exactly match the \
                 submitted prompt"
            );
        }
        other => panic!("expected the second session.jsonl record to be a Turn record, got: {other:?}"),
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
        message: rokr_core::Message::user_text("priorturnuniquetoken"),
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
        message: rokr_core::Message::user_text("targetjumpturnzero"),
        usage: rokr_session::UsageRecord {
            input_tokens: 1,
            output_tokens: 1,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        timestamp: "2026-07-20T01:00:01Z".to_string(),
    };
    let target_turn1 = rokr_session::SessionRecord::Turn {
        message: rokr_core::Message::assistant_text("targetjumpturnone"),
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
        message: rokr_core::Message::user_text("targetjumpturntwo"),
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

    let origin_contents_after = std::fs::read_to_string(
        sessions_root.join(&origin_session_id).join("session.jsonl"),
    )
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
    let snapshot_file_name = snapshot_entries[0].file_name().to_string_lossy().into_owned();
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

/// Ticket 39 (rollback-command) acceptance test: `/rollback [turn]` restores
/// every file snapshot captured at or after the target turn index to its
/// pre-image content (verified against the real filesystem), truncates the
/// running transcript to that turn's boundary (verified by inspecting the
/// NEXT turn's actual outgoing request body via
/// `mock_server.received_requests()`, mirroring
/// `auto_compaction_triggers_once_usage_crosses_threshold_and_preserves_recent_turn`'s
/// exact technique for proving transcript content), and appends a
/// `SessionRecord::Rollback` record to `session.jsonl`. Three turns are
/// scripted: turn 0 and turn 1 each perform an ACCEPTED `write` tool call
/// against the same real temp file (mirroring
/// `write_tool_call_captures_pre_image_snapshot_and_appends_checkpoint_record`'s
/// exact write/diff/accept PTY sequence), and turn 2 is a plain
/// tool-call-free chat turn. `/rollback 1` should then restore the file to
/// turn 1's pre-image (the state right before turn 1's write ran, i.e.
/// turn 0's post-write content) and truncate the transcript back to
/// turn_index <= 1, discarding turn 2. A fourth turn is submitted
/// afterward to inspect what actually goes out on the wire.
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

    // Roll back to turn 1: should restore the file to turn 1's pre-image
    // (turn 0's post-write content) and truncate the transcript to discard
    // turn 2.
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
        turn0_content,
        "expected the file to be restored to turn 1's pre-image (turn 0's post-write content) \
         after rolling back to turn 1"
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
        turn0_content,
        "expected the file's final on-disk content (after process exit) to still be turn 1's \
         pre-image"
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
        String::from_utf8_lossy(&received_requests[received_requests.len() - 1].body)
            .into_owned();
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
    let compaction_failure_attempts =
        u64::from(rokr_provider::RetryPolicy::default().max_attempts);
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
            message: rokr_core::Message::user_text("please find zzyzxfindableterm in here"),
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
            message: rokr_core::Message::user_text("unrelated live turn content"),
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
            message: rokr_core::Message::user_text("completely unrelated content"),
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
