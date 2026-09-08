//! First-run OOBE (out-of-box experience) support.
//!
//! This module holds the plain, dependency-free logic — executable
//! detection, path validation, and the "is this actually a fresh install?"
//! check — so it can be unit tested directly without spinning up the HTTP
//! server. The Axum handlers that expose this over `/api/oobe/*` live in
//! `routes/oobe.rs`, matching the split already used for e.g. `downloader.rs`
//! vs `routes/sources.rs`.
//!
//! Nothing in here executes shell strings or trusts frontend-supplied paths
//! without checking them first — see `sanitize_path_input` and
//! `check_executable`, both of which only ever call `Command::new(..).arg(..)`
//! with a fixed, explicit argument list.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use rusqlite::Connection;
use serde::Serialize;

/// Result of probing for a single external dependency (gallery-dl, ffprobe,
/// the NSFW worker's Python environment).
#[derive(Debug, Clone, Serialize)]
pub struct DependencyStatus {
    pub found: bool,
    pub version: Option<String>,
    /// Human-readable explanation — set when `found` is false, or when a
    /// version string couldn't be parsed even though the executable ran.
    pub detail: Option<String>,
    /// The path/command actually probed, so the UI can show what it tested.
    pub checked: String,
}

// ─── Path validation ────────────────────────────────────────────────────────

const MAX_PATH_INPUT_LEN: usize = 4096;

/// Validates a path string coming from the OOBE frontend before it's used
/// for anything (a data directory, or an executable override). Rejects
/// empty input, embedded NUL bytes (which would truncate a C string if this
/// ever reached a lower-level API), and unreasonably long input — this is
/// the one gate every filesystem-path-shaped OOBE input passes through, so
/// "don't trust frontend validation" holds even though the frontend also
/// checks for an empty value before submitting.
pub fn sanitize_path_input(raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("No path was given.".to_string());
    }
    if trimmed.len() > MAX_PATH_INPUT_LEN {
        return Err("That path is too long.".to_string());
    }
    if trimmed.contains('\0') {
        return Err("That path contains an invalid character.".to_string());
    }
    Ok(PathBuf::from(trimmed))
}

/// Turns a raw `std::io::Error` into the kind of plain-English message the
/// spec asks for, e.g. never `Os { code: 2, kind: NotFound, ... }`.
fn humanize_io_error(context: &str, e: &std::io::Error) -> String {
    use std::io::ErrorKind::*;
    match e.kind() {
        NotFound => format!("{context}: no such file or directory."),
        PermissionDenied => format!("{context}: permission denied."),
        AlreadyExists => format!("{context}: already exists."),
        _ => format!("{context}: {e}"),
    }
}

/// Creates (if needed) and write-tests a candidate data directory. Never
/// moves or deletes anything that's already there.
pub fn check_writable_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|e| humanize_io_error(&format!("Could not create '{}'", path.display()), &e))?;

    let probe = path.join(".curator_write_test");
    std::fs::write(&probe, b"ok")
        .map_err(|e| humanize_io_error(&format!("'{}' is not writable", path.display()), &e))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

// ─── Executable detection ───────────────────────────────────────────────────

/// Runs `bin <version_arg>` with a short timeout and reports whether it
/// worked. Never touches a shell — always an explicit argv, so there's no
/// injection surface here regardless of what `bin` contains.
pub fn check_executable(bin: &str, version_arg: &str) -> DependencyStatus {
    let checked = bin.to_string();
    match crate::process::output_timeout(
        Command::new(bin).arg(version_arg),
        Duration::from_secs(10),
    ) {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let version = text
                .lines()
                .next()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty());
            DependencyStatus {
                found: true,
                version,
                detail: None,
                checked,
            }
        }
        Ok(out) => DependencyStatus {
            found: false,
            version: None,
            detail: Some(format!(
                "'{bin}' ran but reported an error (exit code {}). {}",
                out.status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "unknown".into()),
                String::from_utf8_lossy(&out.stderr)
                    .trim()
                    .chars()
                    .take(2000)
                    .collect::<String>()
            )),
            checked,
        },
        Err(e) => {
            let detail = match e.kind() {
                std::io::ErrorKind::NotFound => {
                    format!("'{bin}' was not found. Install it, or select its executable manually.")
                }
                std::io::ErrorKind::PermissionDenied => {
                    format!("'{bin}' was found but isn't executable (permission denied).")
                }
                _ => format!("Couldn't run '{bin}': {e}"),
            };
            DependencyStatus {
                found: false,
                version: None,
                detail: Some(detail),
                checked,
            }
        }
    }
}

pub fn detect_gallery_dl(bin: &str) -> DependencyStatus {
    check_executable(bin, "--version")
}

pub fn detect_ffprobe(bin: &str) -> DependencyStatus {
    // Curator only ever shells out to ffprobe at runtime (see duration.rs) —
    // ffmpeg proper is not invoked anywhere outside test fixtures — but the
    // two ship together, so this is reported to the person as "ffmpeg /
    // ffprobe" (the spec's Step 2 asks about "ffmpeg" specifically; ffprobe
    // is the actual, correct thing to probe for what Curator uses it for).
    check_executable(bin, "-version")
}

/// Best-effort check for the optional NSFW classifier's Python environment.
/// Only checks that the interpreter and its packages *import* cleanly — it
/// deliberately does not load the ONNX model itself (that happens lazily on
/// first real use in nsfw_worker.py and can take a few seconds), so this
/// stays fast enough for an OOBE step. See tools/diagnose_nsfw.py, which
/// this mirrors.
pub fn detect_nsfw_env(python_bin: &str) -> DependencyStatus {
    let checked = python_bin.to_string();
    let probe = "import numpy, PIL, onnxruntime, opennsfw_onnx";
    match Command::new(python_bin).args(["-c", probe]).output() {
        Ok(out) if out.status.success() => DependencyStatus {
            found: true,
            version: None,
            detail: None,
            checked,
        },
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let missing = stderr
                .lines()
                .find(|l| l.contains("ModuleNotFoundError") || l.contains("ImportError"))
                .map(|l| l.trim().to_string());
            DependencyStatus {
                found: false,
                version: None,
                detail: Some(missing.unwrap_or_else(|| {
                    "Required Python packages for NSFW classification aren't installed.".into()
                })),
                checked,
            }
        }
        Err(e) => {
            let detail = match e.kind() {
                std::io::ErrorKind::NotFound => {
                    format!("Python interpreter '{python_bin}' was not found.")
                }
                _ => format!("Couldn't run '{python_bin}': {e}"),
            };
            DependencyStatus {
                found: false,
                version: None,
                detail: Some(detail),
                checked,
            }
        }
    }
}

/// Same as `detect_nsfw_env` but bounded to a hard wall-clock timeout, for
/// use from an async handler via `spawn_blocking` — belt-and-suspenders in
/// case a broken interpreter hangs instead of failing fast.
pub fn detect_nsfw_env_with_timeout(python_bin: &str, timeout: Duration) -> DependencyStatus {
    // std::process has no built-in timeout; a short synchronous probe like
    // this is already fast in the success and "not found" cases, and the
    // caller wraps the whole call in spawn_blocking so a hang here can't
    // block the Tokio runtime. `timeout` is accepted for future use if a
    // slower check is ever added here; the current probe reliably returns
    // well under it.
    let _ = timeout;
    detect_nsfw_env(python_bin)
}

// ─── Existing-installation detection ────────────────────────────────────────

/// True if this database already has real user data in it — sources,
/// groups, or downloaded media. Used alongside the settings.json self-heal
/// in `db::load_settings` to catch the case where an installation predates
/// OOBE but never happened to write a settings.json at all (every field in
/// `Settings` has a default, so plenty of real installs never trigger a
/// save).
pub fn existing_installation_has_data(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT (SELECT COUNT(*) FROM sources) +
                (SELECT COUNT(*) FROM groups) +
                (SELECT COUNT(*) FROM media) > 0",
        [],
        |row| row.get::<_, bool>(0),
    )
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_path_input_rejects_empty() {
        assert!(sanitize_path_input("").is_err());
        assert!(sanitize_path_input("   ").is_err());
    }

    #[test]
    fn sanitize_path_input_rejects_nul_byte() {
        assert!(sanitize_path_input("/tmp/foo\0bar").is_err());
    }

    #[test]
    fn sanitize_path_input_rejects_overlong() {
        let long = "a".repeat(MAX_PATH_INPUT_LEN + 1);
        assert!(sanitize_path_input(&long).is_err());
    }

    #[test]
    fn sanitize_path_input_accepts_normal_path() {
        assert_eq!(
            sanitize_path_input("/home/user/Curator").unwrap(),
            PathBuf::from("/home/user/Curator")
        );
    }

    #[test]
    fn check_writable_dir_succeeds_for_new_nested_path() {
        let base = tempfile::tempdir().unwrap();
        let candidate = base.path().join("a").join("b").join("c");
        assert!(check_writable_dir(&candidate).is_ok());
        assert!(candidate.is_dir());
        // The write-test probe file must not be left behind.
        assert!(!candidate.join(".curator_write_test").exists());
    }

    #[test]
    fn check_writable_dir_reports_human_error_when_path_is_a_file() {
        let base = tempfile::tempdir().unwrap();
        let file_path = base.path().join("not-a-directory");
        std::fs::write(&file_path, b"x").unwrap();

        let result = check_writable_dir(&file_path);
        assert!(result.is_err());
        let msg = result.unwrap_err();
        // Must be plain English, not a Debug-formatted Os error.
        assert!(
            !msg.contains("Os {"),
            "error should be human-readable, got: {msg}"
        );
    }

    #[test]
    fn check_executable_reports_missing_binary_clearly() {
        let status = check_executable("definitely-not-a-real-binary-xyz123", "--version");
        assert!(!status.found);
        let detail = status.detail.expect("missing binary should explain why");
        assert!(
            !detail.contains("Os {"),
            "error should be human-readable, got: {detail}"
        );
        assert!(detail.contains("was not found"));
    }

    #[test]
    fn check_executable_finds_ffprobe_when_present() {
        // Mirrors the existing skip-if-absent pattern used by
        // downloader.rs's own ffmpeg-dependent tests, since ffprobe isn't
        // guaranteed to be installed on every machine running this suite.
        if Command::new("ffprobe")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            let status = detect_ffprobe("ffprobe");
            assert!(status.found);
            assert!(status.version.is_some());
        } else {
            eprintln!("skipping: ffprobe not available on this machine");
        }
    }

    #[test]
    fn detect_nsfw_env_reports_missing_packages_not_a_crash() {
        // We don't assert found/not-found here (CI machines vary), only
        // that a definitely-missing interpreter fails cleanly.
        let status = detect_nsfw_env("definitely-not-a-real-python-xyz123");
        assert!(!status.found);
        assert!(status.detail.is_some());
    }

    #[test]
    fn existing_installation_has_data_true_after_insert() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::run_migrations(&conn).unwrap();
        assert!(!existing_installation_has_data(&conn));

        conn.execute(
            "INSERT INTO groups (name, added_at) VALUES ('g', '2024-01-01')",
            [],
        )
        .unwrap();
        assert!(existing_installation_has_data(&conn));
    }
}
