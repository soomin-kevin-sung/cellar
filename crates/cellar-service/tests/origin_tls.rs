use std::fs;

use cellar_service::tls::{
    OriginTlsPaths, TlsError, TlsWarning, ensure_origin_tls, rotate_origin_ca,
};
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};
use x509_parser::{extensions::GeneralName, parse_x509_certificate, pem::parse_x509_pem};

// Elevated acceptance tests:
// Run from an elevated Administrator PowerShell (Administrators SID enabled) or a shell running
// as NT SERVICE\Cellar:
// cargo test -p cellar-service --test origin_tls -- --ignored --nocapture
//
// Phase 5 acceptance (not a local simulation): run elevated Windows VM power-loss tests at each
// journal boundary and verify recovery after reboot under both Administrator and NT SERVICE\Cellar.
// Also verify the real cloudflared caPool/route-fragment rollout during explicit CA rotation.

const CA_NAME: &str = "Cellar Local CA";
const ORIGIN_NAME: &str = "cellar.local";

fn paths(temp: &TempDir) -> OriginTlsPaths {
    OriginTlsPaths {
        ca_cert: temp.path().join("ca.pem"),
        ca_key: temp.path().join("ca-key.pem"),
        leaf_cert: temp.path().join("origin.pem"),
        leaf_key: temp.path().join("origin-key.pem"),
    }
}

fn parse_pem_certificate(path: &std::path::Path) -> Vec<u8> {
    let pem = fs::read(path).expect("certificate should be readable");
    let (_, pem) = parse_x509_pem(&pem).expect("certificate should be PEM");
    pem.contents
}

fn serial(der: &[u8]) -> Vec<u8> {
    let (_, certificate) = parse_x509_certificate(der).expect("valid X.509 certificate");
    certificate.raw_serial().to_vec()
}

#[test]
fn creates_a_ca_and_server_certificate_for_cellar_local() {
    let temp = TempDir::new().unwrap();
    let paths = paths(&temp);
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();

    let material = ensure_origin_tls(&paths, now).unwrap();

    assert_eq!(material.certificate_chain().len(), 2);
    assert_eq!(
        material.certificate_chain()[0].as_ref(),
        parse_pem_certificate(&paths.leaf_cert)
    );
    assert_eq!(
        material.certificate_chain()[1].as_ref(),
        parse_pem_certificate(&paths.ca_cert)
    );

    let (_, ca) = parse_x509_certificate(material.ca_certificate().as_ref()).unwrap();
    let (_, leaf) = parse_x509_certificate(material.certificate_chain()[0].as_ref()).unwrap();
    assert!(ca.is_ca());
    assert_eq!(
        ca.subject()
            .iter_common_name()
            .next()
            .unwrap()
            .as_str()
            .unwrap(),
        CA_NAME
    );
    assert_eq!(leaf.issuer(), ca.subject());
    leaf.verify_signature(Some(ca.public_key())).unwrap();

    let san = leaf.subject_alternative_name().unwrap().unwrap();
    assert!(
        san.value
            .general_names
            .iter()
            .any(|name| matches!(name, GeneralName::DNSName(name) if *name == ORIGIN_NAME))
    );
    assert!(
        leaf.extended_key_usage()
            .unwrap()
            .unwrap()
            .value
            .server_auth
    );

    let ca_lifetime = ca.validity().not_after.timestamp() - ca.validity().not_before.timestamp();
    let leaf_lifetime =
        leaf.validity().not_after.timestamp() - leaf.validity().not_before.timestamp();
    let day = Duration::DAY.whole_seconds();
    assert!((5 * 365 * day - day..=5 * 365 * day + day).contains(&ca_lifetime));
    assert!((365 * day - day..=365 * day + day).contains(&leaf_lifetime));
}

#[test]
#[cfg_attr(windows, ignore = "requires elevated token or Cellar service identity")]
fn reuses_material_before_the_renewal_window() {
    let temp = TempDir::new().unwrap();
    let paths = paths(&temp);
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();

    let first = ensure_origin_tls(&paths, now).unwrap();
    let second = ensure_origin_tls(&paths, now + Duration::days(300)).unwrap();

    assert_eq!(
        serial(first.ca_certificate().as_ref()),
        serial(second.ca_certificate().as_ref())
    );
    assert_eq!(
        serial(first.certificate_chain()[0].as_ref()),
        serial(second.certificate_chain()[0].as_ref())
    );
}

#[test]
#[cfg_attr(windows, ignore = "requires elevated token or Cellar service identity")]
fn renews_only_the_leaf_at_thirty_days_remaining() {
    let temp = TempDir::new().unwrap();
    let paths = paths(&temp);
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();

    let first = ensure_origin_tls(&paths, now).unwrap();
    let renewed = ensure_origin_tls(&paths, now + Duration::days(335)).unwrap();

    assert_eq!(
        serial(first.ca_certificate().as_ref()),
        serial(renewed.ca_certificate().as_ref())
    );
    assert_ne!(
        serial(first.certificate_chain()[0].as_ref()),
        serial(renewed.certificate_chain()[0].as_ref())
    );
}

#[test]
#[cfg_attr(windows, ignore = "requires elevated token or Cellar service identity")]
fn mismatched_existing_material_fails_closed_without_replacing_the_ca() {
    let first_dir = TempDir::new().unwrap();
    let second_dir = TempDir::new().unwrap();
    let first_paths = paths(&first_dir);
    let second_paths = paths(&second_dir);
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();

    let first = ensure_origin_tls(&first_paths, now).unwrap();
    ensure_origin_tls(&second_paths, now).unwrap();
    fs::copy(&second_paths.leaf_key, &first_paths.leaf_key).unwrap();

    let ca_before = fs::read(&first_paths.ca_cert).unwrap();
    let error = ensure_origin_tls(&first_paths, now).unwrap_err();

    assert!(matches!(error, TlsError::EstablishedBundleInvalid));
    assert_eq!(fs::read(&first_paths.ca_cert).unwrap(), ca_before);
    assert_eq!(
        serial(first.ca_certificate().as_ref()),
        serial(&parse_pem_certificate(&first_paths.ca_cert))
    );
}

#[test]
#[cfg_attr(windows, ignore = "requires elevated token or Cellar service identity")]
fn an_incomplete_established_bundle_fails_closed() {
    let temp = TempDir::new().unwrap();
    let paths = paths(&temp);
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    let first = ensure_origin_tls(&paths, now).unwrap();
    fs::remove_file(&paths.leaf_cert).unwrap();

    let ca_before = fs::read(&paths.ca_cert).unwrap();
    let error = ensure_origin_tls(&paths, now).unwrap_err();

    assert!(matches!(error, TlsError::EstablishedBundleInvalid));
    assert_eq!(fs::read(&paths.ca_cert).unwrap(), ca_before);
    assert_eq!(
        serial(first.ca_certificate().as_ref()),
        serial(&parse_pem_certificate(&paths.ca_cert))
    );
    assert!(!paths.leaf_cert.exists());
}

#[test]
fn debug_output_does_not_contain_private_key_pem() {
    let temp = TempDir::new().unwrap();
    let paths = paths(&temp);
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    let material = ensure_origin_tls(&paths, now).unwrap();
    let private_prefix = material.private_key().secret_der()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    let debug = format!("{material:?}");

    assert!(!debug.contains("PRIVATE KEY"));
    assert!(!debug.contains(&private_prefix));
    assert!(debug.contains("[redacted]"));
}

#[cfg(windows)]
#[test]
fn medium_integrity_process_cannot_rotate_the_origin_ca() {
    assert!(
        !cellar_windows::acl::is_elevated_administrator().unwrap(),
        "run the elevated rotation acceptance test separately from this medium-token denial test"
    );
    let temp = TempDir::new().unwrap();
    let paths = paths(&temp);
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();

    let error = rotate_origin_ca(&paths, now)
        .err()
        .expect("medium token must be denied");

    assert!(matches!(error, TlsError::AdministratorRequired));
    assert!(!paths.ca_cert.exists());
}

#[cfg(windows)]
#[test]
#[ignore = "requires elevated token or Cellar service identity"]
fn failed_renewal_keeps_the_last_complete_bundle() {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x0000_0001;

    let temp = TempDir::new().unwrap();
    let paths = paths(&temp);
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    ensure_origin_tls(&paths, now).unwrap();
    let before = [
        fs::read(&paths.ca_cert).unwrap(),
        fs::read(&paths.ca_key).unwrap(),
        fs::read(&paths.leaf_cert).unwrap(),
        fs::read(&paths.leaf_key).unwrap(),
    ];

    let _locked_key = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&paths.leaf_key)
        .unwrap();
    assert!(ensure_origin_tls(&paths, now + Duration::days(335)).is_err());

    let after = [
        fs::read(&paths.ca_cert).unwrap(),
        fs::read(&paths.ca_key).unwrap(),
        fs::read(&paths.leaf_cert).unwrap(),
        fs::read(&paths.leaf_key).unwrap(),
    ];
    assert_eq!(after, before);
}

#[test]
#[cfg_attr(windows, ignore = "requires elevated token or Cellar service identity")]
fn corrupt_pem_and_key_bytes_fail_closed_without_rotating_the_ca() {
    for corrupt_index in 0..4 {
        let temp = TempDir::new().unwrap();
        let paths = paths(&temp);
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let first = ensure_origin_tls(&paths, now).unwrap();
        let files = [
            &paths.ca_cert,
            &paths.ca_key,
            &paths.leaf_cert,
            &paths.leaf_key,
        ];
        fs::write(files[corrupt_index], b"corrupt PEM and key bytes").unwrap();

        let ca_before = fs::read(&paths.ca_cert).unwrap();
        let error = ensure_origin_tls(&paths, now).unwrap_err();

        assert!(matches!(error, TlsError::EstablishedBundleInvalid));
        if corrupt_index != 0 {
            assert_eq!(fs::read(&paths.ca_cert).unwrap(), ca_before);
            assert_eq!(
                serial(first.ca_certificate().as_ref()),
                serial(&parse_pem_certificate(&paths.ca_cert))
            );
        }
    }
}

#[test]
#[cfg_attr(windows, ignore = "requires elevated token or Cellar service identity")]
fn near_ca_expiry_preserves_the_trust_anchor_and_reports_rotation_required() {
    let temp = TempDir::new().unwrap();
    let paths = paths(&temp);
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    let first = ensure_origin_tls(&paths, now).unwrap();
    for day in [335, 670, 1005, 1340] {
        ensure_origin_tls(&paths, now + Duration::days(day)).unwrap();
    }
    let before = [
        fs::read(&paths.ca_cert).unwrap(),
        fs::read(&paths.ca_key).unwrap(),
        fs::read(&paths.leaf_cert).unwrap(),
        fs::read(&paths.leaf_key).unwrap(),
    ];

    let material = ensure_origin_tls(&paths, now + Duration::days(1675)).unwrap();

    assert_eq!(material.warnings(), &[TlsWarning::CaRotationRequired]);
    assert_eq!(
        serial(first.ca_certificate()),
        serial(material.ca_certificate())
    );
    assert_eq!(
        before,
        [
            fs::read(&paths.ca_cert).unwrap(),
            fs::read(&paths.ca_key).unwrap(),
            fs::read(&paths.leaf_cert).unwrap(),
            fs::read(&paths.leaf_key).unwrap(),
        ]
    );
}

#[test]
#[cfg(windows)]
#[ignore = "requires an elevated Administrator token"]
fn explicit_admin_rotation_changes_ca_and_signals_cloudflared_update() {
    let temp = TempDir::new().unwrap();
    let paths = paths(&temp);
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    let first = ensure_origin_tls(&paths, now).unwrap();

    let rotated = rotate_origin_ca(&paths, now + Duration::days(1)).unwrap();

    assert_ne!(
        serial(first.ca_certificate()),
        serial(rotated.material().ca_certificate())
    );
    assert!(rotated.cloudflared_ca_pool_and_route_update_required());
}
