//! One process may own a resolved Curator data directory at a time.
//!
//! SQLite WAL protects individual transactions, but it cannot make two
//! independent Curator schedulers, migrators, and cache workers safe.  This
//! advisory OS lock is therefore acquired before a pool is created and kept
//! for the entire backend lifetime.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs2::FileExt;

pub struct DataDirectoryLock {
    file: File,
    path: PathBuf,
}

impl DataDirectoryLock {
    pub fn acquire(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("creating Curator data directory {}", data_dir.display()))?;
        let resolved = dunce::canonicalize(data_dir)
            .with_context(|| format!("resolving Curator data directory {}", data_dir.display()))?;
        let path = resolved.join(".curator-data.lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            // The lock file contains no state. Do not truncate an existing
            // inode while another process holds the advisory lock on it.
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening Curator data lock {}", path.display()))?;
        file.try_lock_exclusive().map_err(|error| {
            anyhow::anyhow!(
                "Curator data directory {} is already owned by another Host or Server process ({error})",
                resolved.display()
            )
        })?;
        Ok(Self { file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DataDirectoryLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_resolved_directory_cannot_be_owned_twice() {
        let dir = tempfile::tempdir().unwrap();
        let first = DataDirectoryLock::acquire(dir.path()).unwrap();
        assert!(DataDirectoryLock::acquire(dir.path()).is_err());
        assert!(first.path().ends_with(".curator-data.lock"));
    }
}
