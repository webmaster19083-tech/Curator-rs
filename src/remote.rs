//! Lightweight diagnostics for the single local Curator HTTP listener.

use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};

use serde::Serialize;

pub const DEFAULT_SERVER_PORT: u16 = 42168;

pub struct ServerStatus {
    port: AtomicU16,
    running: AtomicBool,
}

impl ServerStatus {
    pub fn new(port: u16) -> Self {
        Self { port: AtomicU16::new(port), running: AtomicBool::new(true) }
    }

    pub fn port(&self) -> u16 { self.port.load(Ordering::Acquire) }
    pub fn running(&self) -> bool { self.running.load(Ordering::Acquire) }
    pub fn set_running(&self, running: bool) { self.running.store(running, Ordering::Release); }
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

pub async fn remote_access_info(state: &crate::AppState) -> RemoteAccessInfo {
    let port = state.remote_server.port();
    let running = state.remote_server.running();
    RemoteAccessInfo {
        running,
        server: if running { "Running" } else { "Stopped" },
        port,
        bound_addresses: running.then(|| vec![format!("127.0.0.1:{port}")]).unwrap_or_default(),
        local_urls: running.then(|| vec![format!("http://127.0.0.1:{port}")]).unwrap_or_default(),
        // LAN/Tailscale discovery is deliberately not inferred from untrusted
        // browser input; the desktop listener remains local unless a future
        // explicitly configured remote-access feature enables it.
        lan_urls: Vec::new(),
        tailscale_urls: Vec::new(),
        magicdns_hostname: None,
    }
}
