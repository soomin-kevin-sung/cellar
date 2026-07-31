use std::collections::HashSet;
use std::io;

use thiserror::Error;
use url::Url;

use crate::CellarConfig;

pub const MAX_AUD_TAG_BYTES: usize = 256;

/// Fail-closed configuration failures with stable machine-readable codes.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("external origin must include a host")]
    ExternalOriginMissingHost,
    #[error("external origin must use HTTPS")]
    ExternalOriginMustBeHttps,
    #[error("external origin must not contain credentials")]
    ExternalOriginCredentialsForbidden,
    #[error("external origin must not contain a query")]
    ExternalOriginQueryForbidden,
    #[error("external origin must not contain a fragment")]
    ExternalOriginFragmentForbidden,
    #[error("external origin must contain only an origin")]
    ExternalOriginMustBeOriginOnly,
    #[error("team domain must include a host")]
    TeamDomainMissingHost,
    #[error("team domain must use HTTPS")]
    TeamDomainMustBeHttps,
    #[error("team domain must not contain credentials")]
    TeamDomainCredentialsForbidden,
    #[error("team domain must not contain a query")]
    TeamDomainQueryForbidden,
    #[error("team domain must not contain a fragment")]
    TeamDomainFragmentForbidden,
    #[error("team domain must contain only an origin")]
    TeamDomainMustBeOriginOnly,
    #[error("at least one audience tag is required")]
    AudTagsRequired,
    #[error("audience tags must not be empty")]
    AudTagEmpty,
    #[error("audience tags must not contain surrounding whitespace")]
    AudTagNotTrimmed,
    #[error("audience tags must not exceed {MAX_AUD_TAG_BYTES} bytes")]
    AudTagTooLong,
    #[error("audience tags must be unique")]
    AudTagsDuplicate,
    #[error("bootstrap owner email and owner subject are mutually exclusive")]
    OwnerIdentityConflict,
    #[error("one owner identity state is required")]
    OwnerIdentityRequired,
    #[error("bootstrap owner email must not be empty")]
    BootstrapOwnerEmailEmpty,
    #[error("bootstrap owner email must not contain surrounding whitespace")]
    BootstrapOwnerEmailNotTrimmed,
    #[error("owner subject must not be empty")]
    OwnerSubjectEmpty,
    #[error("owner subject must not contain surrounding whitespace")]
    OwnerSubjectNotTrimmed,
    #[error("bootstrap claim is forbidden after owner enrollment")]
    BootstrapClaimForbiddenWhenEnrolled,
    #[error("storage root must be an absolute path")]
    StorageRootMustBeAbsolute,
    #[error("origin port must be nonzero")]
    OriginPortMustBeNonzero,
    #[error("health port must be nonzero")]
    HealthPortMustBeNonzero,
    #[error("origin and health listener ports must differ")]
    ListenerPortsConflict,
    #[error("configuration I/O failed")]
    Io(#[from] io::Error),
    #[error("configuration TOML is malformed")]
    Parse(#[from] toml::de::Error),
    #[error("configuration could not be serialized")]
    Serialize(#[from] toml::ser::Error),
}

impl ConfigError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ExternalOriginMissingHost => "external_origin_missing_host",
            Self::ExternalOriginMustBeHttps => "external_origin_must_be_https",
            Self::ExternalOriginCredentialsForbidden => "external_origin_credentials_forbidden",
            Self::ExternalOriginQueryForbidden => "external_origin_query_forbidden",
            Self::ExternalOriginFragmentForbidden => "external_origin_fragment_forbidden",
            Self::ExternalOriginMustBeOriginOnly => "external_origin_must_be_origin_only",
            Self::TeamDomainMissingHost => "team_domain_missing_host",
            Self::TeamDomainMustBeHttps => "team_domain_must_be_https",
            Self::TeamDomainCredentialsForbidden => "team_domain_credentials_forbidden",
            Self::TeamDomainQueryForbidden => "team_domain_query_forbidden",
            Self::TeamDomainFragmentForbidden => "team_domain_fragment_forbidden",
            Self::TeamDomainMustBeOriginOnly => "team_domain_must_be_origin_only",
            Self::AudTagsRequired => "aud_tags_required",
            Self::AudTagEmpty => "aud_tag_empty",
            Self::AudTagNotTrimmed => "aud_tag_not_trimmed",
            Self::AudTagTooLong => "aud_tag_too_long",
            Self::AudTagsDuplicate => "aud_tags_duplicate",
            Self::OwnerIdentityConflict => "owner_identity_conflict",
            Self::OwnerIdentityRequired => "owner_identity_required",
            Self::BootstrapOwnerEmailEmpty => "bootstrap_owner_email_empty",
            Self::BootstrapOwnerEmailNotTrimmed => "bootstrap_owner_email_not_trimmed",
            Self::OwnerSubjectEmpty => "owner_subject_empty",
            Self::OwnerSubjectNotTrimmed => "owner_subject_not_trimmed",
            Self::BootstrapClaimForbiddenWhenEnrolled => "bootstrap_claim_forbidden_when_enrolled",
            Self::StorageRootMustBeAbsolute => "storage_root_must_be_absolute",
            Self::OriginPortMustBeNonzero => "origin_port_must_be_nonzero",
            Self::HealthPortMustBeNonzero => "health_port_must_be_nonzero",
            Self::ListenerPortsConflict => "listener_ports_conflict",
            Self::Io(_) => "config_io_error",
            Self::Parse(_) => "config_parse_error",
            Self::Serialize(_) => "config_serialize_error",
        }
    }
}

pub(crate) fn validate(config: &CellarConfig) -> Result<(), ConfigError> {
    validate_origin(&config.external_origin, OriginKind::External)?;
    validate_origin(&config.team_domain, OriginKind::TeamDomain)?;
    validate_aud_tags(&config.aud_tags)?;
    validate_owner_identity(
        config.bootstrap_owner_email.as_deref(),
        config.owner_subject.as_deref(),
    )?;

    if !config.storage_root.is_absolute() {
        return Err(ConfigError::StorageRootMustBeAbsolute);
    }
    if config.origin_port == 0 {
        return Err(ConfigError::OriginPortMustBeNonzero);
    }
    if config.health_port == 0 {
        return Err(ConfigError::HealthPortMustBeNonzero);
    }
    if config.origin_port == config.health_port {
        return Err(ConfigError::ListenerPortsConflict);
    }

    Ok(())
}

enum OriginKind {
    External,
    TeamDomain,
}

fn validate_origin(url: &Url, kind: OriginKind) -> Result<(), ConfigError> {
    if !url.has_host() {
        return Err(match kind {
            OriginKind::External => ConfigError::ExternalOriginMissingHost,
            OriginKind::TeamDomain => ConfigError::TeamDomainMissingHost,
        });
    }
    if url.scheme() != "https" {
        return Err(match kind {
            OriginKind::External => ConfigError::ExternalOriginMustBeHttps,
            OriginKind::TeamDomain => ConfigError::TeamDomainMustBeHttps,
        });
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(match kind {
            OriginKind::External => ConfigError::ExternalOriginCredentialsForbidden,
            OriginKind::TeamDomain => ConfigError::TeamDomainCredentialsForbidden,
        });
    }
    if url.query().is_some() {
        return Err(match kind {
            OriginKind::External => ConfigError::ExternalOriginQueryForbidden,
            OriginKind::TeamDomain => ConfigError::TeamDomainQueryForbidden,
        });
    }
    if url.fragment().is_some() {
        return Err(match kind {
            OriginKind::External => ConfigError::ExternalOriginFragmentForbidden,
            OriginKind::TeamDomain => ConfigError::TeamDomainFragmentForbidden,
        });
    }
    if !matches!(url.path(), "" | "/") {
        return Err(match kind {
            OriginKind::External => ConfigError::ExternalOriginMustBeOriginOnly,
            OriginKind::TeamDomain => ConfigError::TeamDomainMustBeOriginOnly,
        });
    }

    Ok(())
}

fn validate_aud_tags(aud_tags: &[String]) -> Result<(), ConfigError> {
    if aud_tags.is_empty() {
        return Err(ConfigError::AudTagsRequired);
    }

    let mut unique = HashSet::with_capacity(aud_tags.len());
    for tag in aud_tags {
        if tag.trim().is_empty() {
            return Err(ConfigError::AudTagEmpty);
        }
        if tag.trim() != tag {
            return Err(ConfigError::AudTagNotTrimmed);
        }
        if tag.len() > MAX_AUD_TAG_BYTES {
            return Err(ConfigError::AudTagTooLong);
        }
        if !unique.insert(tag) {
            return Err(ConfigError::AudTagsDuplicate);
        }
    }

    Ok(())
}

fn validate_owner_identity(
    bootstrap_owner_email: Option<&str>,
    owner_subject: Option<&str>,
) -> Result<(), ConfigError> {
    match (bootstrap_owner_email, owner_subject) {
        (Some(_), Some(_)) => return Err(ConfigError::OwnerIdentityConflict),
        (None, None) => return Err(ConfigError::OwnerIdentityRequired),
        (Some(email), None) => validate_identity_value(
            email,
            ConfigError::BootstrapOwnerEmailEmpty,
            ConfigError::BootstrapOwnerEmailNotTrimmed,
        )?,
        (None, Some(subject)) => validate_identity_value(
            subject,
            ConfigError::OwnerSubjectEmpty,
            ConfigError::OwnerSubjectNotTrimmed,
        )?,
    }

    Ok(())
}

fn validate_identity_value(
    value: &str,
    empty_error: ConfigError,
    not_trimmed_error: ConfigError,
) -> Result<(), ConfigError> {
    if value.trim().is_empty() {
        return Err(empty_error);
    }
    if value.trim() != value {
        return Err(not_trimmed_error);
    }
    Ok(())
}
