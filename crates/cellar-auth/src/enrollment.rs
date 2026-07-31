use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use cellar_config::{BootstrapClaim, PersistedConfig, load_config, save_config};

use crate::AccessClaims;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnrollmentMode {
    Unenrolled,
    Enrolled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteAccess {
    OwnerClaim,
    Authenticated,
}

#[derive(Clone, Eq, PartialEq)]
pub struct EnrollmentSnapshot {
    canonical_origin: String,
    bootstrap_email: Option<String>,
    owner_subject: Option<String>,
    bootstrap_claim: Option<BootstrapClaim>,
}

impl fmt::Debug for EnrollmentSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnrollmentSnapshot")
            .field("canonical_origin", &self.canonical_origin)
            .field(
                "bootstrap_email",
                &self.bootstrap_email.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "owner_subject",
                &self.owner_subject.as_ref().map(|_| "<redacted>"),
            )
            .field("bootstrap_claim", &self.bootstrap_claim)
            .finish()
    }
}

impl EnrollmentSnapshot {
    #[must_use]
    pub fn unenrolled(
        canonical_origin: impl Into<String>,
        bootstrap_email: impl Into<String>,
        bootstrap_claim: BootstrapClaim,
    ) -> Self {
        Self {
            canonical_origin: canonical_origin.into(),
            bootstrap_email: Some(bootstrap_email.into()),
            owner_subject: None,
            bootstrap_claim: Some(bootstrap_claim),
        }
    }

    #[must_use]
    pub fn enrolled(canonical_origin: impl Into<String>, owner_subject: impl Into<String>) -> Self {
        Self {
            canonical_origin: canonical_origin.into(),
            bootstrap_email: None,
            owner_subject: Some(owner_subject.into()),
            bootstrap_claim: None,
        }
    }

    #[must_use]
    pub const fn mode(&self) -> EnrollmentMode {
        if self.owner_subject.is_some() {
            EnrollmentMode::Enrolled
        } else {
            EnrollmentMode::Unenrolled
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct EnrollmentStoreError;

impl EnrollmentStoreError {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    #[must_use]
    pub const fn code(&self) -> &'static str {
        "enrollment_store_unavailable"
    }
}

impl Default for EnrollmentStoreError {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for EnrollmentStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl fmt::Display for EnrollmentStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for EnrollmentStoreError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompareAndSet {
    Saved,
    Changed,
}

pub trait EnrollmentStore: Send + Sync {
    fn load(&self) -> Result<EnrollmentSnapshot, EnrollmentStoreError>;

    fn compare_and_set_owner(
        &self,
        expected: &EnrollmentSnapshot,
        owner_subject: &str,
    ) -> Result<CompareAndSet, EnrollmentStoreError>;
}

pub struct FileEnrollmentStore {
    path: PathBuf,
    mutation_lock: Arc<Mutex<()>>,
}

static FILE_MUTATION_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

impl FileEnrollmentStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Self {
            mutation_lock: shared_mutation_lock(&path),
            path,
        }
    }

    fn read_persisted(&self) -> Result<PersistedConfig, EnrollmentStoreError> {
        load_config(&self.path).map_err(|_| EnrollmentStoreError::new())
    }
}

fn shared_mutation_lock(path: &Path) -> Arc<Mutex<()>> {
    let key = canonical_lock_key(path);
    let registry = FILE_MUTATION_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = registry.get(&key).and_then(Weak::upgrade) {
        return existing;
    }
    let lock = Arc::new(Mutex::new(()));
    registry.insert(key, Arc::downgrade(&lock));
    lock
}

fn canonical_lock_key(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| {
        path.parent()
            .and_then(|parent| fs::canonicalize(parent).ok())
            .and_then(|parent| path.file_name().map(|name| parent.join(name)))
            .unwrap_or_else(|| path.to_path_buf())
    })
}

impl fmt::Debug for FileEnrollmentStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileEnrollmentStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl EnrollmentStore for FileEnrollmentStore {
    fn load(&self) -> Result<EnrollmentSnapshot, EnrollmentStoreError> {
        snapshot(&self.read_persisted()?)
    }

    fn compare_and_set_owner(
        &self,
        expected: &EnrollmentSnapshot,
        owner_subject: &str,
    ) -> Result<CompareAndSet, EnrollmentStoreError> {
        let _guard = self
            .mutation_lock
            .lock()
            .map_err(|_| EnrollmentStoreError::new())?;
        let mut persisted = self.read_persisted()?;
        if owner_subject.is_empty()
            || persisted.config.owner_subject.is_some()
            || snapshot(&persisted)? != *expected
        {
            return Ok(CompareAndSet::Changed);
        }
        persisted.config.owner_subject = Some(owner_subject.to_owned());
        persisted.config.bootstrap_owner_email = None;
        persisted.bootstrap_claim = None;
        save_config(&self.path, &persisted).map_err(|_| EnrollmentStoreError::new())?;
        Ok(CompareAndSet::Saved)
    }
}

fn snapshot(persisted: &PersistedConfig) -> Result<EnrollmentSnapshot, EnrollmentStoreError> {
    let canonical_origin = persisted
        .config
        .external_origin
        .origin()
        .ascii_serialization();
    match (
        persisted.config.bootstrap_owner_email.clone(),
        persisted.config.owner_subject.clone(),
        persisted.bootstrap_claim.clone(),
    ) {
        (Some(email), None, Some(claim)) => Ok(EnrollmentSnapshot::unenrolled(
            canonical_origin,
            email,
            claim,
        )),
        (None, Some(subject), None) => Ok(EnrollmentSnapshot::enrolled(canonical_origin, subject)),
        _ => Err(EnrollmentStoreError::new()),
    }
}

pub struct ClaimRequest<'a> {
    pub email: &'a str,
    pub origin: &'a str,
    pub code: &'a [u8; 32],
}

impl fmt::Debug for ClaimRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClaimRequest")
            .field("email", &"<redacted>")
            .field("origin", &self.origin)
            .field("code", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum EnrollmentError {
    InvalidIdentity,
    Forbidden,
    NotFound,
    Conflict,
    Unavailable,
}

impl EnrollmentError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidIdentity => "invalid_identity",
            Self::Forbidden => "claim_forbidden",
            Self::NotFound => "claim_not_found",
            Self::Conflict => "claim_conflict",
            Self::Unavailable => "enrollment_unavailable",
        }
    }
}

impl fmt::Debug for EnrollmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl fmt::Display for EnrollmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for EnrollmentError {}

pub struct EnrollmentService<S: EnrollmentStore> {
    store: Arc<S>,
}

impl<S: EnrollmentStore> Clone for EnrollmentService<S> {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
        }
    }
}

impl<S: EnrollmentStore> EnrollmentService<S> {
    #[must_use]
    pub const fn new(store: Arc<S>) -> Self {
        Self { store }
    }

    pub fn route_access(
        &self,
        path: &str,
        claims: &AccessClaims,
    ) -> Result<RouteAccess, EnrollmentError> {
        valid_subject(claims)?;
        let state = self
            .store
            .load()
            .map_err(|_| EnrollmentError::Unavailable)?;
        match state.mode() {
            EnrollmentMode::Unenrolled if path == "/owner/claim" => {
                if claims.email.as_deref() == state.bootstrap_email.as_deref() {
                    Ok(RouteAccess::OwnerClaim)
                } else {
                    Err(EnrollmentError::Forbidden)
                }
            }
            EnrollmentMode::Unenrolled => Err(EnrollmentError::Unavailable),
            EnrollmentMode::Enrolled if path == "/owner/claim" => Err(EnrollmentError::NotFound),
            EnrollmentMode::Enrolled => {
                if state.owner_subject.as_deref() == Some(claims.sub.as_str()) {
                    Ok(RouteAccess::Authenticated)
                } else {
                    Err(EnrollmentError::Forbidden)
                }
            }
        }
    }

    pub fn claim(
        &self,
        claims: &AccessClaims,
        request: ClaimRequest<'_>,
        now_unix_seconds: i64,
    ) -> Result<EnrollmentMode, EnrollmentError> {
        valid_subject(claims)?;
        let state = self
            .store
            .load()
            .map_err(|_| EnrollmentError::Unavailable)?;
        if state.mode() == EnrollmentMode::Enrolled {
            return Err(EnrollmentError::NotFound);
        }
        let valid = claims.email.as_deref() == Some(request.email)
            && state.bootstrap_email.as_deref() == Some(request.email)
            && state.canonical_origin == request.origin
            && state
                .bootstrap_claim
                .as_ref()
                .is_some_and(|claim| claim.verify(request.code, now_unix_seconds));
        if !valid {
            return Err(EnrollmentError::Forbidden);
        }
        match self
            .store
            .compare_and_set_owner(&state, &claims.sub)
            .map_err(|_| EnrollmentError::Unavailable)?
        {
            CompareAndSet::Saved => Ok(EnrollmentMode::Enrolled),
            CompareAndSet::Changed => Err(EnrollmentError::Conflict),
        }
    }
}

fn valid_subject(claims: &AccessClaims) -> Result<(), EnrollmentError> {
    if claims.sub.is_empty() {
        Err(EnrollmentError::InvalidIdentity)
    } else {
        Ok(())
    }
}
