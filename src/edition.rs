//! Product-edition and installation-scope contracts shared by every Curator
//! executable.  These values deliberately live in the core crate so clients
//! cannot drift from the server's API negotiation rules.

use serde::{Deserialize, Serialize};

/// Increment only when a client and server can no longer safely communicate.
/// The product version can change independently of this wire contract.
pub const API_PROTOCOL: &str = "curator-api/1";
pub const PRODUCT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edition {
    Server,
    Host,
    Viewer,
}

impl Edition {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Host => "host",
            Self::Viewer => "viewer",
        }
    }

    pub const fn owns_library(self) -> bool {
        !matches!(self, Self::Viewer)
    }

    pub const fn has_local_admin(self) -> bool {
        matches!(self, Self::Server | Self::Host)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallScope {
    #[default]
    CurrentUser,
    AllUsers,
}

impl InstallScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CurrentUser => "current-user",
            Self::AllUsers => "all-users",
        }
    }

    /// Installers and service units set this environment variable. Keeping
    /// scope selection outside the database means the location is known
    /// before SQLite is opened.
    pub fn from_environment() -> Self {
        match std::env::var("CURATOR_INSTALL_SCOPE") {
            Ok(value)
                if matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "all-users" | "all_users" | "machine" | "system"
                ) =>
            {
                Self::AllUsers
            }
            _ => Self::CurrentUser,
        }
    }
}

#[derive(Debug, Clone)]
pub struct InitializeOptions {
    pub edition: Edition,
    pub install_scope: InstallScope,
    pub data_dir_override: Option<std::path::PathBuf>,
}

impl InitializeOptions {
    pub fn for_edition(edition: Edition) -> Self {
        Self {
            edition,
            install_scope: InstallScope::from_environment(),
            data_dir_override: None,
        }
    }

    pub fn server() -> Self {
        Self::for_edition(Edition::Server)
    }

    pub fn host() -> Self {
        Self::for_edition(Edition::Host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editions_keep_viewer_read_only_of_local_library() {
        assert!(Edition::Server.owns_library());
        assert!(Edition::Host.owns_library());
        assert!(!Edition::Viewer.owns_library());
        assert_eq!(API_PROTOCOL, "curator-api/1");
    }
}
