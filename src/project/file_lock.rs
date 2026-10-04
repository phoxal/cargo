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
