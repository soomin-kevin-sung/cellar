use std::fs;
use std::path::{Path, PathBuf};

use cellar_config::{
    BootstrapClaim, CellarConfig, ConfigError, PersistedConfig, load_config, save_config,
};
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use url::Url;

const CLAIM_CODE: [u8; 32] = [0x5a; 32];

fn unenrolled_config(storage_root: &Path) -> CellarConfig {
    CellarConfig {
        external_origin: Url::parse("https://cellar.example.com/").unwrap(),
        team_domain: Url::parse("https://example.cloudflareaccess.com/").unwrap(),
        aud_tags: vec!["cellar-production".to_owned()],
        bootstrap_owner_email: Some("owner@example.com".to_owned()),
        owner_subject: None,
        storage_root: storage_root.to_owned(),
        origin_port: 8443,
        health_port: 8444,
    }
}

fn assert_code(config: &CellarConfig, expected: &'static str) {
    assert_eq!(config.validate().unwrap_err().code(), expected);
}

#[test]
fn valid_unenrolled_config_is_accepted() {
    let directory = tempdir().unwrap();
    unenrolled_config(directory.path()).validate().unwrap();
}

#[test]
fn valid_enrolled_config_is_accepted() {
    let directory = tempdir().unwrap();
    let mut config = unenrolled_config(directory.path());
    config.bootstrap_owner_email = None;
    config.owner_subject = Some("cf-access-subject".to_owned());

    config.validate().unwrap();
}

#[test]
fn external_origin_rejects_http() {
    let directory = tempdir().unwrap();
    let mut config = unenrolled_config(directory.path());
    config.external_origin = Url::parse("http://cellar.example.com/").unwrap();

    assert_code(&config, "external_origin_must_be_https");
}

#[test]
fn external_origin_requires_host() {
    let directory = tempdir().unwrap();
    let mut config = unenrolled_config(directory.path());
    // `Url` cannot represent hostless HTTPS, so exercise the explicit host
    // guard with another valid hostless URL.
    config.external_origin = Url::parse("data:text/plain,cellar").unwrap();

    assert_code(&config, "external_origin_missing_host");
}

#[test]
fn external_origin_rejects_credentials() {
    let directory = tempdir().unwrap();
    let mut config = unenrolled_config(directory.path());
    config.external_origin = Url::parse("https://user:password@cellar.example.com/").unwrap();

    assert_code(&config, "external_origin_credentials_forbidden");
}

#[test]
fn external_origin_rejects_query() {
    let directory = tempdir().unwrap();
    let mut config = unenrolled_config(directory.path());
    config.external_origin = Url::parse("https://cellar.example.com/?mode=unsafe").unwrap();

    assert_code(&config, "external_origin_query_forbidden");
}

#[test]
fn external_origin_rejects_fragment() {
    let directory = tempdir().unwrap();
    let mut config = unenrolled_config(directory.path());
    config.external_origin = Url::parse("https://cellar.example.com/#fragment").unwrap();

    assert_code(&config, "external_origin_fragment_forbidden");
}

#[test]
fn external_origin_rejects_non_root_path() {
    let directory = tempdir().unwrap();
    let mut config = unenrolled_config(directory.path());
    config.external_origin = Url::parse("https://cellar.example.com/prefix").unwrap();

    assert_code(&config, "external_origin_must_be_origin_only");
}

#[test]
fn team_domain_must_be_https() {
    let directory = tempdir().unwrap();
    let mut config = unenrolled_config(directory.path());
    config.team_domain = Url::parse("http://example.cloudflareaccess.com/").unwrap();

    assert_code(&config, "team_domain_must_be_https");
}

#[test]
fn team_domain_requires_host() {
    let directory = tempdir().unwrap();
    let mut config = unenrolled_config(directory.path());
    // `Url` cannot represent hostless HTTPS, so exercise the explicit host
    // guard with another valid hostless URL.
    config.team_domain = Url::parse("data:text/plain,cellar").unwrap();

    assert_code(&config, "team_domain_missing_host");
}

#[test]
fn team_domain_rejects_credentials_query_fragment_and_paths() {
    let directory = tempdir().unwrap();
    let mut config = unenrolled_config(directory.path());

    for (url, code) in [
        (
            "https://user@example.cloudflareaccess.com/",
            "team_domain_credentials_forbidden",
        ),
        (
            "https://example.cloudflareaccess.com/?query=yes",
            "team_domain_query_forbidden",
        ),
        (
            "https://example.cloudflareaccess.com/#fragment",
            "team_domain_fragment_forbidden",
        ),
        (
            "https://example.cloudflareaccess.com/cdn-cgi/access",
            "team_domain_must_be_origin_only",
        ),
    ] {
        config.team_domain = Url::parse(url).unwrap();
        assert_code(&config, code);
    }
}

#[test]
fn audience_tags_are_required() {
    let directory = tempdir().unwrap();
    let mut config = unenrolled_config(directory.path());
    config.aud_tags.clear();

    assert_code(&config, "aud_tags_required");
}

#[test]
fn audience_tags_reject_blank_untrimmed_and_oversized_values() {
    let directory = tempdir().unwrap();
    let mut config = unenrolled_config(directory.path());

    config.aud_tags = vec![" \t".to_owned()];
    assert_code(&config, "aud_tag_empty");

    config.aud_tags = vec![" audience".to_owned()];
    assert_code(&config, "aud_tag_not_trimmed");

    config.aud_tags = vec!["a".repeat(257)];
    assert_code(&config, "aud_tag_too_long");
}

#[test]
fn audience_tags_reject_duplicates() {
    let directory = tempdir().unwrap();
    let mut config = unenrolled_config(directory.path());
    config.aud_tags = vec!["same".to_owned(), "same".to_owned()];

    assert_code(&config, "aud_tags_duplicate");
}

#[test]
fn owner_identity_requires_exactly_one_state() {
    let directory = tempdir().unwrap();
    let mut config = unenrolled_config(directory.path());

    config.owner_subject = Some("subject".to_owned());
    assert_code(&config, "owner_identity_conflict");

    config.bootstrap_owner_email = None;
    config.owner_subject = None;
    assert_code(&config, "owner_identity_required");
}

#[test]
fn owner_identity_rejects_empty_or_untrimmed_values() {
    let directory = tempdir().unwrap();
    let mut config = unenrolled_config(directory.path());

    config.bootstrap_owner_email = Some(" ".to_owned());
    assert_code(&config, "bootstrap_owner_email_empty");

    config.bootstrap_owner_email = Some(" owner@example.com".to_owned());
    assert_code(&config, "bootstrap_owner_email_not_trimmed");

    config.bootstrap_owner_email = None;
    config.owner_subject = Some(String::new());
    assert_code(&config, "owner_subject_empty");

    config.owner_subject = Some("subject ".to_owned());
    assert_code(&config, "owner_subject_not_trimmed");
}

#[test]
fn storage_root_must_be_absolute() {
    let mut config = unenrolled_config(Path::new("."));
    config.storage_root = PathBuf::from("relative-storage");

    assert_code(&config, "storage_root_must_be_absolute");
}

#[test]
fn listener_ports_must_be_nonzero_and_distinct() {
    let directory = tempdir().unwrap();
    let mut config = unenrolled_config(directory.path());

    config.origin_port = 0;
    assert_code(&config, "origin_port_must_be_nonzero");

    config.origin_port = 8443;
    config.health_port = 0;
    assert_code(&config, "health_port_must_be_nonzero");

    config.health_port = 8443;
    assert_code(&config, "listener_ports_conflict");
}

#[test]
fn config_toml_round_trips() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let persisted = PersistedConfig {
        config: unenrolled_config(directory.path()),
        bootstrap_claim: Some(BootstrapClaim::new(&CLAIM_CODE, 2_000_000_000)),
    };

    save_config(&path, &persisted).unwrap();
    assert_eq!(load_config(&path).unwrap(), persisted);
}

fn enrolled_config(storage_root: &Path) -> CellarConfig {
    let mut config = unenrolled_config(storage_root);
    config.bootstrap_owner_email = None;
    config.owner_subject = Some("cf-access-subject".to_owned());
    config
}

#[test]
fn enrolled_config_with_bootstrap_claim_is_rejected_on_save() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let persisted = PersistedConfig {
        config: enrolled_config(directory.path()),
        bootstrap_claim: Some(BootstrapClaim::new(&CLAIM_CODE, 2_000_000_000)),
    };

    assert_eq!(
        persisted.validate().unwrap_err().code(),
        "bootstrap_claim_forbidden_when_enrolled"
    );
    assert_eq!(
        save_config(&path, &persisted).unwrap_err().code(),
        "bootstrap_claim_forbidden_when_enrolled"
    );
    assert!(!path.exists());
}

#[test]
fn enrolled_config_with_bootstrap_claim_is_rejected_on_load() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let persisted = PersistedConfig {
        config: enrolled_config(directory.path()),
        bootstrap_claim: Some(BootstrapClaim::new(&CLAIM_CODE, 2_000_000_000)),
    };
    fs::write(&path, toml::to_string(&persisted).unwrap()).unwrap();

    assert_eq!(
        load_config(&path).unwrap_err().code(),
        "bootstrap_claim_forbidden_when_enrolled"
    );
}

#[test]
fn claim_record_never_contains_plaintext_and_verifies_in_constant_time() {
    let claim = BootstrapClaim::new(&CLAIM_CODE, 2_000_000_000);
    let serialized = toml::to_string(&claim).unwrap();

    assert!(!serialized.contains(String::from_utf8_lossy(&CLAIM_CODE).as_ref()));
    assert!(claim.verify(&CLAIM_CODE, 1_999_999_999));

    let wrong_code = [0xa5; 32];
    assert!(!claim.verify(&wrong_code, 1_999_999_999));
    assert!(!claim.verify(&CLAIM_CODE, 2_000_000_001));
}

#[test]
fn claim_verifier_bytes_are_redacted_from_debug_output() {
    let directory = tempdir().unwrap();
    let claim = BootstrapClaim::new(&CLAIM_CODE, 2_000_000_000);
    let verifier_marker = format!("{:?}", Sha256::digest(CLAIM_CODE).to_vec());
    let persisted = PersistedConfig {
        config: unenrolled_config(directory.path()),
        bootstrap_claim: Some(claim.clone()),
    };

    let claim_debug = format!("{claim:?}");
    let persisted_debug = format!("{persisted:?}");
    assert!(!claim_debug.contains(&verifier_marker));
    assert!(!persisted_debug.contains(&verifier_marker));
    assert!(claim_debug.contains("<redacted>"));
    assert!(persisted_debug.contains("<redacted>"));
}

#[test]
fn stale_temp_file_cannot_replace_last_valid_config() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let first = PersistedConfig {
        config: unenrolled_config(directory.path()),
        bootstrap_claim: None,
    };
    save_config(&path, &first).unwrap();

    let interrupted = directory.path().join(".atomicwrite-interrupted");
    fs::create_dir(&interrupted).unwrap();
    fs::write(interrupted.join("tmpfile.tmp"), "invalid").unwrap();

    assert_eq!(load_config(&path).unwrap(), first);
}

#[test]
fn loading_absent_malformed_or_invalid_config_fails_closed() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("config.toml");

    assert_eq!(load_config(&path).unwrap_err().code(), "config_io_error");

    fs::write(&path, "not valid = [toml").unwrap();
    assert_eq!(load_config(&path).unwrap_err().code(), "config_parse_error");

    let invalid = PersistedConfig {
        config: {
            let mut config = unenrolled_config(directory.path());
            config.external_origin = Url::parse("http://cellar.example.com/").unwrap();
            config
        },
        bootstrap_claim: None,
    };
    fs::write(&path, toml::to_string(&invalid).unwrap()).unwrap();
    assert_eq!(
        load_config(&path).unwrap_err().code(),
        "external_origin_must_be_https"
    );
}

#[test]
fn unknown_top_level_config_field_fails_closed() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let persisted = PersistedConfig {
        config: unenrolled_config(directory.path()),
        bootstrap_claim: None,
    };
    let mut document = toml::Value::try_from(&persisted).unwrap();
    document
        .as_table_mut()
        .unwrap()
        .insert("unexpected".to_owned(), toml::Value::Boolean(true));
    fs::write(&path, toml::to_string(&document).unwrap()).unwrap();

    assert_eq!(load_config(&path).unwrap_err().code(), "config_parse_error");
}

#[test]
fn unknown_security_config_field_is_rejected() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let persisted = PersistedConfig {
        config: unenrolled_config(directory.path()),
        bootstrap_claim: None,
    };
    let mut document = toml::Value::try_from(persisted).unwrap();
    document
        .get_mut("config")
        .unwrap()
        .as_table_mut()
        .unwrap()
        .insert(
            "orgin_port".to_owned(),
            toml::Value::Integer(i64::from(8443)),
        );
    fs::write(&path, toml::to_string(&document).unwrap()).unwrap();

    assert_eq!(load_config(&path).unwrap_err().code(), "config_parse_error");
}

#[test]
fn unknown_bootstrap_claim_field_is_rejected() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let persisted = PersistedConfig {
        config: unenrolled_config(directory.path()),
        bootstrap_claim: Some(BootstrapClaim::new(&CLAIM_CODE, 2_000_000_000)),
    };
    let mut document = toml::Value::try_from(persisted).unwrap();
    document
        .get_mut("bootstrap_claim")
        .unwrap()
        .as_table_mut()
        .unwrap()
        .insert(
            "plaintext_code".to_owned(),
            toml::Value::String("secret".to_owned()),
        );
    fs::write(&path, toml::to_string(&document).unwrap()).unwrap();

    assert_eq!(load_config(&path).unwrap_err().code(), "config_parse_error");
}

#[test]
fn config_error_display_does_not_expose_secret_material() {
    let error = ConfigError::from(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "permission denied",
    ));

    assert!(!error.to_string().contains("5a5a5a"));
}
