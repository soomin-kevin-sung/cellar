//! Application configuration.

use std::{
    fmt, fs,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
};

use serde::Deserialize;
use thiserror::Error;
use url::Url;

#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    bind: SocketAddr,
    external_origin: CanonicalOrigin,
    data_root: PathBuf,
    database_path: PathBuf,
    access: AccessConfig,
}

#[derive(Clone, PartialEq, Eq)]
pub struct AccessConfig {
    team_domain: CanonicalOrigin,
    audience: String,
    owner_email: String,
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("bind", &self.bind)
            .field("external_origin", &self.external_origin.as_str())
            .field("data_root", &"<redacted>")
            .field("database_path", &"<redacted>")
            .field("access", &self.access)
            .finish()
    }
}

impl fmt::Debug for AccessConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccessConfig")
            .field("team_domain", &self.team_domain.as_str())
            .field("audience", &"<redacted>")
            .field("owner_email", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalOrigin {
    exact: String,
    url: Url,
}

impl CanonicalOrigin {
    pub fn as_str(&self) -> &str {
        &self.exact
    }

    pub(crate) fn url(&self) -> &Url {
        &self.url
    }
}

impl AccessConfig {
    pub fn team_domain(&self) -> &CanonicalOrigin {
        &self.team_domain
    }

    pub fn audience(&self) -> &str {
        &self.audience
    }

    pub fn owner_email(&self) -> &str {
        &self.owner_email
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid configuration ({code})")]
pub struct ConfigError {
    code: &'static str,
}

impl ConfigError {
    pub fn code(&self) -> &'static str {
        self.code
    }

    fn new(code: &'static str) -> Self {
        Self { code }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    bind: String,
    external_origin: String,
    data_root: PathBuf,
    database_path: PathBuf,
    access: RawAccessConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAccessConfig {
    team_domain: String,
    audience: String,
    owner_email: String,
}

impl Config {
    pub fn bind(&self) -> SocketAddr {
        self.bind
    }

    pub fn external_origin(&self) -> &CanonicalOrigin {
        &self.external_origin
    }

    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub fn access(&self) -> &AccessConfig {
        &self.access
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let input = fs::read_to_string(path).map_err(|_| ConfigError::new("config_read_failed"))?;
        Self::parse(&input)
    }

    pub fn parse(input: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(input).map_err(|_| ConfigError::new("invalid_toml"))?;
        let bind: SocketAddr = raw
            .bind
            .parse()
            .map_err(|_| ConfigError::new("invalid_bind_address"))?;
        if !bind.ip().is_loopback() {
            return Err(ConfigError::new("bind_must_be_loopback"));
        }
        let external_origin =
            parse_canonical_origin(&raw.external_origin, "invalid_external_origin")?;
        let team_domain = parse_canonical_origin(&raw.access.team_domain, "invalid_team_domain")?;
        let team_host = team_domain
            .url()
            .host_str()
            .and_then(|host| host.strip_suffix(".cloudflareaccess.com"));
        if team_domain.url().port().is_some()
            || !team_host.is_some_and(|team| !team.is_empty() && !team.contains('.'))
        {
            return Err(ConfigError::new("invalid_team_domain"));
        }
        if raw.access.audience.is_empty() || raw.access.audience != raw.access.audience.trim() {
            return Err(ConfigError::new("invalid_access_audience"));
        }
        let owner_email = normalize_owner_email(&raw.access.owner_email)?;
        if !raw.data_root.is_absolute() {
            return Err(ConfigError::new("data_root_must_be_absolute"));
        }
        if !raw.database_path.is_absolute() {
            return Err(ConfigError::new("database_path_must_be_absolute"));
        }
        if raw
            .database_path
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err(ConfigError::new("database_path_contains_traversal"));
        }
        let cellar_directory = raw.data_root.join(".cellar");
        if raw.database_path == cellar_directory
            || !raw.database_path.starts_with(&cellar_directory)
        {
            return Err(ConfigError::new("database_path_must_be_inside_cellar"));
        }

        Ok(Self {
            bind,
            external_origin,
            data_root: raw.data_root,
            database_path: raw.database_path,
            access: AccessConfig {
                team_domain,
                audience: raw.access.audience,
                owner_email,
            },
        })
    }
}

fn normalize_owner_email(input: &str) -> Result<String, ConfigError> {
    let trimmed = input.trim();
    if trimmed
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(ConfigError::new("invalid_owner_email"));
    }

    let normalized = trimmed.to_ascii_lowercase();
    let mut parts = normalized.split('@');
    let local = parts.next().unwrap_or_default();
    let domain = parts.next().unwrap_or_default();
    if parts.next().is_none() && !local.is_empty() && !domain.is_empty() {
        Ok(normalized)
    } else {
        Err(ConfigError::new("invalid_owner_email"))
    }
}

fn parse_canonical_origin(input: &str, code: &'static str) -> Result<CanonicalOrigin, ConfigError> {
    let url = Url::parse(input).map_err(|_| ConfigError::new(code))?;
    let canonical = url.origin().ascii_serialization();
    let valid = url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.host_str().is_some_and(|host| !host.contains('*'))
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
        && input == canonical;

    if valid {
        Ok(CanonicalOrigin {
            exact: input.to_owned(),
            url,
        })
    } else {
        Err(ConfigError::new(code))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::Config;

    fn valid_config() -> String {
        r#"bind = "127.0.0.1:8787"
external_origin = "https://files.example.com"
data_root = "D:/CellarData"
database_path = "D:/CellarData/.cellar/cellar.db"

[access]
team_domain = "https://example.cloudflareaccess.com"
audience = "replace-with-access-application-aud"
owner_email = "owner@example.com"
"#
        .to_owned()
    }

    #[test]
    fn parses_valid_loopback_configuration() {
        let config = Config::parse(&valid_config()).unwrap();

        assert!(config.bind().ip().is_loopback());
        assert_eq!(config.bind().port(), 8787);
    }

    #[test]
    fn preserves_exact_external_origin_without_trailing_slash() {
        let config = Config::parse(&valid_config()).unwrap();

        assert_eq!(
            config.external_origin().as_str(),
            "https://files.example.com"
        );
    }

    #[test]
    fn exposes_validated_configuration_through_read_only_getters() {
        let config = Config::parse(&valid_config()).unwrap();

        assert_eq!(config.data_root(), std::path::Path::new("D:/CellarData"));
        assert_eq!(
            config.database_path(),
            std::path::Path::new("D:/CellarData/.cellar/cellar.db")
        );
        assert_eq!(
            config.access().team_domain().as_str(),
            "https://example.cloudflareaccess.com"
        );
        assert_eq!(
            config.access().audience(),
            "replace-with-access-application-aud"
        );
        assert_eq!(config.access().owner_email(), "owner@example.com");
    }

    #[test]
    fn debug_output_redacts_owner_audience_and_absolute_paths() {
        let config = Config::parse(&valid_config()).unwrap();
        let debug = format!("{config:?}");

        for sensitive in [
            "owner@example.com",
            "replace-with-access-application-aud",
            "D:/CellarData",
            "D:/CellarData/.cellar/cellar.db",
        ] {
            assert!(
                !debug.contains(sensitive),
                "debug output exposed {sensitive:?}: {debug}"
            );
        }
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn rejects_non_loopback_bind_address() {
        let input = valid_config().replace("127.0.0.1:8787", "0.0.0.0:8787");
        assert_eq!(
            Config::parse(&input).unwrap_err().code(),
            "bind_must_be_loopback"
        );
    }

    #[test]
    fn rejects_lan_and_hostname_bind_addresses() {
        for (address, code) in [
            ("192.168.1.10:8787", "bind_must_be_loopback"),
            ("localhost:8787", "invalid_bind_address"),
        ] {
            let input = valid_config().replace("127.0.0.1:8787", address);
            assert_eq!(Config::parse(&input).unwrap_err().code(), code);
        }
    }

    #[test]
    fn accepts_ipv6_loopback_bind_address() {
        let input = valid_config().replace("127.0.0.1:8787", "[::1]:8787");
        assert!(Config::parse(&input).unwrap().bind().ip().is_loopback());
    }

    #[test]
    fn rejects_external_origin_trailing_slash() {
        let input = valid_config().replace(
            "https://files.example.com\"",
            "https://files.example.com/\"",
        );
        assert_eq!(
            Config::parse(&input).unwrap_err().code(),
            "invalid_external_origin"
        );
    }

    #[test]
    fn rejects_noncanonical_or_unsafe_external_origins() {
        for origin in [
            "http://files.example.com",
            "https://user@files.example.com",
            "https://user:pass@files.example.com",
            "https://files.example.com/path",
            "https://files.example.com?query=yes",
            "https://files.example.com#fragment",
            "https://*.example.com",
            "https://FILES.example.com",
            "https://files.example.com:443",
        ] {
            let input =
                valid_config().replace("https://files.example.com\"", &format!("{origin}\""));
            assert_eq!(
                Config::parse(&input).unwrap_err().code(),
                "invalid_external_origin",
                "origin {origin:?} was accepted"
            );
        }
    }

    #[test]
    fn rejects_relative_data_root() {
        let input = valid_config().replace("D:/CellarData\"", "CellarData\"");
        assert_eq!(
            Config::parse(&input).unwrap_err().code(),
            "data_root_must_be_absolute"
        );
    }

    #[test]
    fn rejects_relative_database_path() {
        let input = valid_config().replace(
            "database_path = \"D:/CellarData/.cellar/cellar.db\"",
            "database_path = \".cellar/cellar.db\"",
        );
        assert_eq!(
            Config::parse(&input).unwrap_err().code(),
            "database_path_must_be_absolute"
        );
    }

    #[test]
    fn rejects_database_path_outside_cellar_directory() {
        let input =
            valid_config().replace("D:/CellarData/.cellar/cellar.db", "D:/CellarData/cellar.db");
        assert_eq!(
            Config::parse(&input).unwrap_err().code(),
            "database_path_must_be_inside_cellar"
        );
    }

    #[test]
    fn rejects_database_path_equal_to_cellar_directory() {
        let input =
            valid_config().replace("D:/CellarData/.cellar/cellar.db", "D:/CellarData/.cellar");
        assert_eq!(
            Config::parse(&input).unwrap_err().code(),
            "database_path_must_be_inside_cellar"
        );
    }

    #[test]
    fn rejects_database_path_traversal() {
        let input = valid_config().replace(
            "D:/CellarData/.cellar/cellar.db",
            "D:/CellarData/.cellar/../cellar.db",
        );
        assert_eq!(
            Config::parse(&input).unwrap_err().code(),
            "database_path_contains_traversal"
        );
    }

    #[test]
    fn rejects_non_cloudflare_access_team_domain() {
        let input = valid_config().replace(
            "https://example.cloudflareaccess.com",
            "https://example.com",
        );
        assert_eq!(
            Config::parse(&input).unwrap_err().code(),
            "invalid_team_domain"
        );
    }

    #[test]
    fn rejects_noncanonical_team_domains() {
        for domain in [
            "http://example.cloudflareaccess.com",
            "https://example.cloudflareaccess.com/",
            "https://example.cloudflareaccess.com/path",
            "https://example.cloudflareaccess.com?query=yes",
            "https://example.cloudflareaccess.com#fragment",
            "https://EXAMPLE.cloudflareaccess.com",
            "https://example.cloudflareaccess.com:8443",
            "https://nested.team.cloudflareaccess.com",
        ] {
            let input = valid_config().replace("https://example.cloudflareaccess.com", domain);
            assert_eq!(
                Config::parse(&input).unwrap_err().code(),
                "invalid_team_domain",
                "team domain {domain:?} was accepted"
            );
        }
    }

    #[test]
    fn rejects_audience_with_surrounding_whitespace() {
        let input =
            valid_config().replace("replace-with-access-application-aud", " audience-value ");
        assert_eq!(
            Config::parse(&input).unwrap_err().code(),
            "invalid_access_audience"
        );
    }

    #[test]
    fn rejects_empty_audience() {
        let input = valid_config().replace("replace-with-access-application-aud", "");
        assert_eq!(
            Config::parse(&input).unwrap_err().code(),
            "invalid_access_audience"
        );
    }

    #[test]
    fn normalizes_owner_email() {
        let config =
            Config::parse(&valid_config().replace("owner@example.com", r"\t Owner@Example.COM \t"))
                .unwrap();
        assert_eq!(config.access().owner_email(), "owner@example.com");
    }

    #[test]
    fn preserves_non_ascii_owner_email_characters_while_lowercasing_ascii() {
        let config =
            Config::parse(&valid_config().replace("owner@example.com", "ÖWNER@ÉXAMPLE.COM"))
                .unwrap();

        assert_eq!(config.access().owner_email(), "Öwner@Éxample.com");
    }

    #[test]
    fn accepts_safe_owner_email_local_punctuation_and_domain_hyphens() {
        let config = Config::parse(
            &valid_config().replace("owner@example.com", "Owner+Cellar@my-domain.example"),
        )
        .unwrap();

        assert_eq!(
            config.access().owner_email(),
            "owner+cellar@my-domain.example"
        );
    }

    #[test]
    fn rejects_structurally_invalid_owner_email() {
        for email in ["", "owner.example.com", "@example.com", "owner@", "a@b@c"] {
            let input = valid_config().replace("owner@example.com", email);
            assert_eq!(
                Config::parse(&input).unwrap_err().code(),
                "invalid_owner_email",
                "email {email:?} was accepted"
            );
        }
    }

    #[test]
    fn rejects_owner_email_with_embedded_whitespace_or_control() {
        for email in ["own er@example.com", r"owner@exam\u0007ple.com"] {
            let input = valid_config().replace("owner@example.com", email);
            assert_eq!(
                Config::parse(&input).unwrap_err().code(),
                "invalid_owner_email",
                "email {email:?} was accepted"
            );
        }
    }

    #[test]
    fn loads_configuration_from_disk() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, valid_config()).unwrap();

        assert_eq!(Config::load(&path).unwrap().bind().port(), 8787);
    }

    #[test]
    fn load_errors_do_not_expose_file_contents() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "not valid toml: SUPER_SECRET_VALUE").unwrap();

        let error = Config::load(&path).unwrap_err().to_string();
        assert!(!error.contains("SUPER_SECRET_VALUE"));
        assert!(!format!("{:?}", Config::load(&path).unwrap_err()).contains("SUPER_SECRET_VALUE"));
    }
}
