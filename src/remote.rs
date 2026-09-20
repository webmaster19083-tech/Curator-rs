//! The one Curator HTTP listener set and its remote-access diagnostics.
//!
//! Curator intentionally has no ordinary-LAN listener. The desktop shell,
//! headless fallback, and any remote browser all use the same Axum router, but
//! it is bound only to loopback and to addresses reported by the local
//! Tailscale daemon. This keeps an accidentally shared Wi-Fi or Ethernet
//! network from becoming an unauthenticated Curator admin surface.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::AppState;

/// Kept stable for local bookmarks. A listener is never opened on an ordinary
/// LAN interface at this port.
pub const DEFAULT_SERVER_PORT: u16 = 42168;
const TAILSCALE_REFRESH_INTERVAL: Duration = Duration::from_secs(15);

/// Process-lifetime state for every socket owned by Curator. The desktop
/// window deliberately has no ownership relationship with this state: hiding
/// it to the tray must not disconnect a browser fallback client.
pub struct ServerStatus {
    port: AtomicU16,
    running: AtomicBool,
    bound_addresses: RwLock<BTreeSet<SocketAddr>>,
    tailscale_listeners: Mutex<HashMap<IpAddr, CancellationToken>>,
    start_lock: tokio::sync::Mutex<()>,
}

impl ServerStatus {
    pub fn new(port: u16) -> Self {
        Self {
            port: AtomicU16::new(port),
            running: AtomicBool::new(false),
            bound_addresses: RwLock::new(BTreeSet::new()),
            tailscale_listeners: Mutex::new(HashMap::new()),
            start_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn port(&self) -> u16 {
        self.port.load(Ordering::Acquire)
    }

    pub fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn bound_addresses(&self) -> Vec<SocketAddr> {
        self.bound_addresses
            .read()
            .map(|addresses| addresses.iter().copied().collect())
            .unwrap_or_default()
    }

    fn set_port(&self, port: u16) {
        self.port.store(port, Ordering::Release);
    }

    fn register(&self, address: SocketAddr) {
        if let Ok(mut addresses) = self.bound_addresses.write() {
            addresses.insert(address);
            self.running.store(!addresses.is_empty(), Ordering::Release);
        }
    }

    fn unregister(&self, address: SocketAddr) {
        if let Ok(mut addresses) = self.bound_addresses.write() {
            addresses.remove(&address);
            self.running.store(!addresses.is_empty(), Ordering::Release);
        }
    }

    fn reserve_tailscale(&self, address: IpAddr) -> Option<CancellationToken> {
        let mut listeners = self.tailscale_listeners.lock().ok()?;
        if listeners.contains_key(&address) {
            return None;
        }
        let cancellation = CancellationToken::new();
        listeners.insert(address, cancellation.clone());
        Some(cancellation)
    }

    fn release_tailscale(&self, address: IpAddr) {
        if let Ok(mut listeners) = self.tailscale_listeners.lock() {
            listeners.remove(&address);
        }
    }

    fn active_tailscale(&self) -> Vec<(IpAddr, CancellationToken)> {
        self.tailscale_listeners
            .lock()
            .map(|listeners| {
                listeners
                    .iter()
                    .map(|(address, cancellation)| (*address, cancellation.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Bind the shared server at Curator's normal port.
pub async fn start_http_server(state: &AppState) -> Result<u16> {
    start_http_server_on(state, DEFAULT_SERVER_PORT).await
}

/// A port of zero is useful for tests. Production calls start_http_server.
pub async fn start_http_server_on(state: &AppState, requested_port: u16) -> Result<u16> {
    let _start_guard = state.remote_server.start_lock.lock().await;
    if state.remote_server.running() {
        return Ok(state.remote_server.port());
    }

    // Loopback is mandatory. It is the common desktop/browser fallback and
    // remains available if Tailscale is absent or disconnected.
    let ipv4_listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, requested_port)))
        .await
        .with_context(|| format!("binding Curator on 127.0.0.1:{requested_port}"))?;
    let port = ipv4_listener
        .local_addr()
        .context("reading Curator loopback listener address")?
        .port();
    state.remote_server.set_port(port);
    spawn_listener(state, ipv4_listener, None);

    // IPv6 loopback is an additional local endpoint, never a wildcard bind.
    match TcpListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, port))).await {
        Ok(listener) => spawn_listener(state, listener, None),
        Err(error) => info!("Curator IPv6 loopback unavailable on [::1]:{port}: {error}"),
    }

    refresh_tailscale_listeners(state).await;
    spawn_tailscale_refresher(state);
    info!("Curator HTTP server listening on loopback port {port}; Tailnet listeners are added only when Tailscale reports local addresses");
    Ok(port)
}

fn spawn_listener(
    state: &AppState,
    listener: TcpListener,
    tailscale: Option<(IpAddr, CancellationToken)>,
) {
    let address = match listener.local_addr() {
        Ok(address) => address,
        Err(error) => {
            warn!("Could not read Curator listener address: {error}");
            return;
        }
    };
    state.remote_server.register(address);
    let app = crate::router(state.clone());
    let global_shutdown = state.shutdown.clone();
    let status = Arc::clone(&state.remote_server);
    state.server_tasks.spawn(async move {
        let local_shutdown = tailscale.as_ref().map(|(_, token)| token.clone());
        let result = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            if let Some(local_shutdown) = local_shutdown {
                tokio::select! {
                    _ = global_shutdown.cancelled() => {},
                    _ = local_shutdown.cancelled() => {},
                }
            } else {
                global_shutdown.cancelled().await;
            }
        })
        .await;
        if let Err(error) = result {
            warn!("Curator HTTP listener {address} stopped unexpectedly: {error}");
        }
        status.unregister(address);
        if let Some((tailnet_address, _)) = tailscale {
            status.release_tailscale(tailnet_address);
        }
    });
}

fn spawn_tailscale_refresher(state: &AppState) {
    let state = state.clone();
    let server_tasks = state.server_tasks.clone();
    server_tasks.spawn(async move {
        loop {
            tokio::select! {
                _ = state.shutdown.cancelled() => break,
                _ = tokio::time::sleep(TAILSCALE_REFRESH_INTERVAL) => {
                    refresh_tailscale_listeners(&state).await;
                }
            }
        }
    });
}

/// Reconcile the Tailscale-only sockets with the daemon's live status. A
/// removed Tailscale address is cancelled immediately; a new one is bound
/// without restarting the desktop app or moving to a wildcard socket.
async fn refresh_tailscale_listeners(state: &AppState) {
    let snapshot = detect_tailscale().await;
    let desired: HashSet<IpAddr> = snapshot.addresses.into_iter().collect();
    for (address, cancellation) in state.remote_server.active_tailscale() {
        if !desired.contains(&address) {
            cancellation.cancel();
        }
    }
    for address in desired {
        let Some(cancellation) = state.remote_server.reserve_tailscale(address) else {
            continue;
        };
        let socket = SocketAddr::new(address, state.remote_server.port());
        match TcpListener::bind(socket).await {
            Ok(listener) => {
                info!("Curator Tailnet listener enabled on {socket}");
                spawn_listener(state, listener, Some((address, cancellation)));
            }
            Err(error) => {
                // A stale status entry, a race while reconnecting, or an IPv6
                // capability gap must leave Curator local-only rather than
                // falling back to a LAN wildcard.
                state.remote_server.release_tailscale(address);
                info!("Curator Tailnet listener unavailable on {socket}: {error}");
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteAccessInfo {
    pub running: bool,
    pub server: &'static str,
    pub port: u16,
    pub bound_addresses: Vec<String>,
    pub local_urls: Vec<String>,
    /// Retained for additive API compatibility. Curator intentionally never
    /// advertises or binds ordinary LAN addresses.
    pub lan_urls: Vec<String>,
    pub tailscale_urls: Vec<String>,
    pub magicdns_hostname: Option<String>,
    pub access_scope: &'static str,
}

#[derive(Default)]
struct TailscaleSnapshot {
    addresses: Vec<IpAddr>,
    magicdns_hostname: Option<String>,
}

/// Read a fresh daemon snapshot so status updates as Wi-Fi/Tailscale changes.
pub async fn remote_access_info(state: &AppState) -> RemoteAccessInfo {
    let port = state.remote_server.port();
    let bound = state.remote_server.bound_addresses();
    let running = !bound.is_empty();
    let snapshot = detect_tailscale().await;
    let reported_tailscale: HashSet<IpAddr> = snapshot.addresses.into_iter().collect();

    let local_urls = bound
        .iter()
        .filter(|address| address.ip().is_loopback())
        .map(|address| url_for(address.ip(), port))
        .collect::<Vec<_>>();
    let tailscale_urls = bound
        .iter()
        .map(SocketAddr::ip)
        .filter(|address| reported_tailscale.contains(address))
        .map(|address| url_for(address, port))
        .collect::<Vec<_>>();
    let magicdns_hostname = snapshot
        .magicdns_hostname
        .filter(|_| !tailscale_urls.is_empty());

    RemoteAccessInfo {
        running,
        server: if running { "Running" } else { "Stopped" },
        port,
        bound_addresses: bound
            .into_iter()
            .map(|address| address.to_string())
            .collect(),
        local_urls,
        lan_urls: Vec::new(),
        tailscale_urls,
        magicdns_hostname,
        access_scope: "loopback_and_tailscale_only",
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
        crate::process::command(command).args(args).output(),
    )
    .await
    .ok()?
    .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn detect_tailscale() -> TailscaleSnapshot {
    let Some(output) = command_output("tailscale", &["status", "--json"]).await else {
        return TailscaleSnapshot::default();
    };
    parse_tailscale_status(&output)
}

fn parse_tailscale_status(text: &str) -> TailscaleSnapshot {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return TailscaleSnapshot::default();
    };
    let running = value
        .get("BackendState")
        .and_then(Value::as_str)
        .is_some_and(|state| state.eq_ignore_ascii_case("running"));
    let Some(self_node) = value.get("Self") else {
        return TailscaleSnapshot::default();
    };
    let online = self_node
        .get("Online")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !running || !online {
        return TailscaleSnapshot::default();
    }

    let addresses = self_node
        .get("TailscaleIPs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|value| value.parse::<IpAddr>().ok())
        .collect();
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
            url_for("fd7a:115c:a1e0::123".parse().unwrap(), 42168),
            "http://[fd7a:115c:a1e0::123]:42168"
        );
    }

    #[tokio::test]
    async fn server_never_binds_an_ordinary_lan_address() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(root.path());
        let port = start_http_server_on(&state, 0).await.unwrap();
        let bound = state.remote_server.bound_addresses();
        let tailscale = detect_tailscale()
            .await
            .addresses
            .into_iter()
            .collect::<HashSet<_>>();
        assert!(bound.iter().any(|address| address.ip().is_loopback()));
        assert!(bound
            .iter()
            .all(|address| address.ip().is_loopback() || tailscale.contains(&address.ip())));

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
        assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200"));

        state.shutdown.cancel();
        state.server_tasks.close();
        state.server_tasks.wait().await;
        assert!(!state.remote_server.running());
    }
}
