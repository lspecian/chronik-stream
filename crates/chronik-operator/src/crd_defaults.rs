//! Default implementations for CRD types

use crate::crd::{StorageBackend, DatabaseType};

impl Default for StorageBackend {
    fn default() -> Self {
        StorageBackend::Local
    }
}

impl Default for DatabaseType {
    fn default() -> Self {
        DatabaseType::Postgres
    }
}