//! Windows login-start registration shared by Settings, OOBE, and the tray.

/// Register or remove the current Curator executable from the per-user Run
/// key.  HKCU requires no elevation and `--background` ensures Windows login
/// never flashes the main window before Curator can settle in the tray.
#[cfg(target_os = "windows")]
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
    // Deleting a missing Run entry is already the requested end-state.
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

#[cfg(target_os = "windows")]
fn run_value(executable: &std::path::Path) -> String {
    format!("\"{}\" --background", executable.display())
}

/// Keeping this a no-op outside Windows keeps the cross-platform headless and
/// desktop builds compatible while making the setting useful where it applies.
#[cfg(not(target_os = "windows"))]
pub fn set_start_with_windows(_enabled: bool) -> Result<(), String> {
    Ok(())
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    #[test]
    fn startup_command_is_quoted_and_background_first() {
        assert_eq!(
            run_value(std::path::Path::new(
                r"C:\Program Files\Curator\Curator.exe"
            )),
            r#""C:\Program Files\Curator\Curator.exe" --background"#
        );
    }
}
