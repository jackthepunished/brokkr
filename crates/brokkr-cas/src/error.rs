//! CAS error type.

use brokkr_common::DigestError;
use thiserror::Error;

/// Errors returned by [`crate::Cas`] implementations.
#[derive(Debug, Error)]
pub enum CasError {
    /// The blob was not found in the CAS.
    #[error("blob not found: {0}")]
    NotFound(brokkr_common::Digest),

    /// The submitted bytes did not match the declared digest.
    #[error(transparent)]
    Digest(#[from] DigestError),

    /// Underlying I/O error (on-disk backends).
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Underlying `redb` storage error.
    #[error("redb error: {0}")]
    Redb(String),
}

impl From<redb::Error> for CasError {
    fn from(e: redb::Error) -> Self {
        Self::Redb(e.to_string())
    }
}

impl From<redb::DatabaseError> for CasError {
    fn from(e: redb::DatabaseError) -> Self {
        Self::Redb(e.to_string())
    }
}

impl From<redb::TransactionError> for CasError {
    fn from(e: redb::TransactionError) -> Self {
        Self::Redb(e.to_string())
    }
}

impl From<redb::TableError> for CasError {
    fn from(e: redb::TableError) -> Self {
        Self::Redb(e.to_string())
    }
}

impl From<redb::StorageError> for CasError {
    fn from(e: redb::StorageError) -> Self {
        Self::Redb(e.to_string())
    }
}

impl From<redb::CommitError> for CasError {
    fn from(e: redb::CommitError) -> Self {
        Self::Redb(e.to_string())
    }
}
