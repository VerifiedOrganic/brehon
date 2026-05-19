//! Schema versioning for future database upgrades.
//!
//! Provides a migration framework for evolving the database schema over time.

use fjall::PartitionHandle;

use crate::keys::KEY_META_SCHEMA_VERSION;
use crate::store::StoreError;

pub const CURRENT_SCHEMA_VERSION: u64 = 1;

pub struct MigrationRunner {
    meta: PartitionHandle,
}

impl MigrationRunner {
    pub fn new(meta: PartitionHandle) -> Self {
        Self { meta }
    }

    pub fn get_version(&self) -> Result<u64, StoreError> {
        match self.meta.get(KEY_META_SCHEMA_VERSION)? {
            Some(bytes) => {
                let arr: [u8; 8] = bytes
                    .as_ref()
                    .try_into()
                    .map_err(|_| StoreError::Storage("Invalid schema version format".into()))?;
                Ok(u64::from_le_bytes(arr))
            }
            None => Ok(0),
        }
    }

    pub fn set_version(&self, version: u64) -> Result<(), StoreError> {
        let bytes = version.to_le_bytes();
        self.meta.insert(KEY_META_SCHEMA_VERSION, bytes)?;
        Ok(())
    }

    pub fn run_migrations(&self) -> Result<MigrationReport, StoreError> {
        let current_version = self.get_version()?;
        let mut report = MigrationReport::new(current_version, CURRENT_SCHEMA_VERSION);

        if current_version < 1 {
            self.set_version(1)?;
            report.applied.push(Migration::V0ToV1);
        }

        if current_version > CURRENT_SCHEMA_VERSION {
            return Err(StoreError::VersionMismatch {
                expected: CURRENT_SCHEMA_VERSION,
                actual: current_version,
            });
        }

        Ok(report)
    }
}

#[derive(Debug, Clone)]
pub struct MigrationReport {
    pub from_version: u64,
    pub to_version: u64,
    pub applied: Vec<Migration>,
}

impl MigrationReport {
    pub fn new(from_version: u64, to_version: u64) -> Self {
        Self {
            from_version,
            to_version,
            applied: Vec::new(),
        }
    }

    pub fn migrations_applied(&self) -> bool {
        !self.applied.is_empty()
    }
}

#[derive(Debug, Clone)]
pub enum Migration {
    V0ToV1,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migration_report() {
        let report = MigrationReport::new(0, 1);
        assert_eq!(report.from_version, 0);
        assert_eq!(report.to_version, 1);
        assert!(!report.migrations_applied());
    }

    #[test]
    fn test_current_schema_version() {
        assert_eq!(CURRENT_SCHEMA_VERSION, 1);
    }
}
