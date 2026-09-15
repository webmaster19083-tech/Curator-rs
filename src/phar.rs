//! Managed P-HAR environment metadata and safety gate.
//!
//! The upstream project is a multi-model MMAction2 application, not a single
//! TorchScript file. Curator therefore tracks one pinned upstream environment
//! beneath the selected data directory and never treats an arbitrary local
//! model path as P-HAR. Checkpoints remain blocked until their individual
//! redistribution rights and checksums are explicitly verified.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::edition::InstallScope;

pub const MANIFEST_FORMAT: &str = "curator-phar-manifest-v1";
pub const UPSTREAM_REPOSITORY: &str = "https://github.com/rlleshi/phar.git";
pub const UPSTREAM_REVISION: &str = "94adf9900cd36360795709d920b44404f29bad3e";
const STATE_FILE: &str = "state.json";
const MANIFEST_FILE: &str = "manifest.json";

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PharRuntime {
    Native,
    Wsl2,
}

impl PharRuntime {
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("wsl2") | Some("wsl") => Self::Wsl2,
            _ => Self::Native,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Wsl2 => "wsl2",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PharSupportTier {
    Supported,
    Experimental,
    Unavailable,
}

#[derive(Debug, Clone, Serialize)]
pub struct PharSupport {
    pub tier: PharSupportTier,
    pub runtime: &'static str,
    pub detail: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Linux,
    Windows,
    MacOs,
    Other,
}

pub const fn current_platform() -> Platform {
    #[cfg(target_os = "linux")]
    {
        Platform::Linux
    }
    #[cfg(target_os = "windows")]
    {
        Platform::Windows
    }
    #[cfg(target_os = "macos")]
    {
        Platform::MacOs
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        Platform::Other
    }
}

/// Pick the supported initial runtime for a platform. A Windows user who
/// simply opts in should be guided to WSL2, not silently placed on the
/// experimental native path. An explicit saved/runtime CLI selection still
/// overrides this default.
pub const fn default_runtime(platform: Platform) -> PharRuntime {
    match platform {
        Platform::Windows => PharRuntime::Wsl2,
        Platform::Linux | Platform::MacOs | Platform::Other => PharRuntime::Native,
    }
}

/// Linux native and Windows through WSL2 are the supported first tier.
/// Native Windows and macOS stay experimental until their automated install
/// and a real inference probe pass; we never silently label them ready.
pub const fn support_for(platform: Platform, runtime: PharRuntime) -> PharSupport {
    match (platform, runtime) {
        (Platform::Linux, PharRuntime::Native) => PharSupport {
            tier: PharSupportTier::Supported,
            runtime: "native",
            detail: "Native Linux is a supported P-HAR target.",
        },
        (Platform::Windows, PharRuntime::Wsl2) => PharSupport {
            tier: PharSupportTier::Supported,
            runtime: "wsl2",
            detail: "Windows uses the supported WSL2 P-HAR environment.",
        },
        (Platform::Windows, PharRuntime::Native) => PharSupport {
            tier: PharSupportTier::Experimental,
            runtime: "native",
            detail: "Native Windows is experimental and needs an installation and inference probe.",
        },
        (Platform::MacOs, PharRuntime::Native) => PharSupport {
            tier: PharSupportTier::Experimental,
            runtime: "native",
            detail: "Native macOS is experimental and needs an installation and inference probe.",
        },
        (Platform::Linux, PharRuntime::Wsl2) => PharSupport {
            tier: PharSupportTier::Unavailable,
            runtime: "wsl2",
            detail: "WSL2 is only a Windows runtime.",
        },
        (Platform::MacOs, PharRuntime::Wsl2) => PharSupport {
            tier: PharSupportTier::Unavailable,
            runtime: "wsl2",
            detail: "WSL2 is only a Windows runtime.",
        },
        (Platform::Other, PharRuntime::Native) => PharSupport {
            tier: PharSupportTier::Unavailable,
            runtime: "native",
            detail: "P-HAR is not supported on this platform/runtime combination.",
        },
        (Platform::Other, PharRuntime::Wsl2) => PharSupport {
            tier: PharSupportTier::Unavailable,
            runtime: "wsl2",
            detail: "P-HAR is not supported on this platform/runtime combination.",
        },
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PharManifest {
    pub format: &'static str,
    pub upstream: UpstreamPin,
    pub pinned_files: Vec<UpstreamFilePin>,
    pub dependencies: Vec<&'static str>,
    pub labels: Vec<&'static str>,
    pub checkpoints: Vec<CheckpointManifest>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpstreamPin {
    pub repository: &'static str,
    pub revision: &'static str,
    pub submodules: Vec<SubmodulePin>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubmodulePin {
    pub path: &'static str,
    pub revision: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpstreamFilePin {
    pub path: &'static str,
    pub sha256: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckpointManifest {
    pub id: &'static str,
    pub required_for: &'static str,
    pub source_url: Option<&'static str>,
    pub sha256: Option<&'static str>,
    pub redistribution_rights: &'static str,
}

/// This records the exact source and upstream dependency declaration used by
/// Curator. The P-HAR repository's model links are intentionally represented
/// as unverified rather than guessed or redistributed.
pub fn pinned_manifest() -> PharManifest {
    PharManifest {
        format: MANIFEST_FORMAT,
        upstream: UpstreamPin {
            repository: UPSTREAM_REPOSITORY,
            revision: UPSTREAM_REVISION,
            submodules: vec![
                SubmodulePin {
                    path: "mmaction2",
                    revision: "255bbc0",
                },
                SubmodulePin {
                    path: "mmdetection",
                    revision: "9894980",
                },
                SubmodulePin {
                    path: "mmpose",
                    revision: "5c8ba26",
                },
            ],
        },
        pinned_files: vec![
            UpstreamFilePin {
                path: "requirements/extra.txt",
                sha256: "dd3073bbc6c7c839ececbe1ec1883afa5171d8554c51ffc884f6368c973ddcac",
            },
            UpstreamFilePin {
                path: "resources/annotations/annotations.txt",
                sha256: "a629d4256ea7faf65e08d7d00ccfe54e53ce2957fe257a3227ea3b874dd75e8c",
            },
        ],
        dependencies: vec![
            "librosa==0.8.1",
            "lws==1.2.7",
            "moviepy==1.0.3",
            "numpy==1.22.4",
            "pyloudnorm==0.1.0",
            "SoundFile==0.10.3.post1",
            "mlflow (upstream declaration; lock required before installation)",
            "rich (upstream declaration; lock required before installation)",
            "schedule (upstream declaration; lock required before installation)",
        ],
        labels: vec![
            "kissing",
            "fondling",
            "handjob",
            "fingering",
            "titjob",
            "blowjob",
            "cunnilingus",
            "deepthroat",
            "doggy",
            "the-snake",
            "anal",
            "missionary",
            "cowgirl",
            "scoop-up",
            "cumshot",
            "facial-cumshot",
            "69",
        ],
        checkpoints: vec![CheckpointManifest {
            id: "p-har-model-bundle",
            required_for: "inference",
            source_url: None,
            sha256: None,
            redistribution_rights: "unverified",
        }],
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PharPhase {
    Disabled,
    Requested,
    Blocked,
    Installing,
    Ready,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct PharStatus {
    pub requested: bool,
    pub ready: bool,
    pub phase: PharPhase,
    pub progress_percent: u8,
    pub support: PharSupport,
    pub message: String,
    pub manifest_revision: &'static str,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct PersistedState {
    phase: PharPhase,
    progress_percent: u8,
    message: String,
}

pub fn environment_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("phar")
}

fn manifest_path(data_dir: &Path) -> PathBuf {
    environment_dir(data_dir).join(MANIFEST_FILE)
}

fn state_path(data_dir: &Path) -> PathBuf {
    environment_dir(data_dir).join(STATE_FILE)
}

/// Write the pin file into Curator's dedicated environment. It contains no
/// checkpoint URL or payload while rights verification is unresolved.
pub fn ensure_manifest(data_dir: &Path) -> Result<()> {
    let path = manifest_path(data_dir);
    if path.is_file() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&pinned_manifest())?)?;
    Ok(())
}

fn configured_runtime(scope: InstallScope) -> PharRuntime {
    let cfg = crate::config::load_config_for(scope);
    cfg.phar_runtime
        .as_deref()
        .map(|runtime| PharRuntime::parse(Some(runtime)))
        .unwrap_or_else(|| default_runtime(current_platform()))
}

pub fn status(data_dir: &Path, scope: InstallScope) -> PharStatus {
    let cfg = crate::config::load_config_for(scope);
    let requested = cfg.phar_setup_requested;
    let runtime = configured_runtime(scope);
    let support = support_for(current_platform(), runtime);
    let persisted = std::fs::read_to_string(state_path(data_dir))
        .ok()
        .and_then(|value| serde_json::from_str::<PersistedState>(&value).ok());
    let state = persisted.unwrap_or_else(|| default_state(requested, &support));
    let ready = state.phase == PharPhase::Ready && runtime_is_verified(data_dir);
    PharStatus {
        requested,
        ready,
        phase: if state.phase == PharPhase::Ready && !ready {
            PharPhase::Failed
        } else {
            state.phase
        },
        progress_percent: state.progress_percent,
        support,
        message: if state.phase == PharPhase::Ready && !ready {
            "P-HAR readiness marker did not pass manifest verification; NudeNet/manual review remain active.".into()
        } else {
            state.message
        },
        manifest_revision: UPSTREAM_REVISION,
    }
}

fn default_state(requested: bool, support: &PharSupport) -> PersistedState {
    if !requested {
        return PersistedState {
            phase: PharPhase::Disabled,
            progress_percent: 0,
            message: "P-HAR is opt-in and currently disabled.".into(),
        };
    }
    if support.tier == PharSupportTier::Unavailable {
        return PersistedState {
            phase: PharPhase::Failed,
            progress_percent: 0,
            message: support.detail.into(),
        };
    }
    PersistedState {
        phase: PharPhase::Requested,
        progress_percent: 0,
        message: "P-HAR setup was requested and will be evaluated after Curator starts.".into(),
    }
}

fn write_state(data_dir: &Path, state: PersistedState) -> Result<()> {
    ensure_manifest(data_dir)?;
    std::fs::write(state_path(data_dir), serde_json::to_string_pretty(&state)?)?;
    Ok(())
}

/// OS installers call only this intent-recording path. No download, clone,
/// model, or package installation can occur inside an installer transaction.
pub fn record_install_intent(
    data_dir: &Path,
    scope: InstallScope,
    enabled: bool,
    runtime: Option<PharRuntime>,
) -> Result<PharStatus> {
    let mut cfg = crate::config::load_config_for(scope);
    cfg.phar_setup_requested = enabled;
    if let Some(runtime) = runtime {
        cfg.phar_runtime = Some(runtime.as_str().into());
    }
    crate::config::save_config_for(scope, &cfg)?;
    let support = support_for(current_platform(), configured_runtime(scope));
    write_state(
        data_dir,
        if enabled {
            default_state(true, &support)
        } else {
            default_state(false, &support)
        },
    )?;
    Ok(status(data_dir, scope))
}

/// Run after a Host/Server has started. Before a checkpoint has a verified
/// upstream location and SHA-256, this deliberately stops at a transparent
/// blocked state instead of pretending a generic TorchScript file is ready.
pub fn resume_requested_setup(data_dir: &Path, scope: InstallScope) -> Result<PharStatus> {
    let current = status(data_dir, scope);
    if !current.requested || current.phase == PharPhase::Cancelled {
        return Ok(current);
    }
    ensure_manifest(data_dir)?;
    if current.support.tier == PharSupportTier::Unavailable {
        return Ok(current);
    }
    if pinned_manifest()
        .checkpoints
        .iter()
        .any(|checkpoint| checkpoint.redistribution_rights != "verified")
    {
        write_state(
            data_dir,
            PersistedState {
                phase: PharPhase::Blocked,
                progress_percent: 0,
                message: "P-HAR source pin is recorded, but required checkpoint rights/checksums are not verified. No model was downloaded; NudeNet and manual review remain available.".into(),
            },
        )?;
        return Ok(status(data_dir, scope));
    }
    bail!("No verified P-HAR checkpoint installer is bundled in this build.")
}

pub fn cancel(data_dir: &Path, scope: InstallScope) -> Result<PharStatus> {
    let current = status(data_dir, scope);
    if !current.requested {
        return Ok(current);
    }
    write_state(
        data_dir,
        PersistedState {
            phase: PharPhase::Cancelled,
            progress_percent: current.progress_percent,
            message: "P-HAR setup was cancelled. Resume or repair from Local Admin when ready."
                .into(),
        },
    )?;
    Ok(status(data_dir, scope))
}

/// Disable managed setup before its on-disk environment is removed. This
/// prevents an explicit "Delete P-HAR environment" Admin action from being
/// undone at the next Server/Host startup by a still-recorded opt-in request.
pub fn disable(data_dir: &Path, scope: InstallScope) -> Result<PharStatus> {
    record_install_intent(data_dir, scope, false, None)
}

/// Re-evaluate a previously cancelled, blocked, or failed setup while keeping
/// the recorded runtime choice. This is safe to call repeatedly: no model is
/// downloaded until a future manifest supplies verified rights and hashes.
pub fn repair(data_dir: &Path, scope: InstallScope) -> Result<PharStatus> {
    let runtime = configured_runtime(scope);
    record_install_intent(data_dir, scope, true, Some(runtime))?;
    resume_requested_setup(data_dir, scope)
}

/// Verify that a ready environment still has a valid marker. A verified
/// installer is responsible for the expensive real inference probe before it
/// writes that marker; this check makes stale/partial environments fail closed.
pub fn self_test(data_dir: &Path, scope: InstallScope) -> Result<PharStatus> {
    let current = status(data_dir, scope);
    if !current.requested {
        return Ok(current);
    }
    if !runtime_is_verified(data_dir) {
        write_state(
            data_dir,
            PersistedState {
                phase: PharPhase::Failed,
                progress_percent: 0,
                message: "P-HAR self-test cannot run because the environment has no verified checkpoint/inference marker.".into(),
            },
        )?;
    }
    Ok(status(data_dir, scope))
}

/// A ready marker is accepted only after a future verified installer writes
/// the exact pinned revision and confirms checkpoint validation. This keeps a
/// partial or stale environment from ever enabling automatic Fast evidence.
pub fn runtime_is_verified(data_dir: &Path) -> bool {
    #[derive(Deserialize)]
    struct RuntimeMarker {
        format: String,
        upstream_revision: String,
        checkpoints_verified: bool,
        inference_probe_passed: bool,
    }
    let marker = environment_dir(data_dir).join("runtime.json");
    let Ok(text) = std::fs::read_to_string(marker) else {
        return false;
    };
    let Ok(marker) = serde_json::from_str::<RuntimeMarker>(&text) else {
        return false;
    };
    marker.format == "curator-phar-runtime-v1"
        && marker.upstream_revision == UPSTREAM_REVISION
        && marker.checkpoints_verified
        && marker.inference_probe_passed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn support_tiers_are_explicit() {
        assert_eq!(
            support_for(Platform::Linux, PharRuntime::Native).tier,
            PharSupportTier::Supported
        );
        assert_eq!(
            support_for(Platform::Windows, PharRuntime::Wsl2).tier,
            PharSupportTier::Supported
        );
        assert_eq!(
            support_for(Platform::Windows, PharRuntime::Native).tier,
            PharSupportTier::Experimental
        );
        assert_eq!(
            support_for(Platform::MacOs, PharRuntime::Native).tier,
            PharSupportTier::Experimental
        );
        assert_eq!(default_runtime(Platform::Windows), PharRuntime::Wsl2);
        assert_eq!(default_runtime(Platform::Linux), PharRuntime::Native);
        assert_eq!(
            support_for(Platform::MacOs, PharRuntime::Wsl2).runtime,
            "wsl2"
        );
    }

    #[test]
    fn manifest_has_no_unverified_checkpoint_download() {
        let manifest = pinned_manifest();
        assert!(manifest
            .checkpoints
            .iter()
            .all(|checkpoint| checkpoint.source_url.is_none() && checkpoint.sha256.is_none()));
        assert_eq!(manifest.labels.len(), 17);
    }

    #[test]
    fn requested_setup_stays_blocked_without_checkpoint_rights() {
        let _guard = crate::PROCESS_ENV_LOCK.lock().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var("CURATOR_CONFIG_DIR", data.path());
        let _ = record_install_intent(data.path(), InstallScope::CurrentUser, true, None);
        let status = resume_requested_setup(data.path(), InstallScope::CurrentUser).unwrap();
        std::env::remove_var("CURATOR_CONFIG_DIR");
        assert_eq!(status.phase, PharPhase::Blocked);
        assert!(!status.ready);
    }

    #[test]
    fn self_test_fails_closed_without_a_verified_runtime() {
        let _guard = crate::PROCESS_ENV_LOCK.lock().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var("CURATOR_CONFIG_DIR", data.path());
        let _ = record_install_intent(data.path(), InstallScope::CurrentUser, true, None);
        let status = self_test(data.path(), InstallScope::CurrentUser).unwrap();
        std::env::remove_var("CURATOR_CONFIG_DIR");
        assert_eq!(status.phase, PharPhase::Failed);
        assert!(!status.ready);
    }

    #[test]
    fn disabling_managed_setup_clears_the_request() {
        let _guard = crate::PROCESS_ENV_LOCK.lock().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var("CURATOR_CONFIG_DIR", data.path());
        record_install_intent(data.path(), InstallScope::CurrentUser, true, None).unwrap();
        let status = disable(data.path(), InstallScope::CurrentUser).unwrap();
        std::env::remove_var("CURATOR_CONFIG_DIR");
        assert!(!status.requested);
        assert_eq!(status.phase, PharPhase::Disabled);
    }
}
