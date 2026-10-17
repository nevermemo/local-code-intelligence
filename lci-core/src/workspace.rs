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
    /// The index table's name: `ws_` plus the first 16 hex digits of the
    /// workspace id. The full 64-digit id made `<data_dir>/lancedb/ws_<id>.lance`
    /// plus Lance's internal paths exceed Windows' 260-character limit for a
    /// long `data_dir` (os error 3). 64 bits keep a collision among a
    /// thousand workspaces in one `data_dir` at about 3e-14, and `Store`
    /// refuses a table written for another workspace path rather than
    /// overwrite it.
    pub fn table_name(&self) -> String {
        // `resolve` always yields a 64-digit id; `get` keeps a hand-built
        // `Workspace` with a short id from panicking.
        format!("ws_{}", self.id.get(..16).unwrap_or(&self.id))
    }

    /// The name earlier versions gave the table: `ws_` plus the whole id.
    /// `Store` still reads it and drops it at the next commit.
    pub fn legacy_table_name(&self) -> String {
        format!("ws_{}", self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(key: &str) -> Workspace {
        Workspace {
            id: hash(key),
            path: PathBuf::from(key),
        }
    }

    #[test]
    fn table_name_is_ws_plus_16_hex_of_the_id() {
        let workspace = workspace("/projects/a");
        let name = workspace.table_name();
        assert_eq!(name.len(), "ws_".len() + 16);
        assert_eq!(name, format!("ws_{}", &workspace.id[..16]));
        assert!(name["ws_".len()..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn legacy_table_name_is_the_full_id() {
        let workspace = workspace("/projects/a");
        assert_eq!(workspace.id.len(), 64);
        assert_eq!(
            workspace.legacy_table_name(),
            format!("ws_{}", workspace.id)
        );
        assert!(
            workspace
                .legacy_table_name()
                .starts_with(&workspace.table_name())
        );
    }

    #[test]
    fn a_hand_built_workspace_with_a_short_id_does_not_panic() {
        let workspace = Workspace {
            id: "test".into(),
            path: PathBuf::from("/projects/a"),
        };
        assert_eq!(workspace.table_name(), "ws_test");
        assert_eq!(workspace.legacy_table_name(), "ws_test");
    }

    #[test]
    fn names_differ_between_workspaces() {
        let a = workspace("/projects/a");
        let b = workspace("/projects/b");
        assert_ne!(a.table_name(), b.table_name());
        assert_ne!(a.legacy_table_name(), b.legacy_table_name());
    }
}
