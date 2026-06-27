//! Worker and job identifier newtypes.
//!
//! These replace raw `String` usages for worker and job IDs throughout the
//! Brokkr codebase, providing compile-time type safety and validation.

use std::convert::TryFrom;
use std::fmt;
use std::str::FromStr;

use serde::Serialize;
use thiserror::Error;

/// Maximum byte length for any worker or job identifier.
pub const ID_MAX_LEN: usize = 128;

/// Errors that can occur when constructing an ID.
#[derive(Debug, Error, PartialEq, Eq, Clone)]
pub enum IdError {
    /// Identifier was empty.
    #[error("id cannot be empty")]
    Empty,
    /// Identifier exceeded [`ID_MAX_LEN`] bytes.
    #[error("id exceeds maximum length of {max}: got {len}")]
    TooLong {
        /// Maximum allowed byte length.
        max: usize,
        /// Actual byte length of the identifier.
        len: usize,
    },
}

/// A worker node identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(try_from = "String")]
pub struct WorkerId(String);

impl WorkerId {
    /// Construct a [`WorkerId`] from a [`String`], validating it is non-empty
    /// and within [`ID_MAX_LEN`].
    pub fn new(inner: String) -> Result<Self, IdError> {
        if inner.is_empty() {
            return Err(IdError::Empty);
        }
        if inner.len() > ID_MAX_LEN {
            return Err(IdError::TooLong {
                max: ID_MAX_LEN,
                len: inner.len(),
            });
        }
        Ok(Self(inner))
    }

    /// Returns the raw string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes self and returns the inner [`String`].
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for WorkerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for WorkerId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for WorkerId {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s.to_string())
    }
}

impl TryFrom<String> for WorkerId {
    type Error = IdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for WorkerId {
    type Error = IdError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value.to_string())
    }
}

// Note: WorkerId intentionally has no Borrow<str> impl — it is never
// used as a HashMap key, so the ergonomic lookup trick is unnecessary.

/// A job (work item) identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(try_from = "String")]
pub struct JobId(String);

impl JobId {
    /// Construct a [`JobId`] from a [`String`], validating it is non-empty
    /// and within [`ID_MAX_LEN`].
    pub fn new(inner: String) -> Result<Self, IdError> {
        if inner.is_empty() {
            return Err(IdError::Empty);
        }
        if inner.len() > ID_MAX_LEN {
            return Err(IdError::TooLong {
                max: ID_MAX_LEN,
                len: inner.len(),
            });
        }
        Ok(Self(inner))
    }

    /// Returns the raw string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes self and returns the inner [`String`].
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for JobId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for JobId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl FromStr for JobId {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s.to_string())
    }
}

impl TryFrom<String> for JobId {
    type Error = IdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for JobId {
    type Error = IdError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value.to_string())
    }
}

/// The tenant id used when a request carries no explicit tenant.
pub const DEFAULT_TENANT: &str = "default";

/// A tenant (authenticated entity) identifier. Carried through the control
/// plane for quota accounting and fair scheduling; the source is a request
/// metadata header until auth makes it authoritative (plan §16 task 8).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(try_from = "String")]
pub struct TenantId(String);

impl TenantId {
    /// Construct a [`TenantId`], validating it is non-empty and within
    /// [`ID_MAX_LEN`].
    pub fn new(inner: String) -> Result<Self, IdError> {
        if inner.is_empty() {
            return Err(IdError::Empty);
        }
        if inner.len() > ID_MAX_LEN {
            return Err(IdError::TooLong {
                max: ID_MAX_LEN,
                len: inner.len(),
            });
        }
        Ok(Self(inner))
    }

    /// Returns the raw string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes self and returns the inner [`String`].
    pub fn into_string(self) -> String {
        self.0
    }
}

impl Default for TenantId {
    fn default() -> Self {
        Self(DEFAULT_TENANT.to_string())
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for TenantId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for TenantId {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s.to_string())
    }
}

impl TryFrom<String> for TenantId {
    type Error = IdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for TenantId {
    type Error = IdError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value.to_string())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]
mod tests {
    use super::*;

    // WorkerId tests
    #[test]
    fn worker_id_new_accepts_valid_string() {
        let id = WorkerId::new("worker-abc123".to_string()).unwrap();
        assert_eq!(id.as_str(), "worker-abc123");
    }

    #[test]
    fn worker_id_new_rejects_empty() {
        let err = WorkerId::new("".to_string()).unwrap_err();
        assert_eq!(err, IdError::Empty);
    }

    #[test]
    fn worker_id_new_rejects_too_long() {
        let long = "a".repeat(ID_MAX_LEN + 1);
        let err = WorkerId::new(long).unwrap_err();
        assert_eq!(
            err,
            IdError::TooLong {
                max: ID_MAX_LEN,
                len: ID_MAX_LEN + 1
            }
        );
    }

    #[test]
    fn worker_id_display_and_fromstr_roundtrip() {
        let id = WorkerId::new("worker-xyz".to_string()).unwrap();
        let s = id.to_string();
        let parsed: WorkerId = s.parse().unwrap();
        assert_eq!(id, parsed);
    }

    // JobId tests
    #[test]
    fn job_id_new_accepts_valid_string() {
        let id = JobId::new("job-abc123".to_string()).unwrap();
        assert_eq!(id.as_str(), "job-abc123");
    }

    #[test]
    fn job_id_new_rejects_empty() {
        let err = JobId::new("".to_string()).unwrap_err();
        assert_eq!(err, IdError::Empty);
    }

    #[test]
    fn job_id_new_rejects_too_long() {
        let long = "j".repeat(ID_MAX_LEN + 1);
        let err = JobId::new(long).unwrap_err();
        assert_eq!(
            err,
            IdError::TooLong {
                max: ID_MAX_LEN,
                len: ID_MAX_LEN + 1
            }
        );
    }

    #[test]
    fn job_id_display_and_fromstr_roundtrip() {
        let id = JobId::new("job-xyz".to_string()).unwrap();
        let s = id.to_string();
        let parsed: JobId = s.parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn job_id_borrow_enables_hashmap_remove() {
        use std::collections::HashMap;
        let mut map: HashMap<JobId, &'static str> = HashMap::new();
        let id = JobId::new("job-key".to_string()).unwrap();
        map.insert(id.clone(), "value");
        // Should work with &str lookup via Borrow<str>
        assert_eq!(map.remove("job-key"), Some("value"));
    }

    // TenantId tests
    #[test]
    fn tenant_id_default_is_default_tenant() {
        assert_eq!(TenantId::default().as_str(), DEFAULT_TENANT);
        assert_eq!(DEFAULT_TENANT, "default");
    }

    #[test]
    fn tenant_id_new_validates_and_roundtrips() {
        assert_eq!(TenantId::new(String::new()).unwrap_err(), IdError::Empty);
        let id = TenantId::new("team-acme".to_string()).unwrap();
        assert_eq!(id.as_str(), "team-acme");
        let parsed: TenantId = id.to_string().parse().unwrap();
        assert_eq!(id, parsed);
    }
}
