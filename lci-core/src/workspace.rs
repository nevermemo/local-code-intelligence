use anyhow::{Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

#[derive(Debug, Clone, Serialize)]
pub struct Workspace {
    pub id: String,
    pub path: PathBuf,
}

impl Workspace {
    pub fn resolve(path: &Path, data_dir: &Path) -> Result<Self> {
        let path = dunce::canonicalize(path)?;
        ensure!(path.is_dir(), "workspace must be a directory");
        let data = dunce::canonicalize(data_dir)?;
        ensure!(
            !data.starts_with(&path) && !path.starts_with(&data),
            "data directory and workspace must not contain one another"
        );
        let key = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("workspace path must be UTF-8"))?;
        Ok(Self {
            id: hash(key),
            path,
        })
    }
    pub fn table_name(&self) -> String {
        format!("ws_{}", self.id)
    }
}
