use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockPurpose {
    Server,
    IntegrityCheck,
}

#[derive(Debug, Error)]
pub enum InstanceLockError {
    #[error("another Vault process holds the instance lock at {path}")]
    Busy { path: PathBuf },
    #[error("cannot acquire Vault instance lock at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug)]
pub struct InstanceLock {
    path: PathBuf,
    file: File,
}

impl InstanceLock {
    pub fn acquire(db_path: &Path, purpose: LockPurpose) -> Result<Self, InstanceLockError> {
        let path = lock_path(db_path);
        if purpose == LockPurpose::Server
            && let Some(parent) = path.parent()
        {
            std::fs::create_dir_all(parent).map_err(|source| InstanceLockError::Io {
                path: path.clone(),
                source,
            })?;
        }
        let file = open_lock_file(&path).map_err(|source| InstanceLockError::Io {
            path: path.clone(),
            source,
        })?;
        file.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => InstanceLockError::Busy { path: path.clone() },
            TryLockError::Error(source) => InstanceLockError::Io {
                path: path.clone(),
                source,
            },
        })?;
        Ok(Self { path, file })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    // Existing lock sidecars are opened without `create`, so replacing one
    // with a dangling symlink cannot create its target. Missing sidecars use
    // `create_new`, whose exclusive-create semantics reject a symlink raced
    // into place. A concurrent process that creates the legitimate sidecar is
    // handled by retrying the existing-file branch.
    for _ in 0..3 {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "instance lock path is not a regular file",
                    ));
                }
                let file = OpenOptions::new().read(true).write(true).open(path)?;
                let after = std::fs::symlink_metadata(path)?;
                if after.file_type().is_symlink() || !after.is_file() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "instance lock path changed while it was opened",
                    ));
                }
                return Ok(file);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(path)
                {
                    Ok(file) => return Ok(file),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "instance lock path changed repeatedly during acquisition",
    ))
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[must_use]
pub fn lock_path(db_path: &Path) -> PathBuf {
    let lock_target = canonical_lock_target(db_path);
    let mut name = lock_target
        .file_name()
        .map_or_else(|| OsString::from("vault.db"), ToOwned::to_owned);
    name.push(".lock");
    lock_target.with_file_name(name)
}

fn canonical_lock_target(db_path: &Path) -> PathBuf {
    if let Ok(path) = std::fs::canonicalize(db_path) {
        return path;
    }
    let absolute = std::path::absolute(db_path).unwrap_or_else(|_| db_path.to_path_buf());
    let Some(file_name) = absolute.file_name() else {
        return absolute;
    };
    let parent = absolute.parent().unwrap_or_else(|| Path::new("."));
    std::fs::canonicalize(parent)
        .map(|parent| parent.join(file_name))
        .unwrap_or(absolute)
}
