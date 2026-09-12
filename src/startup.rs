//! Windows login-start registration shared by Settings and first-run setup.

/// Register or remove the current Curator executable from the per-user Run
/// key. HKCU requires no elevation and `--background` prevents a login launch
/// from flashing a foreground window before Curator settles into the tray.
#[cfg(windows)]
pub fn set_start_with_windows(enabled: bool) -> Result<(), String> {
    use std::process::Command;

    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "Curator";

    let mut command = Command::new("reg.exe");
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
    let output = command
        .output()
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

#[cfg(windows)]
fn run_value(executable: &std::path::Path) -> String {
    format!("\"{}\" --background", executable.display())
}

/// Keep non-Windows/headless builds compatible while retaining the saved
/// preference for a later Windows desktop launch.
#[cfg(not(windows))]
pub fn set_start_with_windows(_enabled: bool) -> Result<(), String> {
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn startup_command_is_quoted() {
        assert_eq!(
            run_value(std::path::Path::new(r"C:\Program Files\Curator\Curator.exe")),
            r#""C:\Program Files\Curator\Curator.exe" --background"#,
        );
    }
}
