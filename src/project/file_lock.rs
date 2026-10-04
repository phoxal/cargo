//! Scoped advisory locks with explicit release before closing the descriptor.

use std::fs::File;

use fs4::{FileExt, TryLockError};

#[derive(Debug)]
pub(crate) struct ExclusiveFileLock(File);

impl ExclusiveFileLock {
    pub(crate) fn acquire(file: File) -> std::io::Result<Self> {
        FileExt::lock(&file)?;
        Ok(Self(file))
    }

    pub(crate) fn try_acquire(file: File) -> Result<Self, TryLockError> {
        FileExt::try_lock(&file)?;
        Ok(Self(file))
    }
}

impl Drop for ExclusiveFileLock {
    fn drop(&mut self) {
        // A concurrently spawned process can inherit this open file description
        // until exec. Closing our descriptor alone would leave its lock held.
        let _ = FileExt::unlock(&self.0);
    }
}

/// A shared (reader) advisory lock on one file.
///
/// Paired with [`ExclusiveFileLock`] on the same lock file, this gives the
/// reader/writer boundary between long-lived selections and mutating
/// installation commands: any number of selections may hold the shared
/// lock, while replacement and removal require the exclusive lock.
pub(crate) struct SharedFileLock(File);

impl SharedFileLock {
    pub(crate) fn try_acquire(file: File) -> Result<Self, TryLockError> {
        FileExt::try_lock_shared(&file)?;
        Ok(Self(file))
    }
}

impl Drop for SharedFileLock {
    fn drop(&mut self) {
        // A concurrently spawned process can inherit this open file
        // description until exec. Closing our descriptor alone would leave
        // its lock held.
        let _ = FileExt::unlock(&self.0);
    }
}
