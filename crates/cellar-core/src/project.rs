use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::{OperationId, ProjectId};

pub const MAX_PROJECT_NAME_BYTES: usize = 255;
pub const MAX_PROJECT_DESCRIPTION_BYTES: usize = 8 * 1024;
pub const MAX_PROJECT_LIST_LIMIT: u32 = 100;

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum ProjectValidationError {
    InvalidName,
    InvalidDescription,
    InvalidStatus,
    EmptyPatch,
}

impl ProjectValidationError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidName => "invalid_project_name",
            Self::InvalidDescription => "invalid_project_description",
            Self::InvalidStatus => "invalid_project_status",
            Self::EmptyPatch => "empty_project_patch",
        }
    }
}

impl fmt::Debug for ProjectValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl fmt::Display for ProjectValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for ProjectValidationError {}

#[derive(Clone, Eq, PartialEq)]
pub struct ProjectName(String);

impl ProjectName {
    pub fn parse(value: impl Into<String>) -> Result<Self, ProjectValidationError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_PROJECT_NAME_BYTES
            || value.trim() != value
            || value.chars().any(char::is_control)
        {
            return Err(ProjectValidationError::InvalidName);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ProjectName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProjectName(<redacted>)")
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ProjectDescription(String);

impl ProjectDescription {
    pub fn parse(value: impl Into<String>) -> Result<Self, ProjectValidationError> {
        let value = value.into();
        if value.len() > MAX_PROJECT_DESCRIPTION_BYTES || value.contains('\0') {
            return Err(ProjectValidationError::InvalidDescription);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ProjectDescription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProjectDescription(<redacted>)")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewProject {
    pub name: ProjectName,
    pub description: ProjectDescription,
}

impl NewProject {
    pub fn try_new(
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<Self, ProjectValidationError> {
        Ok(Self {
            name: ProjectName::parse(name)?,
            description: ProjectDescription::parse(description)?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectPatch {
    pub name: Option<ProjectName>,
    pub description: Option<ProjectDescription>,
}

impl ProjectPatch {
    pub fn try_new(
        name: Option<String>,
        description: Option<String>,
    ) -> Result<Self, ProjectValidationError> {
        if name.is_none() && description.is_none() {
            return Err(ProjectValidationError::EmptyPatch);
        }
        Ok(Self {
            name: name.map(ProjectName::parse).transpose()?,
            description: description.map(ProjectDescription::parse).transpose()?,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectStatus {
    Active,
    Archived,
}

impl ProjectStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
        }
    }
}

impl FromStr for ProjectStatus {
    type Err = ProjectValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "active" => Ok(Self::Active),
            "archived" => Ok(Self::Archived),
            _ => Err(ProjectValidationError::InvalidStatus),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Project {
    pub id: ProjectId,
    pub name: ProjectName,
    pub description: ProjectDescription,
    pub status: ProjectStatus,
    pub version: i64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub deleted_at: Option<OffsetDateTime>,
}

impl Project {
    #[must_use]
    pub fn from_new(id: ProjectId, input: NewProject, now: OffsetDateTime) -> Self {
        Self {
            id,
            name: input.name,
            description: input.description,
            status: ProjectStatus::Active,
            version: 1,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectListFilter {
    All,
    Status(ProjectStatus),
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum ProjectRepositoryError {
    NotFound,
    Stale,
    Conflict,
    Unavailable,
}

impl ProjectRepositoryError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotFound => "project_not_found",
            Self::Stale => "stale_project_version",
            Self::Conflict => "project_conflict",
            Self::Unavailable => "project_repository_unavailable",
        }
    }
}

impl fmt::Debug for ProjectRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl fmt::Display for ProjectRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for ProjectRepositoryError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationStart {
    New,
    Completed(Project),
    InProgress,
}

#[async_trait]
pub trait ProjectRepository: Send + Sync {
    async fn begin_create(
        &self,
        operation_id: OperationId,
        project_id: ProjectId,
        request_digest: &str,
        now: OffsetDateTime,
    ) -> Result<OperationStart, ProjectRepositoryError>;

    async fn mark_create_fs_applied(
        &self,
        operation_id: OperationId,
        now: OffsetDateTime,
    ) -> Result<(), ProjectRepositoryError>;

    async fn mark_create_failed(
        &self,
        operation_id: OperationId,
        error_code: &'static str,
        now: OffsetDateTime,
    ) -> Result<(), ProjectRepositoryError>;

    async fn create(
        &self,
        operation_id: OperationId,
        project: &Project,
    ) -> Result<Project, ProjectRepositoryError>;

    async fn read(&self, id: ProjectId) -> Result<Project, ProjectRepositoryError>;

    async fn list(
        &self,
        filter: ProjectListFilter,
        limit: u32,
    ) -> Result<Vec<Project>, ProjectRepositoryError>;

    async fn update(
        &self,
        id: ProjectId,
        expected_version: i64,
        patch: &ProjectPatch,
        now: OffsetDateTime,
    ) -> Result<Project, ProjectRepositoryError>;

    async fn archive(
        &self,
        id: ProjectId,
        expected_version: i64,
        now: OffsetDateTime,
    ) -> Result<Project, ProjectRepositoryError>;
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum DirectoryStoreError {
    Conflict,
    Unavailable,
}

impl DirectoryStoreError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Conflict => "project_destination_conflict",
            Self::Unavailable => "project_storage_unavailable",
        }
    }
}

impl fmt::Debug for DirectoryStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl fmt::Display for DirectoryStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for DirectoryStoreError {}

#[async_trait]
pub trait ProjectDirectoryStore: Send + Sync {
    /// Durably creates logical `projects/<project-id>/files` with no-replace semantics.
    ///
    /// `Ok(())` is returned only after the mutation is durable. Errors certify
    /// that no destination mutation was applied, so the journal may safely be
    /// marked failed; adapters must never report an error after applying it.
    async fn create_project_directory(&self, id: ProjectId) -> Result<(), DirectoryStoreError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateProjectResult {
    pub project: Project,
    pub replayed: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum ProjectServiceError {
    NotFound,
    Stale,
    Conflict,
    IdempotencyConflict,
    InProgress,
    Unavailable,
}

impl ProjectServiceError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotFound => "project_not_found",
            Self::Stale => "stale_project_version",
            Self::Conflict => "project_destination_conflict",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::InProgress => "project_create_in_progress",
            Self::Unavailable => "project_service_unavailable",
        }
    }
}

impl fmt::Debug for ProjectServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl fmt::Display for ProjectServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for ProjectServiceError {}

#[derive(Clone)]
pub struct ProjectService {
    repository: Arc<dyn ProjectRepository>,
    directories: Arc<dyn ProjectDirectoryStore>,
}

impl ProjectService {
    #[must_use]
    pub fn new(
        repository: Arc<dyn ProjectRepository>,
        directories: Arc<dyn ProjectDirectoryStore>,
    ) -> Self {
        Self {
            repository,
            directories,
        }
    }

    pub async fn create(
        &self,
        input: NewProject,
        idempotency_key: Option<OperationId>,
        now: OffsetDateTime,
    ) -> Result<CreateProjectResult, ProjectServiceError> {
        let operation_id = idempotency_key.unwrap_or_default();
        let project_id = ProjectId::new();
        let digest = request_digest(&input);
        match self
            .repository
            .begin_create(operation_id, project_id, &digest, now)
            .await
        {
            Ok(OperationStart::Completed(project)) => {
                return Ok(CreateProjectResult {
                    project,
                    replayed: true,
                });
            }
            Ok(OperationStart::InProgress) => return Err(ProjectServiceError::InProgress),
            Ok(OperationStart::New) => {}
            Err(ProjectRepositoryError::Conflict) => {
                return Err(ProjectServiceError::IdempotencyConflict);
            }
            Err(error) => return Err(map_repository_error(error)),
        }
        let project = Project::from_new(project_id, input, now);
        if let Err(error) = self.directories.create_project_directory(project_id).await {
            let _ = self
                .repository
                .mark_create_failed(operation_id, error.code(), now)
                .await;
            return Err(match error {
                DirectoryStoreError::Conflict => ProjectServiceError::Conflict,
                DirectoryStoreError::Unavailable => ProjectServiceError::Unavailable,
            });
        }
        self.repository
            .mark_create_fs_applied(operation_id, now)
            .await
            .map_err(map_repository_error)?;
        let project = self
            .repository
            .create(operation_id, &project)
            .await
            .map_err(map_repository_error)?;
        Ok(CreateProjectResult {
            project,
            replayed: false,
        })
    }

    pub async fn read(&self, id: ProjectId) -> Result<Project, ProjectServiceError> {
        self.repository.read(id).await.map_err(map_repository_error)
    }

    pub async fn list(
        &self,
        filter: ProjectListFilter,
        limit: u32,
    ) -> Result<Vec<Project>, ProjectServiceError> {
        self.repository
            .list(filter, limit.min(MAX_PROJECT_LIST_LIMIT))
            .await
            .map_err(map_repository_error)
    }

    pub async fn update(
        &self,
        id: ProjectId,
        expected_version: i64,
        patch: &ProjectPatch,
        now: OffsetDateTime,
    ) -> Result<Project, ProjectServiceError> {
        self.repository
            .update(id, expected_version, patch, now)
            .await
            .map_err(map_repository_error)
    }

    pub async fn archive(
        &self,
        id: ProjectId,
        expected_version: i64,
        now: OffsetDateTime,
    ) -> Result<Project, ProjectServiceError> {
        self.repository
            .archive(id, expected_version, now)
            .await
            .map_err(map_repository_error)
    }
}

fn map_repository_error(error: ProjectRepositoryError) -> ProjectServiceError {
    match error {
        ProjectRepositoryError::NotFound => ProjectServiceError::NotFound,
        ProjectRepositoryError::Stale => ProjectServiceError::Stale,
        ProjectRepositoryError::Conflict => ProjectServiceError::Conflict,
        ProjectRepositoryError::Unavailable => ProjectServiceError::Unavailable,
    }
}

fn request_digest(input: &NewProject) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"cellar-project-create-v1\0");
    hash_field(&mut hasher, input.name.as_str().as_bytes());
    hash_field(&mut hasher, input.description.as_str().as_bytes());
    format!("{:x}", hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_is_byte_bounded_and_redacted() {
        assert!(ProjectName::parse("a".repeat(MAX_PROJECT_NAME_BYTES)).is_ok());
        let error = ProjectName::parse(" secret ").unwrap_err();
        assert_eq!(error.code(), "invalid_project_name");
        assert!(!format!("{error:?}").contains("secret"));
        assert!(ProjectDescription::parse("ordinary\nwhitespace\t").is_ok());
        assert!(ProjectDescription::parse("bad\0description").is_err());
    }

    #[test]
    fn create_digest_is_deterministic_and_unambiguous() {
        let first = NewProject::try_new("a", "bc").unwrap();
        let same = NewProject::try_new("a", "bc").unwrap();
        let different = NewProject::try_new("ab", "c").unwrap();
        assert_eq!(request_digest(&first), request_digest(&same));
        assert_ne!(request_digest(&first), request_digest(&different));
    }
}
