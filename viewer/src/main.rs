#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

const DEFAULT_PORT: u16 = 42168;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedHost {
    id: String,
    name: String,
    /// The user-entered Tailnet origin. It is revalidated against the local
    /// peer inventory on every connection so a node whose Tailnet address
    /// changes can still be reached by its MagicDNS name.
    endpoint: String,
    /// The numeric Tailnet origin selected during the most recent successful
    /// validation. Browser navigation uses this value rather than resolving
    /// the MagicDNS name a second time.
    #[serde(default)]
    navigation_endpoint: Option<String>,
    instance_id: String,
    edition: String,
    last_tested_at: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct HostStore {
    #[serde(default)]
    hosts: Vec<SavedHost>,
    #[serde(default)]
    last_host_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SystemInfo {
    edition: String,
    api_protocol: String,
    instance_id: String,
    tailnet_only: bool,
}

#[derive(Debug, Serialize)]
struct HostProbe {
    endpoint: String,
    navigation_endpoint: String,
    edition: String,
    instance_id: String,
}

#[derive(Debug, Deserialize)]
struct TailscaleStatus {
    #[serde(rename = "BackendState")]
    backend_state: Option<String>,
    #[serde(rename = "MagicDNSSuffix")]
    magic_dns_suffix: Option<String>,
    #[serde(rename = "Self")]
    self_node: Option<TailscaleNode>,
    #[serde(rename = "Peer", default)]
    peers: HashMap<String, TailscaleNode>,
}

#[derive(Debug, Deserialize)]
struct TailscaleNode {
    #[serde(rename = "DNSName")]
    dns_name: Option<String>,
    #[serde(rename = "HostName")]
    host_name: Option<String>,
    #[serde(rename = "TailscaleIPs", default)]
    tailscale_ips: Vec<String>,
}

#[derive(Debug)]
struct TailnetPeer {
    names: BTreeSet<String>,
    ips: BTreeSet<IpAddr>,
}

fn peer_matches_host(peer: &TailnetPeer, host: &str) -> bool {
    host.parse::<IpAddr>()
        .map(|ip| peer.ips.contains(&ip))
        .unwrap_or_else(|_| peer.names.contains(host))
}

fn resolution_matches_peer(resolved: &[IpAddr], peer: &TailnetPeer) -> bool {
    !resolved.is_empty() && resolved.iter().all(|ip| peer.ips.contains(ip))
}

fn origin_for_tailnet_ip(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(ip) => format!("http://{ip}:{port}"),
        IpAddr::V6(ip) => format!("http://[{ip}]:{port}"),
    }
}

fn hosts_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let directory = app
        .path()
        .app_config_dir()
        .map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    Ok(directory.join("hosts.json"))
}

fn load_hosts(app: &tauri::AppHandle) -> Result<HostStore, String> {
    let path = hosts_path(app)?;
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(HostStore::default()),
        Err(error) => Err(error.to_string()),
    }
}

fn save_hosts(app: &tauri::AppHandle, store: &HostStore) -> Result<(), String> {
    let path = hosts_path(app)?;
    let text = serde_json::to_string_pretty(store).map_err(|error| error.to_string())?;
    std::fs::write(path, text).map_err(|error| error.to_string())
}

fn normalized_endpoint(raw: &str) -> Result<reqwest::Url, String> {
    let value = raw.trim();
    let mut url =
        reqwest::Url::parse(value).map_err(|_| "Enter an http:// Tailnet host URL.".to_string())?;
    if url.scheme() != "http" {
        return Err("Curator Server currently accepts only http:// Tailnet endpoints.".into());
    }
    if url.host_str().is_none() || url.username() != "" || url.password().is_some() {
        return Err("The host URL must not contain credentials and must include a host.".into());
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(
            "Save only the Curator host origin, without a path, query, or fragment.".into(),
        );
    }
    if url.port().is_none() {
        url.set_port(Some(DEFAULT_PORT))
            .map_err(|_| "Could not set the Curator Server port.".to_string())?;
    }
    Ok(url)
}

async fn tailscale_peers() -> Result<Vec<TailnetPeer>, String> {
    let output = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::process::Command::new("tailscale")
            .args(["status", "--json"])
            .output(),
    )
    .await
    .map_err(|_| {
        "Timed out while asking the local Tailscale client for its peer inventory.".to_string()
    })?
    .map_err(|_| "Tailscale is not available on this device.".to_string())?;
    if !output.status.success() {
        return Err("Tailscale did not return a connected peer inventory.".into());
    }
    let status: TailscaleStatus = serde_json::from_slice(&output.stdout)
        .map_err(|_| "Tailscale returned an unreadable peer inventory.".to_string())?;
    if !status
        .backend_state
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("running"))
    {
        return Err("Tailscale is not connected.".into());
    }

    let suffix = status
        .magic_dns_suffix
        .as_deref()
        .map(|value| value.trim_end_matches('.').to_ascii_lowercase());
    let mut nodes = status.peers.into_values().collect::<Vec<_>>();
    if let Some(self_node) = status.self_node {
        nodes.push(self_node);
    }
    let peers = nodes
        .into_iter()
        .filter_map(|node| {
            let ips = node
                .tailscale_ips
                .iter()
                .filter_map(|value| value.parse::<IpAddr>().ok())
                .collect::<BTreeSet<_>>();
            if ips.is_empty() {
                return None;
            }
            let mut names = BTreeSet::new();
            if let Some(name) = node.dns_name {
                names.insert(name.trim_end_matches('.').to_ascii_lowercase());
            }
            if let Some(name) = node.host_name {
                let name = name.to_ascii_lowercase();
                names.insert(name.clone());
                if let Some(suffix) = &suffix {
                    names.insert(format!("{name}.{suffix}"));
                }
            }
            Some(TailnetPeer { names, ips })
        })
        .collect::<Vec<_>>();
    if peers.is_empty() {
        return Err("Tailscale has no usable Tailnet peers.".into());
    }
    Ok(peers)
}

async fn validate_host(raw: &str) -> Result<HostProbe, String> {
    let endpoint = normalized_endpoint(raw)?;
    let endpoint_origin = endpoint.as_str().trim_end_matches('/').to_string();
    let host = endpoint
        .host_str()
        .unwrap()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let peers = tailscale_peers().await?;
    let literal_ip = host.parse::<IpAddr>().ok();
    let peer = peers
        .iter()
        .find(|peer| peer_matches_host(peer, &host))
        .ok_or_else(|| "That host is not in this device's Tailscale peer inventory.".to_string())?;

    let mut resolved = if let Some(ip) = literal_ip {
        vec![ip]
    } else {
        tokio::net::lookup_host((
            host.as_str(),
            endpoint.port_or_known_default().unwrap_or(DEFAULT_PORT),
        ))
        .await
        .map_err(|_| "Could not resolve that Tailnet hostname locally.".to_string())?
        .map(|address| address.ip())
        .collect::<Vec<_>>()
    };
    let mut seen = BTreeSet::new();
    resolved.retain(|ip| seen.insert(*ip));
    if !resolution_matches_peer(&resolved, peer) {
        return Err(
            "The host did not resolve solely to the Tailnet IP reported by Tailscale.".into(),
        );
    }

    // Connect only to the address that was just checked. A normal reqwest
    // request to the MagicDNS name would resolve it again, opening a DNS
    // rebinding window between validation and the system-info handshake.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|error| error.to_string())?;
    let port = endpoint.port_or_known_default().unwrap_or(DEFAULT_PORT);
    let mut connection_errors = Vec::new();
    for ip in resolved {
        let navigation_endpoint = origin_for_tailnet_ip(ip, port);
        let info_url = reqwest::Url::parse(&navigation_endpoint)
            .and_then(|url| url.join("api/system/info"))
            .map_err(|error| error.to_string())?;
        let response = match client.get(info_url).send().await {
            Ok(response) => response,
            Err(error) => {
                connection_errors.push(format!("{ip}: {error}"));
                continue;
            }
        };
        if !response.status().is_success() {
            connection_errors.push(format!(
                "{ip}: returned {} for /api/system/info",
                response.status()
            ));
            continue;
        }
        let info: SystemInfo = match response.json().await {
            Ok(info) => info,
            Err(_) => {
                connection_errors.push(format!("{ip}: invalid Curator system-info response"));
                continue;
            }
        };
        if info.api_protocol != curator::API_PROTOCOL {
            return Err(format!(
                "Protocol mismatch: Viewer expects {}, host provides {}.",
                curator::API_PROTOCOL,
                info.api_protocol
            ));
        }
        if !matches!(info.edition.as_str(), "server" | "host") {
            return Err("That endpoint is not a Curator Server or Host.".into());
        }
        if !info.tailnet_only {
            return Err(
                "That endpoint does not advertise Curator's Tailnet-only listener policy.".into(),
            );
        }
        return Ok(HostProbe {
            endpoint: endpoint_origin,
            navigation_endpoint,
            edition: info.edition,
            instance_id: info.instance_id,
        });
    }
    Err(format!(
        "Could not contact the validated Tailnet address{}: {}",
        if connection_errors.len() == 1 {
            ""
        } else {
            "es"
        },
        connection_errors.join("; ")
    ))
}

#[tauri::command]
fn list_hosts(app: tauri::AppHandle) -> Result<HostStore, String> {
    load_hosts(&app)
}

#[tauri::command]
async fn test_host(endpoint: String) -> Result<HostProbe, String> {
    validate_host(&endpoint).await
}

#[tauri::command]
async fn save_host(
    app: tauri::AppHandle,
    name: String,
    endpoint: String,
) -> Result<SavedHost, String> {
    let probe = validate_host(&endpoint).await?;
    let mut store = load_hosts(&app)?;
    let normalized_name = name.trim();
    if normalized_name.is_empty() {
        return Err("Give this host a short name.".into());
    }
    let now = chrono_like_now();
    let record = SavedHost {
        id: probe.instance_id.clone(),
        name: normalized_name.to_string(),
        endpoint: probe.endpoint,
        navigation_endpoint: Some(probe.navigation_endpoint),
        instance_id: probe.instance_id,
        edition: probe.edition,
        last_tested_at: Some(now),
    };
    if let Some(existing) = store.hosts.iter_mut().find(|host| host.id == record.id) {
        *existing = record.clone();
    } else {
        store.hosts.push(record.clone());
    }
    store.last_host_id = Some(record.id.clone());
    save_hosts(&app, &store)?;
    Ok(record)
}

#[tauri::command]
fn delete_host(app: tauri::AppHandle, id: String) -> Result<(), String> {
    let mut store = load_hosts(&app)?;
    store.hosts.retain(|host| host.id != id);
    if store.last_host_id.as_deref() == Some(id.as_str()) {
        store.last_host_id = None;
    }
    save_hosts(&app, &store)
}

#[tauri::command]
async fn connect_host(app: tauri::AppHandle, id: String) -> Result<(), String> {
    let mut store = load_hosts(&app)?;
    let host = store
        .hosts
        .iter()
        .find(|host| host.id == id)
        .cloned()
        .ok_or_else(|| "Saved host not found.".to_string())?;
    let probe = validate_host(&host.endpoint).await?;
    if probe.instance_id != host.instance_id {
        return Err("This address now identifies a different Curator library; save it as a new host instead.".into());
    }
    store.last_host_id = Some(host.id.clone());
    if let Some(saved) = store.hosts.iter_mut().find(|saved| saved.id == host.id) {
        saved.last_tested_at = Some(chrono_like_now());
        saved.navigation_endpoint = Some(probe.navigation_endpoint.clone());
    }
    save_hosts(&app, &store)?;
    let url = reqwest::Url::parse(&probe.navigation_endpoint).map_err(|error| error.to_string())?;
    app.get_webview_window("main")
        .ok_or_else(|| "Viewer window is not available.".to_string())?
        .navigate(url)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn show_hosts(app: tauri::AppHandle) -> Result<(), String> {
    let url = "tauri://localhost/index.html"
        .parse()
        .map_err(|error| format!("Could not open host picker: {error}"))?;
    app.get_webview_window("main")
        .ok_or_else(|| "Viewer window is not available.".to_string())?
        .navigate(url)
        .map_err(|error| error.to_string())
}

fn chrono_like_now() -> String {
    // Viewer deliberately avoids a second database. Unix seconds are enough
    // for ordering and are formatted as a stable, machine-readable string.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_default()
}

fn viewer_initialization_script() -> String {
    let appearance = serde_json::to_string(&curator::appearance::client_appearance())
        .unwrap_or_else(|_| "{}".to_string());
    format!(
        "window.__CURATOR_RUNTIME__ = 'viewer'; window.__CURATOR_CLIENT_APPEARANCE__ = {appearance};"
    )
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            list_hosts,
            test_host,
            save_host,
            delete_host,
            connect_host,
            show_hosts
        ])
        .setup(|app| {
            // A Viewer window is intentionally navigated to a remote same-
            // origin Curator page after connection. Keep the saved-host picker
            // reachable without restarting so multiple named hosts are useful
            // in a long-running desktop session.
            let choose_host = tauri::menu::MenuItem::with_id(
                app,
                "choose-host",
                "Choose Curator Host",
                true,
                Some("CmdOrCtrl+Shift+H"),
            )?;
            let menu = tauri::menu::Menu::with_items(app, &[&choose_host])?;
            app.set_menu(menu)?;
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("Curator Viewer")
                .inner_size(920.0, 680.0)
                .min_inner_size(600.0, 480.0)
                .initialization_script(viewer_initialization_script())
                .build()?;
            Ok(())
        })
        .on_menu_event(|app, event| {
            if event.id().as_ref() == "choose-host" {
                let _ = show_hosts(app.clone());
            }
        })
        .run(tauri::generate_context!())
        .expect("Curator Viewer failed to start");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_tailnet_shape_before_network_work() {
        assert!(normalized_endpoint("https://example.com").is_err());
        assert!(normalized_endpoint("http://example.com/path").is_err());
        assert_eq!(
            normalized_endpoint("http://host.tailnet.ts.net")
                .unwrap()
                .port(),
            Some(DEFAULT_PORT)
        );
    }

    #[test]
    fn peer_inventory_and_resolution_are_both_required() {
        let peer = TailnetPeer {
            names: ["curator.example.ts.net".to_string()].into_iter().collect(),
            ips: ["100.64.20.4".parse().unwrap()].into_iter().collect(),
        };
        assert!(peer_matches_host(&peer, "curator.example.ts.net"));
        assert!(peer_matches_host(&peer, "100.64.20.4"));
        assert!(!peer_matches_host(&peer, "other.example.ts.net"));
        assert!(resolution_matches_peer(
            &["100.64.20.4".parse().unwrap()],
            &peer
        ));
        assert!(!resolution_matches_peer(
            &["100.64.20.4".parse().unwrap(), "127.0.0.1".parse().unwrap()],
            &peer
        ));
    }

    #[test]
    fn navigation_origin_is_pinned_to_the_validated_tailnet_ip() {
        assert_eq!(
            origin_for_tailnet_ip("100.64.20.4".parse().unwrap(), DEFAULT_PORT),
            "http://100.64.20.4:42168"
        );
        assert_eq!(
            origin_for_tailnet_ip("fd7a:115c:a1e0::123".parse().unwrap(), DEFAULT_PORT),
            "http://[fd7a:115c:a1e0::123]:42168"
        );
    }
}
