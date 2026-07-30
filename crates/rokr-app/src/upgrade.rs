//! Ticket 67 (self-update-rokr-upgrade): `rokr upgrade` detects a
//! Homebrew-managed install (by resolving the running binary's path against
//! the Homebrew Cellar) and, when found, declines to self-update and directs
//! the user to `brew upgrade` instead -- so the two update paths never race
//! over the same on-disk binary. A non-Homebrew install instead runs an
//! update check/install via `axoupdater`, which consumes the same
//! cargo-dist-published releases ticket 66 (cargo-dist-release-spine) wired
//! up.

use std::path::Path;

/// True iff `exe_path` resolves under a Homebrew Cellar prefix. Homebrew
/// installs a formula's real binary under
/// `<prefix>/Cellar/<formula>/<version>/...` (both the Apple Silicon
/// `/opt/homebrew` prefix and the Intel `/usr/local` prefix funnel through
/// the same `Cellar/` shape) and symlinks it into `<prefix>/bin` --
/// resolving symlinks (see `run`, which canonicalizes the real
/// `std::env::current_exe()` result -- but deliberately NOT the test-only
/// `ROKR_UPGRADE_EXE_PATH_OVERRIDE` path, which is synthetic and may not
/// exist on disk -- before calling this) lands back inside `Cellar/` either
/// way, so a substring check on `/Cellar/` is sufficient without needing to
/// know the exact prefix. Takes the path as a parameter (rather than calling
/// `std::env::current_exe()` itself) so it's unit-testable with a synthetic
/// path -- see `homebrew_managed_install_detected_from_binary_path_under_cellar_prefix`.
pub fn is_homebrew_managed(exe_path: &Path) -> bool {
    exe_path.to_string_lossy().contains("/Cellar/")
}

/// Ticket 67 (self-update-rokr-upgrade): `rokr upgrade`'s entry point,
/// called from `main.rs`'s thin dispatch arm. Resolves the running binary's
/// path (via the test-only `ROKR_UPGRADE_EXE_PATH_OVERRIDE` env var when
/// set -- the real compiled test binary never actually lives under a
/// Homebrew Cellar, so acceptance tests use this to force either branch --
/// otherwise `std::env::current_exe()`) and, when it resolves under a
/// Homebrew Cellar prefix, prints guidance directing the user to `brew
/// upgrade` and exits successfully without attempting any update itself.
///
/// A non-Homebrew install instead runs a real update check via
/// `AxoUpdateChecker` (or, in acceptance tests, `MockUpdateChecker` --
/// selected by the presence of `ROKR_UPGRADE_MOCK_CHECK_OUTCOME`, since the
/// real checker makes a network call against GitHub Releases and tests must
/// not depend on that). `AxoUpdateChecker::check_for_update`'s single call
/// into `axoupdater`'s `run()` covers check, download, verification, and
/// install atomically -- there is no separate "check" then "apply" step
/// here to keep in sync, so this function can never leave a partially
/// replaced binary on disk: `axoupdater` either swaps the whole binary in
/// (verified against cargo-dist-published checksums) or leaves the existing
/// one untouched and reports an error.
pub async fn run() -> std::process::ExitCode {
    let exe_path = match std::env::var("ROKR_UPGRADE_EXE_PATH_OVERRIDE") {
        Ok(raw) => std::path::PathBuf::from(raw),
        Err(_) => std::env::current_exe()
            .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
            .unwrap_or_default(),
    };

    if is_homebrew_managed(&exe_path) {
        println!("rokr is managed by Homebrew; run `brew upgrade rokr` to update.");
        return std::process::ExitCode::SUCCESS;
    }

    let outcome = match std::env::var("ROKR_UPGRADE_MOCK_CHECK_OUTCOME") {
        Ok(raw) => MockUpdateChecker(parse_mock_check_outcome(&raw))
            .check_for_update()
            .await,
        Err(_) => AxoUpdateChecker.check_for_update().await,
    };

    match outcome {
        Ok(UpdateOutcome::UpToDate) => {
            println!("rokr is already up to date.");
            std::process::ExitCode::SUCCESS
        }
        Ok(UpdateOutcome::Updated { new_version }) => {
            println!("Updated rokr to version {new_version}.");
            std::process::ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("failed to check for updates: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// The result of an update check/install attempt against a non-Homebrew
/// install. `axoupdater`'s own `run()` folds "checked, nothing newer" and
/// "checked, downloaded, verified, and installed a newer build" into a
/// single call, so this enum mirrors that same two-outcome shape rather
/// than splitting "check" and "apply" into separate steps this module would
/// have to keep atomic itself.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UpdateOutcome {
    /// No newer release was found; the on-disk binary was left untouched.
    UpToDate,
    /// A newer release was found, downloaded, verified, and installed in
    /// place of the running binary.
    Updated {
        /// The version string of the release that was just installed.
        new_version: String,
    },
}

/// Abstracts "check for (and possibly install) an update" behind a trait so
/// `run()` can be exercised by acceptance tests (`MockUpdateChecker`)
/// without making a real network call against GitHub Releases, while
/// `AxoUpdateChecker` wraps the real `axoupdater` crate for production use.
/// A native `async fn` in the trait (rather than `async-trait`/boxed
/// futures) matches the pattern already established by `Provider` in
/// `crates/rokr-core/src/lib.rs` and `ExecutableTool` in
/// `crates/rokr-tools/src/lib.rs` -- neither implementation here needs to be
/// boxed or made dyn-compatible, so the plain `async fn` is simplest.
trait UpdateChecker {
    async fn check_for_update(&self) -> Result<UpdateOutcome, String>;
}

/// Wraps the real `axoupdater` crate, which consumes the cargo-dist-published
/// GitHub releases ticket 66 (cargo-dist-release-spine) wired up. `run()`
/// alone performs the entire check-download-verify-install sequence, so
/// integrity of the replacement binary is `axoupdater`'s responsibility
/// (cargo-dist checksums) -- this module never hand-rolls any part of that.
struct AxoUpdateChecker;

impl UpdateChecker for AxoUpdateChecker {
    async fn check_for_update(&self) -> Result<UpdateOutcome, String> {
        let mut updater = axoupdater::AxoUpdater::new_for("rokr");
        updater.set_release_source(axoupdater::ReleaseSource {
            release_type: axoupdater::ReleaseSourceType::GitHub,
            owner: "rokrdev".to_string(),
            name: "rokr".to_string(),
            app_name: "rokr".to_string(),
        });
        updater
            .set_current_version(
                axoupdater::Version::parse(env!("CARGO_PKG_VERSION"))
                    .map_err(|err| format!("invalid current version: {err}"))?,
            )
            .map_err(|err| err.to_string())?;

        match updater.run().await {
            Ok(None) => Ok(UpdateOutcome::UpToDate),
            Ok(Some(result)) => Ok(UpdateOutcome::Updated {
                new_version: result.new_version.to_string(),
            }),
            Err(err) => Err(err.to_string()),
        }
    }
}

/// Parses the test-only `ROKR_UPGRADE_MOCK_CHECK_OUTCOME` env var into an
/// `UpdateOutcome`/error. This seam exists purely so acceptance tests (see
/// `crates/rokr/tests/cli_test.rs`) can force each branch of `run()`'s
/// match on `UpdateChecker::check_for_update` without making a real network
/// call against GitHub Releases -- `AxoUpdateChecker` is never used when
/// this env var is set.
fn parse_mock_check_outcome(raw: &str) -> Result<UpdateOutcome, String> {
    if raw == "up-to-date" {
        Ok(UpdateOutcome::UpToDate)
    } else if let Some(version) = raw.strip_prefix("update-available:") {
        Ok(UpdateOutcome::Updated {
            new_version: version.to_string(),
        })
    } else if let Some(message) = raw.strip_prefix("error:") {
        Err(message.to_string())
    } else {
        Err(format!(
            "unrecognized ROKR_UPGRADE_MOCK_CHECK_OUTCOME value: {raw:?}"
        ))
    }
}

/// A test-only `UpdateChecker` that returns a canned outcome instead of
/// calling `axoupdater`, wired in by `run()` when
/// `ROKR_UPGRADE_MOCK_CHECK_OUTCOME` is set. See `parse_mock_check_outcome`
/// for why this seam exists.
struct MockUpdateChecker(Result<UpdateOutcome, String>);

impl UpdateChecker for MockUpdateChecker {
    async fn check_for_update(&self) -> Result<UpdateOutcome, String> {
        self.0.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Ticket 67 RED: `is_homebrew_managed` doesn't exist yet -- this must
    /// fail to compile until it's added. Once it exists, a path resolving
    /// under a Homebrew Cellar prefix (both the classic Apple Silicon
    /// `/opt/homebrew` prefix and the Intel `/usr/local` prefix funnel
    /// through the same `.../Cellar/<formula>/<version>/...` shape) must be
    /// detected as Homebrew-managed, while an unrelated install path (e.g.
    /// a plain `~/.cargo/bin` install) must not be.
    #[test]
    fn homebrew_managed_install_detected_from_binary_path_under_cellar_prefix() {
        let cellar_path = Path::new("/opt/homebrew/Cellar/rokr/1.2.3/bin/rokr");
        assert!(
            is_homebrew_managed(cellar_path),
            "expected a path under /Cellar/ to be detected as Homebrew-managed"
        );

        let intel_cellar_path = Path::new("/usr/local/Cellar/rokr/1.2.3/bin/rokr");
        assert!(
            is_homebrew_managed(intel_cellar_path),
            "expected a path under the Intel Homebrew /usr/local/Cellar/ prefix to be detected as Homebrew-managed"
        );

        let non_cellar_path = Path::new("/Users/bharat/.cargo/bin/rokr");
        assert!(
            !is_homebrew_managed(non_cellar_path),
            "expected a non-Cellar path to NOT be detected as Homebrew-managed"
        );
    }
}
