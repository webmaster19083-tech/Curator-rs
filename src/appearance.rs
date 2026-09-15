//! Small client-side appearance hint used by Host and Viewer webviews.
//!
//! Curator maps known GTK families to its own accessible palettes; it does
//! not attempt to parse or execute arbitrary GTK stylesheet files.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ClientAppearance {
    pub gtk_name: Option<String>,
    pub prefers_dark: bool,
    pub accent: Option<String>,
    pub font: Option<String>,
}

pub fn client_appearance() -> ClientAppearance {
    let gtk_name = env_value("GTK_THEME").or_else(|| gsettings("gtk-theme"));
    let color_scheme = gsettings("color-scheme");
    let prefers_dark = env_value("CURATOR_PREFER_DARK")
        .map(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "dark"))
        .unwrap_or_else(|| {
            color_scheme
                .as_deref()
                .is_some_and(|value| value.to_ascii_lowercase().contains("dark"))
                || gtk_name
                    .as_deref()
                    .is_some_and(|value| value.to_ascii_lowercase().contains("dark"))
        });
    ClientAppearance {
        gtk_name,
        prefers_dark,
        accent: env_value("CURATOR_GTK_ACCENT").or_else(|| gsettings("accent-color")),
        font: env_value("CURATOR_GTK_FONT").or_else(|| gsettings("font-name")),
    }
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim_matches(['\'', '"']).trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(target_os = "linux")]
fn gsettings(key: &str) -> Option<String> {
    let output = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", key])
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .trim_matches(['\'', '"'])
            .to_string()
    })
}

#[cfg(not(target_os = "linux"))]
fn gsettings(_key: &str) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appearance_payload_is_safe_to_serialize_for_a_webview() {
        let payload = serde_json::to_value(client_appearance()).unwrap();
        assert!(payload.get("prefers_dark").is_some());
    }
}
