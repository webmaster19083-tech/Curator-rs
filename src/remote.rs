//! The single Curator HTTP listener and runtime remote-access diagnostics.
//!
//! The desktop shell deliberately uses the same Axum router as the headless
//! binary.  Keeping the socket here prevents a second, desktop-only server
//! from drifting away from the API used by LAN and Tailscale clients.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{info, warn};

use crate::AppState;

/// Curator has historically used this port.  Keep it stable for existing LAN
/// bookmarks, firewall rules, and Tailscale clients.
pub const DEFAULT_SERVER_PORT: u16 = 8642;

/// Process-lifetime listener state.  It is deliberately independent of any
/// desktop window so closing/hiding a window can never make remote clients
/// think the service stopped.
#[derive(Default)]
pub struct ServerStatus {
    port: AtomicU16,
    ipv4: AtomicBool,
    ipv6: AtomicBool,
}

impl ServerStatus {
    pub fn new(port: u16) -> Self {
        Self {
            port: AtomicU16::new(port),
            ipv4: AtomicBool::new(false),
            ipv6: AtomicBool::new(false),
        }
    }

    pub fn port(&self) -> u16 {
        self.port.load(Ordering::Acquire)
    }

    pub fn ipv4_running(&self) -> bool {
        self.ipv4.load(Ordering::Acquire)
    }

    pub fn ipv6_running(&self) -> bool {
        self.ipv6.load(Ordering::Acquire)
    }

    pub fn running(&self) -> bool {
        self.ipv4_running() || self.ipv6_running()
    }

    fn set_listener(&self, port: u16, ipv6: bool, running: bool) {
        self.port.store(port, Ordering::Release);
        let flag = if ipv6 { &self.ipv6 } else { &self.ipv4 };
        flag.store(running, Ordering::Release);
    }
}

/// Bind and supervise Curator's one shared HTTP/API server.
///
/// IPv4 wildcard binding is required: it keeps existing LAN and Tailscale
/// IPv4 clients working.  IPv6 is attempted independently so it is available
/// on machines where the OS/network supports it, without making an IPv6-only
/// configuration a startup requirement.
pub async fn start_http_server(state: &AppState) -> Result<u16> {
    start_http_server_on(state, DEFAULT_SERVER_PORT).await
}

/// `port == 0` is useful for focused tests; production always calls
/// [`start_http_server`] and therefore retains port 8642.
pub async fn start_http_server_on(state: &AppState, requested_port: u16) -> Result<u16> {
    if state.remote_server.running() {
        return Ok(state.remote_server.port());
    }

    let ipv4_listener =
        TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, requested_port)))
            .await
            .with_context(|| format!("binding Curator HTTP server on 0.0.0.0:{requested_port}"))?;
    let port = ipv4_listener
        .local_addr()
        .context("reading Curator HTTP listener address")?
        .port();

    state.remote_server.set_listener(port, false, true);
    spawn_listener(state, ipv4_listener, false);

    // On some Windows configurations an IPv6 wildcard socket is dual-stack
    // and this bind reports address-in-use after IPv4 is established.  That is
    // harmless: the IPv4 listener is already live, and we only advertise IPv6
    // candidates when this distinct listener succeeds.
    match TcpListener::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, port))).await {
        Ok(ipv6_listener) => {
            state.remote_server.set_listener(port, true, true);
            spawn_listener(state, ipv6_listener, true);
        }
        Err(error) => {
            info!(
                "Curator IPv6 listener unavailable on [::]:{port}; continuing with IPv4: {error}"
            );
        }
    }

    info!("Curator HTTP server listening on 0.0.0.0:{port}");
    Ok(port)
}

fn spawn_listener(state: &AppState, listener: TcpListener, ipv6: bool) {
    let app = crate::router(state.clone());
    let cancellation = state.shutdown.clone();
    let status = Arc::clone(&state.remote_server);
    state.server_tasks.spawn(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                cancellation.cancelled().await;
            })
            .await;
        if let Err(error) = result {
            warn!("Curator HTTP listener stopped unexpectedly: {error}");
        }
        status.set_listener(status.port(), ipv6, false);
    });
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteAccessInfo {
    pub running: bool,
    pub server: &'static str,
    pub port: u16,
    pub bound_addresses: Vec<String>,
    pub local_urls: Vec<String>,
    pub lan_urls: Vec<String>,
    pub tailscale_urls: Vec<String>,
    pub magicdns_hostname: Option<String>,
}

#[derive(Default)]
struct TailscaleSnapshot {
    addresses: Vec<IpAddr>,
    magicdns_hostname: Option<String>,
}

/// Take a fresh snapshot rather than caching adapter values: Wi-Fi/Ethernet
/// changes and Tailscale reconnects should be reflected while Curator stays in
/// the tray for days.
pub async fn remote_access_info(state: &AppState) -> RemoteAccessInfo {
    let port = state.remote_server.port();
    let ipv4_running = state.remote_server.ipv4_running();
    let ipv6_running = state.remote_server.ipv6_running();
    let running = ipv4_running || ipv6_running;

    let mut bound_addresses = Vec::new();
    let mut local_urls = Vec::new();
    if ipv4_running {
        bound_addresses.push(format!("0.0.0.0:{port}"));
        local_urls.push(format!("http://127.0.0.1:{port}"));
    }
    if ipv6_running {
        bound_addresses.push(format!("[::]:{port}"));
        local_urls.push(format!("http://[::1]:{port}"));
    }

    if !running {
        return RemoteAccessInfo {
            running: false,
            server: "Stopped",
            port,
            bound_addresses,
            local_urls,
            lan_urls: Vec::new(),
            tailscale_urls: Vec::new(),
            magicdns_hostname: None,
        };
    }

    let tailscale = detect_tailscale().await;
    let tailscale_set: HashSet<IpAddr> = tailscale.addresses.iter().copied().collect();
    let lan_urls = detect_lan_addresses()
        .await
        .into_iter()
        .filter(|address| !tailscale_set.contains(address))
        .filter(|address| family_is_listening(*address, ipv4_running, ipv6_running))
        .map(|address| url_for(address, port))
        .collect();

    let mut tailscale_urls: Vec<String> = tailscale
        .addresses
        .iter()
        .copied()
        .filter(|address| family_is_listening(*address, ipv4_running, ipv6_running))
        .map(|address| url_for(address, port))
        .collect();

    let magicdns_hostname = match tailscale.magicdns_hostname {
        Some(hostname) if resolve_magicdns(&hostname, port, ipv4_running, ipv6_running).await => {
            tailscale_urls.push(format!("http://{hostname}:{port}"));
            Some(hostname)
        }
        _ => None,
    };

    RemoteAccessInfo {
        running: true,
        server: "Running",
        port,
        bound_addresses,
        local_urls,
        lan_urls,
        tailscale_urls,
        magicdns_hostname,
    }
}

fn family_is_listening(address: IpAddr, ipv4_running: bool, ipv6_running: bool) -> bool {
    match address {
        IpAddr::V4(_) => ipv4_running,
        IpAddr::V6(_) => ipv6_running,
    }
}

fn url_for(address: IpAddr, port: u16) -> String {
    match address {
        IpAddr::V4(address) => format!("http://{address}:{port}"),
        IpAddr::V6(address) => format!("http://[{address}]:{port}"),
    }
}

async fn command_output(command: &str, args: &[&str]) -> Option<String> {
    let output = timeout(
        Duration::from_secs(2),
        Command::new(command).args(args).output(),
    )
    .await
    .ok()?
    .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn detect_lan_addresses() -> Vec<IpAddr> {
    #[cfg(target_os = "windows")]
    let output = command_output("ipconfig", &[]).await;
    #[cfg(not(target_os = "windows"))]
    let output = command_output("ip", &["-o", "addr", "show"]).await;

    output
        .map(|text| parse_interface_addresses(&text))
        .unwrap_or_default()
}

async fn detect_tailscale() -> TailscaleSnapshot {
    let Some(output) = command_output("tailscale", &["status", "--json"]).await else {
        return TailscaleSnapshot::default();
    };
    parse_tailscale_status(&output)
}

async fn resolve_magicdns(
    hostname: &str,
    port: u16,
    ipv4_running: bool,
    ipv6_running: bool,
) -> bool {
    // A bounded lookup keeps the diagnostics endpoint responsive even if a
    // stale resolver is configured. It also avoids advertising a hostname that
    // only exists in Tailscale status but cannot actually be reached here.
    timeout(
        Duration::from_secs(2),
        tokio::net::lookup_host((hostname, port)),
    )
    .await
    .ok()
    .and_then(|result| result.ok())
    .is_some_and(|mut addresses| {
        addresses.any(|address| family_is_listening(address.ip(), ipv4_running, ipv6_running))
    })
}

fn parse_interface_addresses(text: &str) -> Vec<IpAddr> {
    let mut addresses = Vec::new();
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        // `ipconfig` uses stable IPv4/IPv6 markers even when the rest of its
        // labels are localized.  The Unix `ip -o addr` form contains `inet`.
        let is_address_line = lower.contains("ipv4")
            || lower.contains("ipv6")
            || lower
                .split_whitespace()
                .any(|word| word == "inet" || word == "inet6");
        if !is_address_line {
            continue;
        }
        for token in line.split(|c: char| {
            !(c.is_ascii_hexdigit() || c == '.' || c == ':' || c == '%' || c == '/')
        }) {
            let token = token.split('%').next().unwrap_or_default();
            let token = token.split('/').next().unwrap_or_default();
            let Ok(address) = token.parse::<IpAddr>() else {
                continue;
            };
            if usable_lan_address(address) && !addresses.contains(&address) {
                addresses.push(address);
            }
        }
    }
    addresses
}

fn usable_lan_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            !address.is_loopback()
                && !address.is_unspecified()
                && !address.is_multicast()
                && !address.is_link_local()
                && !is_tailscale_v4(address)
        }
        IpAddr::V6(address) => {
            !address.is_loopback()
                && !address.is_unspecified()
                && !address.is_multicast()
                && !address.is_unicast_link_local()
        }
    }
}

fn is_tailscale_v4(address: Ipv4Addr) -> bool {
    // Tailscale allocates from the shared CGNAT range 100.64.0.0/10.
    let octets = address.octets();
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

fn parse_tailscale_status(text: &str) -> TailscaleSnapshot {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return TailscaleSnapshot::default();
    };
    let backend_running = value
        .get("BackendState")
        .and_then(Value::as_str)
        .map(|state| state.eq_ignore_ascii_case("running"))
        .unwrap_or(false);
    let Some(self_node) = value.get("Self") else {
        return TailscaleSnapshot::default();
    };
    let online = self_node
        .get("Online")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !backend_running || !online {
        return TailscaleSnapshot::default();
    }

    let mut addresses = Vec::new();
    if let Some(values) = self_node.get("TailscaleIPs").and_then(Value::as_array) {
        for value in values {
            let Some(text) = value.as_str() else {
                continue;
            };
            if let Ok(address) = text.parse::<IpAddr>() {
                addresses.push(address);
            }
        }
    }

    let magicdns_hostname = self_node
        .get("DNSName")
        .and_then(Value::as_str)
        .map(|name| name.trim_end_matches('.'))
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            let hostname = self_node.get("HostName").and_then(Value::as_str)?;
            let suffix = value.get("MagicDNSSuffix").and_then(Value::as_str)?;
            (!hostname.is_empty() && !suffix.is_empty()).then(|| format!("{hostname}.{suffix}"))
        });

    TailscaleSnapshot {
        addresses,
        magicdns_hostname,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn parses_only_usable_interface_addresses() {
        let fixture = r#"
Ethernet adapter Ethernet:
   IPv4 Address. . . . . . . . . . . : 192.168.10.42
   Link-local IPv6 Address . . . . . : fe80::1234%9
Wireless LAN adapter Wi-Fi:
   IPv4 Address. . . . . . . . . . . : 100.115.20.5
   IPv6 Address. . . . . . . . . . . : 2001:db8::42
"#;
        assert_eq!(
            parse_interface_addresses(fixture),
            vec![
                "192.168.10.42".parse::<IpAddr>().unwrap(),
                "2001:db8::42".parse::<IpAddr>().unwrap()
            ]
        );
    }

    #[test]
    fn parses_connected_tailscale_status_and_magicdns() {
        let fixture = r#"{
  "BackendState": "Running",
  "MagicDNSSuffix": "tailnet-name.ts.net",
  "Self": {
    "Online": true,
    "HostName": "curator-pc",
    "TailscaleIPs": ["100.83.44.1", "fd7a:115c:a1e0::123"]
  }
}"#;
        let parsed = parse_tailscale_status(fixture);
        assert_eq!(parsed.addresses.len(), 2);
        assert_eq!(
            parsed.magicdns_hostname.as_deref(),
            Some("curator-pc.tailnet-name.ts.net")
        );
    }

    #[test]
    fn ignores_disconnected_tailscale_status() {
        let fixture =
            r#"{"BackendState":"Stopped","Self":{"Online":false,"TailscaleIPs":["100.64.0.1"]}}"#;
        assert!(parse_tailscale_status(fixture).addresses.is_empty());
    }

    #[test]
    fn formats_ipv6_urls_correctly() {
        assert_eq!(
            url_for("fd7a:115c:a1e0::123".parse().unwrap(), 8642),
            "http://[fd7a:115c:a1e0::123]:8642"
        );
    }

    #[tokio::test]
    async fn one_shared_listener_serves_remote_status_until_shutdown() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(root.path());
        let port = start_http_server_on(&state, 0).await.unwrap();
        assert!(state.remote_server.ipv4_running());
        // A second caller in the same background process must reuse the
        // listener rather than spawning a competing HTTP server.
        assert_eq!(start_http_server_on(&state, 0).await.unwrap(), port);

        let mut stream = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        stream
            .write_all(
                b"GET /api/remote-access HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("\"running\":true"), "{response}");
        assert!(
            response.contains(&format!("127.0.0.1:{port}")),
            "{response}"
        );

        state.shutdown.cancel();
        state.server_tasks.close();
        state.server_tasks.wait().await;
        assert!(!state.remote_server.running());
    }
}
