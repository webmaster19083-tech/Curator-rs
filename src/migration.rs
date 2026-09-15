//! Explicit Host-to-Server library import.
//!
//! The importer copies a stopped Host data directory instead of trying to
//! share it. The source and destination locks make a live Host/Server handoff
//! fail safely rather than racing SQLite WAL or downloader state.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rusqlite::{Connection, DatabaseName};
use serde::Serialize;
use walkdir::WalkDir;

use crate::data_lock::DataDirectoryLock;
use crate::edition::InstallScope;

#[derive(Debug, Clone, Serialize)]
pub struct HostImportReport {
    pub source_data_dir: PathBuf,
    pub destination_data_dir: PathBuf,
    pub copied_files: u64,
    pub copied_bytes: u64,
}

/// Import a complete Host library into an empty all-users Server directory.
///
/// The target is intentionally required to be empty. This makes the command
/// restartable and prevents a migration from overwriting a Server library
/// that has already accepted downloads.
pub fn import_host_library(source: &Path, destination: &Path) -> Result<HostImportReport> {
    let source = dunce::canonicalize(source)
        .with_context(|| format!("resolving Host data directory {}", source.display()))?;
    if !source.join("data.db").is_file() {
        bail!(
            "{} is not a Curator Host data directory (data.db is missing)",
            source.display()
        );
    }

    fs::create_dir_all(destination).with_context(|| {
        format!(
            "creating destination Server data directory {}",
            destination.display()
        )
    })?;
    let destination = dunce::canonicalize(destination).with_context(|| {
        format!(
            "resolving destination Server data directory {}",
            destination.display()
        )
    })?;
    if source == destination {
        bail!("Host and Server must use different data directories.");
    }

    // Acquire both locks before copying anything. A running Host or Server
    // causes a precise error and leaves the target untouched.
    let _source_lock = DataDirectoryLock::acquire(&source)?;
    let _destination_lock = DataDirectoryLock::acquire(&destination)?;
    ensure_empty_destination(&destination)?;

    let mut report = HostImportReport {
        source_data_dir: source.clone(),
        destination_data_dir: destination.clone(),
        copied_files: 0,
        copied_bytes: 0,
    };

    // An online backup produces a transactionally consistent main database,
    // including any clean-shutdown data that may still live in its WAL file.
    let source_db = Connection::open(source.join("data.db"))?;
    source_db.backup(DatabaseName::Main, destination.join("data.db"), None)?;
    record_file(&destination.join("data.db"), &mut report)?;

    for file in ["settings.json", "instance.json"] {
        copy_file_if_present(&source.join(file), &destination.join(file), &mut report)?;
    }
    for directory in ["library", "archives", "thumbnails", "backups", "phar"] {
        copy_tree_if_present(
            &source.join(directory),
            &destination.join(directory),
            &mut report,
        )?;
    }
    Ok(report)
}

/// Point the explicit all-users Server configuration at a successfully
/// imported destination. This is intentionally separate from copying so unit
/// tests and callers can verify the data transfer without mutating a system
/// configuration location.
pub fn configure_all_users_server(destination: &Path) -> Result<()> {
    let cfg = crate::config::Config {
        data_dir: Some(destination.to_string_lossy().into_owned()),
        ..Default::default()
    };
    crate::config::save_config_for(InstallScope::AllUsers, &cfg)
        .context("writing the all-users Server configuration")
}

fn ensure_empty_destination(destination: &Path) -> Result<()> {
    let occupied = fs::read_dir(destination)?
        .filter_map(|entry| entry.ok())
        .any(|entry| entry.file_name() != ".curator-data.lock");
    if occupied {
        bail!(
            "Destination {} is not empty; refusing to overwrite an existing Server library.",
            destination.display()
        );
    }
    Ok(())
}

fn copy_file_if_present(
    source: &Path,
    destination: &Path,
    report: &mut HostImportReport,
) -> Result<()> {
    if !source.is_file() {
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, destination).with_context(|| format!("copying {}", source.display()))?;
    record_file(destination, report)
}

fn copy_tree_if_present(
    source: &Path,
    destination: &Path,
    report: &mut HostImportReport,
) -> Result<()> {
    if !source.is_dir() {
        return Ok(());
    }
    for entry in WalkDir::new(source).follow_links(false) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(source)?;
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            copy_file_if_present(entry.path(), &target, report)?;
        } else if entry.file_type().is_symlink() {
            bail!(
                "Refusing to import symbolic link {} from Host data directory.",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn record_file(path: &Path, report: &mut HostImportReport) -> Result<()> {
    report.copied_files += 1;
    report.copied_bytes += fs::metadata(path)?.len();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_copies_a_consistent_library_without_the_source_lock() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("host");
        let destination = root.path().join("server");
        fs::create_dir_all(source.join("library/example")).unwrap();
        fs::write(source.join("library/example/media.bin"), b"media").unwrap();
        fs::write(source.join("settings.json"), "{}").unwrap();
        crate::db::init_pool(&source).unwrap();

        let report = import_host_library(&source, &destination).unwrap();
        assert!(destination.join("data.db").is_file());
        assert_eq!(
            fs::read(destination.join("library/example/media.bin")).unwrap(),
            b"media"
        );
        assert!(report.copied_files >= 3);
    }

    #[test]
    fn import_rejects_an_occupied_destination() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("host");
        let destination = root.path().join("server");
        fs::create_dir_all(&source).unwrap();
        crate::db::init_pool(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("already-here"), b"nope").unwrap();
        assert!(import_host_library(&source, &destination).is_err());
    }
}
