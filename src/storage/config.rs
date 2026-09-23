//! [`DbConfig`] — opening parameters for [`crate::storage::Db`] and
//! [`crate::storage::DbPool`].
//!
//! Modeled on `servertoolkit-db/src/config.rs`, adapted the way
//! `mail4agent-store-sqlite/src/db.rs` adapted the same source into its own
//! crate: own types, no `servertoolkit` dependency. Only the two shapes
//! memory4agent needs — a file-backed path (WAL, production) or an
//! in-memory connection (tests) — unlike stk's own `DbConfig`, which also
//! carries SQLCipher-encrypted variants this crate has no use for.

use std::path::{Path, PathBuf};

#[derive(Clone)]
pub enum DbConfig {
    Path(PathBuf),
    InMemory,
}

impl DbConfig {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::Path(path.into())
    }

    pub fn in_memory() -> Self {
        Self::InMemory
    }

    pub fn is_in_memory(&self) -> bool {
        matches!(self, Self::InMemory)
    }

    /// File path if file-backed, `None` if in-memory.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Path(p) => Some(p.as_path()),
            Self::InMemory => None,
        }
    }
}

impl std::fmt::Debug for DbConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(p) => write!(f, "DbConfig::Path({p:?})"),
            Self::InMemory => write!(f, "DbConfig::InMemory"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_and_in_memory_report_correctly() {
        let path_cfg = DbConfig::new("some/dir/index.db");
        assert!(!path_cfg.is_in_memory());
        assert_eq!(path_cfg.path(), Some(Path::new("some/dir/index.db")));

        let mem_cfg = DbConfig::in_memory();
        assert!(mem_cfg.is_in_memory());
        assert_eq!(mem_cfg.path(), None);
    }
}
