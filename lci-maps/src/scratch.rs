//! Scratch directories for the contract kit and the tests: `lci-maps` has
//! no `tempfile` dependency.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// A directory under the system's temporary directory, deleted on drop.
#[derive(Debug)]
pub struct Scratch {
    path: PathBuf,
}

static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

impl Scratch {
    pub fn new(label: &str) -> std::io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "lci-maps-{label}-{}-{}-{nanos}",
            std::process::id(),
            SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
