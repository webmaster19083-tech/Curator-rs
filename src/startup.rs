//! Windows login-start registration shared by Settings and first-run setup.

use serde::Serialize;

/// What Settings needs to show after reconciling Curator's stored preference
/// against the actual Windows Run entry. The command itself is returned only
/// to loopback Host clients by the settings route.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StartupRegistration {
    pub supported: bool,
    pub registered: bool,
    /// `registered`, `missing`, `stale`, `unavailable`, or `unsupported`.
    pub state: String,
    pub message: String,
    pub actual_command: Option<String>,
    pub expected_command: Option<String>,
    pub repair_available: bool,
}

/// Register or remove the current Curator executable from the per-user Run
/// key. HKCU requires no elevation and `--background` prevents a login launch
/// from flashing a foreground window before Curator settles into the tray.
#[cfg(windows)]
pub fn set_start_with_windows(enabled: bool) -> Result<(), String> {
    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "Curator";

    let mut command = crate::process::blocking_command("reg.exe");
    if enabled {
        let executable = std::env::current_exe()
            .map_err(|error| format!("Could not find the Curator executable: {error}"))?;
        command.args([
            "add",
            RUN_KEY,
            "/v",
            VALUE_NAME,
            "/t",
            "REG_SZ",
            "/d",
            &run_value(&executable),
            "/f",
        ]);
    } else {
        command.args(["delete", RUN_KEY, "/v", VALUE_NAME, "/f"]);
    }
    let output = crate::process::output_timeout(&mut command, std::time::Duration::from_secs(5))
        .map_err(|error| format!("Could not update Windows startup: {error}"))?;
    // Deleting a missing Run entry already reaches the requested end state.
    if output.status.success() || !enabled {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(if detail.is_empty() {
        "Windows did not accept Curator's startup setting.".to_string()
    } else {
        format!("Windows did not accept Curator's startup setting: {detail}")
    })
}

// These helpers are exercised by unit tests on every platform, but are only
// needed by the production Host process on Windows. Keeping them out of a
// non-Windows release build avoids dead-code failures under CI's `-D warnings`.
#[cfg(any(windows, test))]
fn run_value(executable: &std::path::Path) -> String {
    format!("\"{}\" --background", executable.display())
}

#[cfg(any(windows, test))]
fn command_executable(command: &str) -> Option<String> {
    let command = command.trim();
    if let Some(rest) = command.strip_prefix('"') {
        return rest.split_once('"').map(|(path, _)| path.to_string());
    }
    command.split_whitespace().next().map(ToString::to_string)
}

#[cfg(any(windows, test))]
fn same_path(left: &str, right: &std::path::Path) -> bool {
    left.trim_matches('"')
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

/// Pure classification seam for tests and for a clear stale-path diagnosis.
/// `actual_executable_exists` is supplied by the caller because a Run entry
/// can outlive an uninstalled/moved executable.
#[cfg(any(windows, test))]
pub fn classify_startup_command(
    actual: Option<&str>,
    expected_executable: &std::path::Path,
    actual_executable_exists: bool,
) -> StartupRegistration {
    let expected_command = run_value(expected_executable);
    let Some(actual_command) = actual.map(str::trim).filter(|value| !value.is_empty()) else {
        return StartupRegistration {
            supported: cfg!(windows),
            registered: false,
            state: "missing".into(),
            message:
                "Windows has no Curator startup entry. Enable Start with Windows to repair it."
                    .into(),
            actual_command: None,
            expected_command: Some(expected_command),
            repair_available: true,
        };
    };
    let executable_matches = command_executable(actual_command)
        .is_some_and(|executable| same_path(&executable, expected_executable));
    if executable_matches && actual_executable_exists {
        StartupRegistration {
            supported: cfg!(windows),
            registered: true,
            state: "registered".into(),
            message: "Windows will start this Curator installation after sign-in.".into(),
            actual_command: Some(actual_command.to_string()),
            expected_command: Some(expected_command),
            repair_available: false,
        }
    } else {
        let message = if !actual_executable_exists {
            "Windows startup points to an executable that no longer exists. Enable Start with Windows to repair it."
        } else {
            "Windows startup points to a different Curator executable. Enable Start with Windows to repair it."
        };
        StartupRegistration {
            supported: cfg!(windows),
            registered: false,
            state: "stale".into(),
            message: message.into(),
            actual_command: Some(actual_command.to_string()),
            expected_command: Some(expected_command),
            repair_available: true,
        }
    }
}

#[cfg(windows)]
fn query_run_value() -> Result<Option<String>, String> {
    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "Curator";
    let output = crate::process::output_timeout(
        crate::process::blocking_command("reg.exe").args(["query", RUN_KEY, "/v", VALUE_NAME]),
        std::time::Duration::from_secs(5),
    )
    .map_err(|error| format!("Could not inspect Windows startup: {error}"))?;
    if !output.status.success() {
        // `reg query` uses a non-zero exit for a missing value. That is an
        // expected reconciliation result rather than an error.
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().find_map(|line| {
        line.find("REG_SZ")
            .map(|index| line[index + "REG_SZ".len()..].trim().to_string())
            .filter(|value| !value.is_empty())
    }))
}

#[cfg(windows)]
pub fn inspect_startup_registration() -> StartupRegistration {
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            return StartupRegistration {
                supported: true,
                registered: false,
                state: "unavailable".into(),
                message: format!("Could not locate this Curator executable: {error}"),
                actual_command: None,
                expected_command: None,
                repair_available: false,
            }
        }
    };
    match query_run_value() {
        Ok(actual) => {
            let exists = actual
                .as_deref()
                .and_then(command_executable)
                .is_some_and(|path| std::path::Path::new(&path).is_file());
            classify_startup_command(actual.as_deref(), &executable, exists)
        }
        Err(error) => StartupRegistration {
            supported: true,
            registered: false,
            state: "unavailable".into(),
            message: error,
            actual_command: None,
            expected_command: Some(run_value(&executable)),
            repair_available: false,
        },
    }
}

/// Keep non-Windows/headless builds compatible while retaining the saved
/// preference for a later Windows desktop launch.
#[cfg(not(windows))]
pub fn set_start_with_windows(_enabled: bool) -> Result<(), String> {
    Ok(())
}

#[cfg(not(windows))]
pub fn inspect_startup_registration() -> StartupRegistration {
    StartupRegistration {
        supported: false,
        registered: false,
        state: "unsupported".into(),
        message: "Windows startup registration is available in the local Windows Host app.".into(),
        actual_command: None,
        expected_command: None,
        repair_available: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn startup_command_is_quoted() {
        assert_eq!(
            run_value(std::path::Path::new(
                r"C:\Program Files\Curator\Curator.exe"
            )),
            r#""C:\Program Files\Curator\Curator.exe" --background"#,
        );
    }

    #[test]
    fn stale_or_missing_entries_are_actionable() {
        let expected = std::path::Path::new(r"C:\Program Files\Curator\Curator.exe");
        let missing = classify_startup_command(None, expected, false);
        assert_eq!(missing.state, "missing");
        assert!(missing.repair_available);
        let stale = classify_startup_command(
            Some(r#""C:\Old Curator\Curator.exe" --background"#),
            expected,
            false,
        );
        assert_eq!(stale.state, "stale");
        assert!(!stale.registered);
        let current = classify_startup_command(
            Some(r#""C:\Program Files\Curator\Curator.exe" --background"#),
            expected,
            true,
        );
        assert_eq!(current.state, "registered");
        assert!(current.registered);
    }
}
