use std::error::Error;
use std::fmt;

use thiserror::Error;

/// A condition that prevents Cellar from reporting itself as ready.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReadinessBlocker {
    ConfigurationRequired,
    OwnerEnrollmentRequired,
    MigrationRequired,
    RecoveryRequired,
    StorageUnavailable,
    ReconciliationRequired,
}

impl ReadinessBlocker {
    /// Returns the stable machine-readable blocker code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ConfigurationRequired => "configuration_required",
            Self::OwnerEnrollmentRequired => "owner_enrollment_required",
            Self::MigrationRequired => "migration_required",
            Self::RecoveryRequired => "recovery_required",
            Self::StorageUnavailable => "storage_unavailable",
            Self::ReconciliationRequired => "reconciliation_required",
        }
    }
}

impl fmt::Display for ReadinessBlocker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

/// An application error independent of any transport or runtime framework.
#[derive(Error)]
pub enum CellarError {
    #[error("invalid input")]
    InvalidInput,
    #[error("authentication required")]
    Unauthenticated,
    #[error("access forbidden")]
    Forbidden,
    #[error("conflict")]
    Conflict,
    #[error("invalid range")]
    InvalidRange,
    #[error("storage full")]
    StorageFull,
    #[error("service unavailable")]
    Unavailable,
    #[error("internal error")]
    Internal {
        #[source]
        source: Box<dyn Error + Send + Sync + 'static>,
    },
}

impl fmt::Debug for CellarError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput => formatter.write_str("InvalidInput"),
            Self::Unauthenticated => formatter.write_str("Unauthenticated"),
            Self::Forbidden => formatter.write_str("Forbidden"),
            Self::Conflict => formatter.write_str("Conflict"),
            Self::InvalidRange => formatter.write_str("InvalidRange"),
            Self::StorageFull => formatter.write_str("StorageFull"),
            Self::Unavailable => formatter.write_str("Unavailable"),
            Self::Internal { .. } => formatter.write_str("Internal { source: <redacted> }"),
        }
    }
}

impl CellarError {
    /// Wraps an internal source while keeping its details out of the safe display message.
    pub fn internal(source: impl Error + Send + Sync + 'static) -> Self {
        Self::Internal {
            source: Box::new(source),
        }
    }

    /// Returns the stable machine-readable error category code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::Unauthenticated => "unauthenticated",
            Self::Forbidden => "forbidden",
            Self::Conflict => "conflict",
            Self::InvalidRange => "invalid_range",
            Self::StorageFull => "storage_full",
            Self::Unavailable => "unavailable",
            Self::Internal { .. } => "internal",
        }
    }
}
