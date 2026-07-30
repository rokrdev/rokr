//! Ticket 66 (cargo-dist-release-spine): verifies that the workspace has a
//! working `cargo dist` release configuration -- `dist-workspace.toml`
//! declares `rokr` as the release binary, targets the expected build
//! matrix, and configures the shell + Homebrew installers -- and that
//! `cargo dist plan` actually succeeds against that config.
//!
//! Deliberately dependency-free: `dist-workspace.toml` is read as raw text
//! and checked with `str::contains` rather than parsed with a `toml` crate,
//! per the ticket's files-touched constraints.

use std::path::PathBuf;
use std::process::Command;

/// Path to the workspace root, derived from this crate's manifest dir
/// (`crates/rokr`) by going up two directories.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/rokr should have a parent (crates/)")
        .parent()
        .expect("crates/ should have a parent (workspace root)")
        .to_path_buf()
}

#[test]
fn dist_workspace_toml_declares_rokr_as_release_binary_and_configures_install_and_homebrew_targets(
) {
    let dist_toml_path = workspace_root().join("dist-workspace.toml");
    let contents = std::fs::read_to_string(&dist_toml_path).unwrap_or_else(|e| {
        panic!(
            "expected {} to exist and be readable: {e}",
            dist_toml_path.display()
        )
    });

    // rokr identified as the release binary/workspace member.
    assert!(
        contents.contains("crates/rokr"),
        "expected dist-workspace.toml to declare crates/rokr as a workspace \
         member / release binary, got:\n{contents}"
    );

    // Build matrix: at minimum these three triples must be present.
    for triple in [
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-unknown-linux-gnu",
    ] {
        assert!(
            contents.contains(triple),
            "expected dist-workspace.toml to list target triple {triple}, got:\n{contents}"
        );
    }

    // Installers: shell (curl|sh) and homebrew (tap formula).
    assert!(
        contents.contains("shell"),
        "expected dist-workspace.toml to configure the shell installer, got:\n{contents}"
    );
    assert!(
        contents.contains("homebrew"),
        "expected dist-workspace.toml to configure the homebrew installer, got:\n{contents}"
    );
}

#[test]
fn cargo_dist_plan_succeeds_locally_and_lists_expected_release_targets() {
    let root = workspace_root();
    // Invoke the `dist` binary directly rather than `cargo dist plan`.
    // As of the cargo-dist -> dist rebrand (0.28+), `cargo install
    // cargo-dist --locked` installs a binary literally named `dist`; no
    // `cargo-dist` shim is created, so `cargo dist ...` only works if
    // something manually aliases `cargo-dist` -> `dist` on PATH. `dist
    // plan` is the canonical, portable invocation that works on any
    // machine/CI that just installs cargo-dist, with no extra setup.
    let output = Command::new("dist")
        .arg("plan")
        .current_dir(&root)
        .output()
        .expect("failed to spawn `dist plan` -- is cargo-dist installed (as the `dist` binary)?");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "`dist plan` did not succeed (status: {:?})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status
    );

    for triple in [
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-unknown-linux-gnu",
    ] {
        assert!(
            stdout.contains(triple),
            "expected `dist plan` stdout to mention target triple {triple}\n--- stdout ---\n{stdout}"
        );
    }

    assert!(
        stdout.contains("rokr"),
        "expected `dist plan` stdout to mention the `rokr` release binary\n--- stdout ---\n{stdout}"
    );

    // `dist plan`'s human-readable output lists the concrete artifacts it
    // would produce rather than literally printing the words "shell" /
    // "homebrew" -- so assert on the actual artifact names: a
    // `*-installer.sh` shell/curl install script and a `*.rb` Homebrew
    // formula.
    assert!(
        stdout.contains("-installer.sh"),
        "expected `dist plan` stdout to list a shell installer artifact (*-installer.sh)\n--- stdout ---\n{stdout}"
    );

    assert!(
        stdout.contains("rokr.rb"),
        "expected `dist plan` stdout to list a homebrew formula artifact (rokr.rb)\n--- stdout ---\n{stdout}"
    );
}
