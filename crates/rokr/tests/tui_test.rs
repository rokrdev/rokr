use portable_pty::{CommandBuilder, PtySize, native_pty_system};
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
    let dir =
        std::env::temp_dir().join(format!("rokr-tui-test-{label}-{}-{nanos}", std::process::id()));
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
            panic!(
                "rokr did not exit within timeout after pressing q; output so far: {output:?}"
            );
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
