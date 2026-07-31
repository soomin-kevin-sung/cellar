use std::{
    array,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use cellar_windows::acl::{
    AclError, PRIVATE_KEY_SERVICE_NAME, is_elevated_administrator, restrict_private_key_handle,
};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, PublicKeyData,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use thiserror::Error;
use time::{Duration, OffsetDateTime};
use x509_parser::{
    certificate::X509Certificate, extensions::GeneralName, parse_x509_certificate,
    pem::parse_x509_pem,
};
use zeroize::{Zeroize, Zeroizing};

const CA_COMMON_NAME: &str = "Cellar Local CA";
const ORIGIN_DNS_NAME: &str = "cellar.local";
const BACKDATE: Duration = Duration::minutes(5);
const CA_LIFETIME: Duration = Duration::days(5 * 365);
const LEAF_LIFETIME: Duration = Duration::days(365);
const RENEWAL_WINDOW: Duration = Duration::days(30);
const TRANSACTION_MARKER: &str = ".cellar-origin-tls.transaction";
const OPERATION_LOCK: &str = ".cellar-origin-tls.lock";
const ESTABLISHMENT_MARKER: &str = ".cellar-origin-tls.established";
const ESTABLISHMENT_MARKER_CONTENT: &[u8] = b"cellar-origin-tls-established:v1\n";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OriginTlsPaths {
    pub ca_cert: PathBuf,
    pub ca_key: PathBuf,
    pub leaf_cert: PathBuf,
    pub leaf_key: PathBuf,
}

pub struct TlsMaterial {
    certificate_chain: Vec<CertificateDer<'static>>,
    private_key: SecretPrivateKey,
    ca_certificate: CertificateDer<'static>,
    warnings: Vec<TlsWarning>,
}

struct SecretPrivateKey(PrivateKeyDer<'static>);

impl Drop for SecretPrivateKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlsWarning {
    CaRotationRequired,
}

pub struct OriginCaRotation {
    material: TlsMaterial,
}

impl std::fmt::Debug for OriginCaRotation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OriginCaRotation")
            .field("material", &self.material)
            .field("cloudflared_ca_pool_and_route_update_required", &true)
            .finish()
    }
}

impl OriginCaRotation {
    pub fn material(&self) -> &TlsMaterial {
        &self.material
    }

    /// The caller must update cloudflared's `caPool` and route fragment before
    /// treating the new trust anchor as deployed.
    pub fn cloudflared_ca_pool_and_route_update_required(&self) -> bool {
        true
    }
}

impl std::fmt::Debug for TlsMaterial {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TlsMaterial")
            .field("certificate_chain_len", &self.certificate_chain.len())
            .field("private_key", &"[redacted]")
            .field("ca_certificate_len", &self.ca_certificate.as_ref().len())
            .finish()
    }
}

impl TlsMaterial {
    pub fn certificate_chain(&self) -> &[CertificateDer<'static>] {
        &self.certificate_chain
    }

    pub fn private_key(&self) -> &PrivateKeyDer<'static> {
        &self.private_key.0
    }

    pub fn ca_certificate(&self) -> &CertificateDer<'static> {
        &self.ca_certificate
    }

    pub fn warnings(&self) -> &[TlsWarning] {
        &self.warnings
    }
}

#[derive(Debug, Error)]
pub enum TlsError {
    #[error("all origin TLS files must have the same parent directory")]
    DifferentDirectories,
    #[error("origin TLS path {path} has no parent directory")]
    MissingParent { path: PathBuf },
    #[error("could not {operation} origin TLS file {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not generate origin TLS material: {0}")]
    Generation(#[from] rcgen::Error),
    #[error("could not secure origin TLS private key: {0}")]
    PrivateKeyAcl(#[from] AclError),
    #[error("origin CA rotation requires an elevated Windows Administrator token")]
    AdministratorRequired,
    #[error("could not verify Administrator authorization for origin CA rotation: {0}")]
    AdministratorCheck(AclError),
    #[error("could not recover the previous origin TLS transaction: {0}")]
    Recovery(String),
    #[error("could not preserve the previous origin TLS bundle after a write failure: {0}")]
    Rollback(String),
    #[error("the established origin TLS bundle is incomplete, unreadable, or invalid")]
    EstablishedBundleInvalid,
    #[error("origin TLS was previously established but all certificate and key files are missing")]
    EstablishedMaterialMissing,
    #[error("the origin TLS establishment marker is invalid")]
    EstablishmentStateInvalid,
    #[error(
        "origin CA rotation committed and cloudflared must be updated, but establishment-state publication failed: {marker_error}"
    )]
    RotationCommitted {
        rotation: Box<OriginCaRotation>,
        marker_error: String,
    },
    #[cfg(test)]
    #[error("injected crash after {0}")]
    InjectedCrash(&'static str),
}

struct Bundle {
    ca_cert_pem: Vec<u8>,
    ca_key_pem: Zeroizing<Vec<u8>>,
    leaf_cert_pem: Vec<u8>,
    leaf_key_pem: Zeroizing<Vec<u8>>,
    ca_cert_der: Vec<u8>,
    leaf_cert_der: Vec<u8>,
    leaf_key_der: Zeroizing<Vec<u8>>,
}

struct ExistingBundle {
    bundle: Bundle,
    ca_key: Zeroizing<KeyPair>,
    leaf_not_after: OffsetDateTime,
    ca_not_after: OffsetDateTime,
}

enum ExistingState {
    Valid(Box<ExistingBundle>),
    Uninitialized,
}

pub fn ensure_origin_tls(
    paths: &OriginTlsPaths,
    now: OffsetDateTime,
) -> Result<TlsMaterial, TlsError> {
    ensure_origin_tls_impl(paths, now, true)
}

/// Replaces the origin trust anchor. This operation is intentionally separate
/// from normal startup and must only be exposed through an administrator-only
/// control path.
pub fn rotate_origin_ca(
    paths: &OriginTlsPaths,
    now: OffsetDateTime,
) -> Result<OriginCaRotation, TlsError> {
    if !is_elevated_administrator().map_err(TlsError::AdministratorCheck)? {
        return Err(TlsError::AdministratorRequired);
    }
    rotate_origin_ca_impl(paths, now, true)
}

impl TlsError {
    /// Returns the committed rotation when trust changed even though publishing
    /// establishment state failed. Callers must still update cloudflared.
    pub fn committed_rotation(&self) -> Option<&OriginCaRotation> {
        match self {
            Self::RotationCommitted { rotation, .. } => Some(rotation),
            _ => None,
        }
    }
}

fn ensure_origin_tls_impl(
    paths: &OriginTlsPaths,
    now: OffsetDateTime,
    protect_private_keys: bool,
) -> Result<TlsMaterial, TlsError> {
    let directory = common_directory(paths)?;
    fs::create_dir_all(directory)
        .map_err(|source| io_error("create directory", directory, source))?;
    let _lock = acquire_operation_lock(directory, protect_private_keys)?;
    recover_transaction(paths, directory)?;
    let established = establishment_is_recorded(directory, protect_private_keys)?;

    match load_existing(paths, now, established)? {
        ExistingState::Valid(existing) => {
            if existing.leaf_not_after - now > RENEWAL_WINDOW {
                if protect_private_keys {
                    secure_existing_keys(paths)?;
                }
                ensure_establishment_marker(directory, established, protect_private_keys)?;
                let warning = (existing.ca_not_after < now + LEAF_LIFETIME)
                    .then_some(TlsWarning::CaRotationRequired);
                return Ok(existing.bundle.into_material(warning));
            }

            if existing.ca_not_after >= now + LEAF_LIFETIME {
                let renewed = renew_leaf(*existing, now)?;
                persist_bundle(paths, directory, &renewed, protect_private_keys)?;
                ensure_establishment_marker(directory, established, protect_private_keys)?;
                return Ok(renewed.into_material(None));
            }
            ensure_establishment_marker(directory, established, protect_private_keys)?;
            return Ok(existing
                .bundle
                .into_material(Some(TlsWarning::CaRotationRequired)));
        }
        ExistingState::Uninitialized => {}
    }

    let generated = generate_bundle(now)?;
    persist_bundle(paths, directory, &generated, protect_private_keys)?;
    ensure_establishment_marker(directory, established, protect_private_keys)?;
    Ok(generated.into_material(None))
}

fn rotate_origin_ca_impl(
    paths: &OriginTlsPaths,
    now: OffsetDateTime,
    protect_private_keys: bool,
) -> Result<OriginCaRotation, TlsError> {
    let directory = common_directory(paths)?;
    fs::create_dir_all(directory)
        .map_err(|source| io_error("create directory", directory, source))?;
    let _lock = acquire_operation_lock(directory, protect_private_keys)?;
    recover_transaction(paths, directory)?;
    let established = establishment_is_recorded(directory, protect_private_keys)?;
    let generated = generate_bundle(now)?;
    persist_bundle(paths, directory, &generated, protect_private_keys)?;
    let rotation = OriginCaRotation {
        material: generated.into_material(None),
    };
    if let Err(error) = ensure_establishment_marker(directory, established, protect_private_keys) {
        return Err(TlsError::RotationCommitted {
            rotation: Box::new(rotation),
            marker_error: error.to_string(),
        });
    }
    Ok(rotation)
}

#[cfg(test)]
fn ensure_origin_tls_for_test(
    paths: &OriginTlsPaths,
    now: OffsetDateTime,
) -> Result<TlsMaterial, TlsError> {
    ensure_origin_tls_impl(paths, now, false)
}

fn common_directory(paths: &OriginTlsPaths) -> Result<&Path, TlsError> {
    let all = [
        &paths.ca_cert,
        &paths.ca_key,
        &paths.leaf_cert,
        &paths.leaf_key,
    ];
    let parent = all[0].parent().ok_or_else(|| TlsError::MissingParent {
        path: all[0].clone(),
    })?;
    for path in &all[1..] {
        let candidate = path.parent().ok_or_else(|| TlsError::MissingParent {
            path: (*path).clone(),
        })?;
        if candidate != parent {
            return Err(TlsError::DifferentDirectories);
        }
    }
    Ok(parent)
}

struct OperationLock(File);

impl Drop for OperationLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn acquire_operation_lock(directory: &Path, protect: bool) -> Result<OperationLock, TlsError> {
    let path = directory.join(OPERATION_LOCK);

    #[cfg(windows)]
    let file = if protect {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::{
            Foundation::{GENERIC_READ, GENERIC_WRITE},
            Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL, WRITE_DAC},
        };

        loop {
            let mut create = OpenOptions::new();
            create
                .read(true)
                .write(true)
                .create_new(true)
                .access_mode(GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
            match create.open(&path) {
                Ok(file) => break file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let mut existing = OpenOptions::new();
                    existing
                        .read(true)
                        .access_mode(GENERIC_READ | READ_CONTROL | WRITE_DAC)
                        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
                    match existing.open(&path) {
                        Ok(file) => break file,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(source) => return Err(io_error("open operation lock", &path, source)),
                    }
                }
                Err(source) => return Err(io_error("create operation lock", &path, source)),
            }
        }
    } else {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| io_error("open operation lock", &path, source))?
    };

    #[cfg(not(windows))]
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|source| io_error("open operation lock", &path, source))?;

    if protect {
        #[cfg(windows)]
        restrict_private_key_handle(&file, PRIVATE_KEY_SERVICE_NAME)?;
        #[cfg(not(windows))]
        set_owner_only_permissions(&path)?;
    }
    file.lock()
        .map_err(|source| io_error("acquire operation lock", &path, source))?;
    Ok(OperationLock(file))
}

fn secure_existing_keys(paths: &OriginTlsPaths) -> Result<(), TlsError> {
    #[cfg(windows)]
    {
        for path in [&paths.ca_key, &paths.leaf_key] {
            secure_existing_protected_file(path)?;
        }
    }
    #[cfg(not(windows))]
    {
        let _ = paths;
    }
    Ok(())
}

#[cfg(windows)]
fn secure_existing_protected_file(path: &Path) -> Result<(), TlsError> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL, WRITE_DAC,
    };

    let mut options = OpenOptions::new();
    options
        .read(true)
        .access_mode(READ_CONTROL | WRITE_DAC)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    let file = options
        .open(path)
        .map_err(|source| io_error("open protected TLS state for ACL", path, source))?;
    restrict_private_key_handle(&file, PRIVATE_KEY_SERVICE_NAME)?;
    Ok(())
}

fn establishment_is_recorded(directory: &Path, protect: bool) -> Result<bool, TlsError> {
    let marker = directory.join(ESTABLISHMENT_MARKER);
    if !transaction_path_exists(&marker)? {
        return Ok(false);
    }
    if !transaction_path_is_file(&marker)? {
        return Err(TlsError::EstablishmentStateInvalid);
    }
    let contents = fs::read(&marker)
        .map_err(|source| io_error("read TLS establishment marker", &marker, source))?;
    if contents != ESTABLISHMENT_MARKER_CONTENT {
        return Err(TlsError::EstablishmentStateInvalid);
    }
    if protect {
        #[cfg(windows)]
        secure_existing_protected_file(&marker)?;
        #[cfg(not(windows))]
        set_owner_only_permissions(&marker)?;
    }
    Ok(true)
}

fn ensure_establishment_marker(
    directory: &Path,
    established: bool,
    protect: bool,
) -> Result<(), TlsError> {
    if established {
        return Ok(());
    }
    let marker = directory.join(ESTABLISHMENT_MARKER);
    let stage = sibling_path(&marker, "stage");
    remove_if_exists(&stage)?;
    let staged = write_staged_file(
        &stage,
        ESTABLISHMENT_MARKER_CONTENT,
        protect,
        PersistenceFault::None,
    )?;
    rename_staged_file(&staged, &marker)
}

fn load_existing(
    paths: &OriginTlsPaths,
    now: OffsetDateTime,
    established: bool,
) -> Result<ExistingState, TlsError> {
    let present = [
        file_is_present(&paths.ca_cert)?,
        file_is_present(&paths.ca_key)?,
        file_is_present(&paths.leaf_cert)?,
        file_is_present(&paths.leaf_key)?,
    ];
    if present.iter().all(|exists| !exists) {
        if established {
            return Err(TlsError::EstablishedMaterialMissing);
        }
        return Ok(ExistingState::Uninitialized);
    }
    load_and_validate(paths, now)
        .map(|bundle| ExistingState::Valid(Box::new(bundle)))
        .map_err(|()| TlsError::EstablishedBundleInvalid)
}

fn file_is_present(path: &Path) -> Result<bool, TlsError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(TlsError::EstablishedBundleInvalid),
    }
}

fn load_and_validate(paths: &OriginTlsPaths, now: OffsetDateTime) -> Result<ExistingBundle, ()> {
    let ca_cert_pem = fs::read(&paths.ca_cert).map_err(|_| ())?;
    let ca_key_pem = Zeroizing::new(fs::read(&paths.ca_key).map_err(|_| ())?);
    let leaf_cert_pem = fs::read(&paths.leaf_cert).map_err(|_| ())?;
    let leaf_key_pem = Zeroizing::new(fs::read(&paths.leaf_key).map_err(|_| ())?);
    let ca_cert_der = certificate_der_from_pem(&ca_cert_pem)?;
    let leaf_cert_der = certificate_der_from_pem(&leaf_cert_pem)?;
    let ca_key_text = std::str::from_utf8(&ca_key_pem).map_err(|_| ())?;
    let leaf_key_text = std::str::from_utf8(&leaf_key_pem).map_err(|_| ())?;
    let ca_key = Zeroizing::new(KeyPair::from_pem(ca_key_text).map_err(|_| ())?);
    let leaf_key = Zeroizing::new(KeyPair::from_pem(leaf_key_text).map_err(|_| ())?);

    let (ca_remainder, ca) = parse_x509_certificate(&ca_cert_der).map_err(|_| ())?;
    let (leaf_remainder, leaf) = parse_x509_certificate(&leaf_cert_der).map_err(|_| ())?;
    if !ca_remainder.is_empty() || !leaf_remainder.is_empty() {
        return Err(());
    }
    validate_ca(&ca, &ca_key, now)?;
    validate_leaf(&leaf, &leaf_key, &ca, now)?;

    let leaf_not_after = OffsetDateTime::from_unix_timestamp(leaf.validity().not_after.timestamp())
        .map_err(|_| ())?;
    let ca_not_after =
        OffsetDateTime::from_unix_timestamp(ca.validity().not_after.timestamp()).map_err(|_| ())?;
    let leaf_key_der = Zeroizing::new(leaf_key.serialize_der());
    Ok(ExistingBundle {
        bundle: Bundle {
            ca_cert_pem,
            ca_key_pem,
            leaf_cert_pem,
            leaf_key_pem,
            ca_cert_der,
            leaf_cert_der,
            leaf_key_der,
        },
        ca_key,
        leaf_not_after,
        ca_not_after,
    })
}

fn certificate_der_from_pem(pem: &[u8]) -> Result<Vec<u8>, ()> {
    let (remainder, parsed) = parse_x509_pem(pem).map_err(|_| ())?;
    if !remainder.iter().all(u8::is_ascii_whitespace) {
        return Err(());
    }
    Ok(parsed.contents)
}

fn validate_ca(ca: &X509Certificate<'_>, key: &KeyPair, now: OffsetDateTime) -> Result<(), ()> {
    if !ca.is_ca()
        || common_name(ca) != Some(CA_COMMON_NAME)
        || ca.issuer() != ca.subject()
        || ca.public_key().raw != key.subject_public_key_info()
        || !valid_at(ca, now)
        || certificate_lifetime(ca) != CA_LIFETIME.whole_seconds()
    {
        return Err(());
    }
    let key_usage = ca.key_usage().map_err(|_| ())?.ok_or(())?;
    if !key_usage.value.key_cert_sign() {
        return Err(());
    }
    ca.verify_signature(None).map_err(|_| ())
}

fn validate_leaf(
    leaf: &X509Certificate<'_>,
    key: &KeyPair,
    ca: &X509Certificate<'_>,
    now: OffsetDateTime,
) -> Result<(), ()> {
    if leaf.is_ca()
        || common_name(leaf) != Some(ORIGIN_DNS_NAME)
        || leaf.public_key().raw != key.subject_public_key_info()
        || leaf.issuer() != ca.subject()
        || !valid_at(leaf, now)
        || leaf.validity().not_after > ca.validity().not_after
        || certificate_lifetime(leaf) != LEAF_LIFETIME.whole_seconds()
    {
        return Err(());
    }
    let san = leaf.subject_alternative_name().map_err(|_| ())?.ok_or(())?;
    if !matches!(
        san.value.general_names.as_slice(),
        [GeneralName::DNSName(name)] if *name == ORIGIN_DNS_NAME
    ) {
        return Err(());
    }
    let key_usage = leaf.key_usage().map_err(|_| ())?.ok_or(())?;
    if !key_usage.value.digital_signature() {
        return Err(());
    }
    let usages = leaf.extended_key_usage().map_err(|_| ())?.ok_or(())?;
    if !usages.value.server_auth {
        return Err(());
    }
    leaf.verify_signature(Some(ca.public_key())).map_err(|_| ())
}

fn common_name<'a>(certificate: &'a X509Certificate<'a>) -> Option<&'a str> {
    certificate
        .subject()
        .iter_common_name()
        .next()?
        .as_str()
        .ok()
}

fn valid_at(certificate: &X509Certificate<'_>, now: OffsetDateTime) -> bool {
    let timestamp = now.unix_timestamp();
    certificate.validity().not_before.timestamp() <= timestamp
        && timestamp <= certificate.validity().not_after.timestamp()
}

fn certificate_lifetime(certificate: &X509Certificate<'_>) -> i64 {
    certificate.validity().not_after.timestamp() - certificate.validity().not_before.timestamp()
}

fn generate_bundle(now: OffsetDateTime) -> Result<Bundle, TlsError> {
    let ca_key = Zeroizing::new(KeyPair::generate()?);
    let leaf_key = Zeroizing::new(KeyPair::generate()?);
    let ca_params = ca_parameters(now);
    let ca_cert = ca_params.self_signed(&*ca_key)?;
    let issuer = Issuer::from_params(&ca_params, &*ca_key);
    let leaf_cert = leaf_parameters(now).signed_by(&*leaf_key, &issuer)?;

    Ok(Bundle {
        ca_cert_pem: ca_cert.pem().into_bytes(),
        ca_key_pem: Zeroizing::new(ca_key.serialize_pem().into_bytes()),
        leaf_cert_pem: leaf_cert.pem().into_bytes(),
        leaf_key_pem: Zeroizing::new(leaf_key.serialize_pem().into_bytes()),
        ca_cert_der: ca_cert.der().to_vec(),
        leaf_cert_der: leaf_cert.der().to_vec(),
        leaf_key_der: Zeroizing::new(leaf_key.serialize_der()),
    })
}

fn renew_leaf(existing: ExistingBundle, now: OffsetDateTime) -> Result<Bundle, TlsError> {
    let leaf_key = Zeroizing::new(KeyPair::generate()?);
    let ca_pem = std::str::from_utf8(&existing.bundle.ca_cert_pem)
        .map_err(|_| TlsError::Recovery("validated CA PEM was not UTF-8".to_owned()))?;
    let issuer = Issuer::from_ca_cert_pem(ca_pem, &*existing.ca_key)?;
    let leaf_cert = leaf_parameters(now).signed_by(&*leaf_key, &issuer)?;
    Ok(Bundle {
        ca_cert_pem: existing.bundle.ca_cert_pem,
        ca_key_pem: existing.bundle.ca_key_pem,
        leaf_cert_pem: leaf_cert.pem().into_bytes(),
        leaf_key_pem: Zeroizing::new(leaf_key.serialize_pem().into_bytes()),
        ca_cert_der: existing.bundle.ca_cert_der,
        leaf_cert_der: leaf_cert.der().to_vec(),
        leaf_key_der: Zeroizing::new(leaf_key.serialize_der()),
    })
}

fn ca_parameters(now: OffsetDateTime) -> CertificateParams {
    let mut distinguished_name = DistinguishedName::new();
    distinguished_name.push(DnType::CommonName, CA_COMMON_NAME);
    let mut parameters = CertificateParams::default();
    parameters.not_before = now - BACKDATE;
    parameters.not_after = now + CA_LIFETIME - BACKDATE;
    parameters.distinguished_name = distinguished_name;
    parameters.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    parameters.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    parameters
}

fn leaf_parameters(now: OffsetDateTime) -> CertificateParams {
    let mut parameters =
        CertificateParams::new(vec![ORIGIN_DNS_NAME.to_owned()]).expect("static DNS name is valid");
    let mut distinguished_name = DistinguishedName::new();
    distinguished_name.push(DnType::CommonName, ORIGIN_DNS_NAME);
    parameters.not_before = now - BACKDATE;
    parameters.not_after = now + LEAF_LIFETIME - BACKDATE;
    parameters.distinguished_name = distinguished_name;
    parameters.is_ca = IsCa::ExplicitNoCa;
    parameters.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    parameters.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    parameters
}

impl Bundle {
    fn into_material(mut self, warning: Option<TlsWarning>) -> TlsMaterial {
        let ca_certificate = CertificateDer::from(self.ca_cert_der);
        let leaf_certificate = CertificateDer::from(self.leaf_cert_der);
        let leaf_key_der = std::mem::take(&mut *self.leaf_key_der);
        TlsMaterial {
            certificate_chain: vec![leaf_certificate, ca_certificate.clone()],
            private_key: SecretPrivateKey(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                leaf_key_der,
            ))),
            ca_certificate,
            warnings: warning.into_iter().collect(),
        }
    }
}

struct TransactionFile<'a> {
    target: &'a Path,
    contents: &'a [u8],
    private: bool,
}

struct StagedFile {
    #[cfg(not(windows))]
    path: PathBuf,
    file: File,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PersistenceFault {
    None,
    #[cfg(test)]
    PrivateKeyAcl,
    #[cfg(test)]
    CrashAfterPreparedMarker,
    #[cfg(test)]
    CrashAfterBackup(usize),
    #[cfg(test)]
    CrashAfterReplacement(usize),
    #[cfg(test)]
    MarkerReplacement(TransactionState),
    #[cfg(test)]
    RollbackStep(usize),
    #[cfg(test)]
    Cleanup,
}

#[cfg(test)]
type TestPersistenceFault = PersistenceFault;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransactionState {
    Prepared,
    Committed,
}

fn persist_bundle(
    paths: &OriginTlsPaths,
    directory: &Path,
    bundle: &Bundle,
    protect_private_keys: bool,
) -> Result<(), TlsError> {
    persist_bundle_impl(
        paths,
        directory,
        bundle,
        protect_private_keys,
        PersistenceFault::None,
    )
}

fn persist_bundle_impl(
    paths: &OriginTlsPaths,
    directory: &Path,
    bundle: &Bundle,
    protect_private_keys: bool,
    fault: PersistenceFault,
) -> Result<(), TlsError> {
    let files = [
        TransactionFile {
            target: &paths.ca_cert,
            contents: &bundle.ca_cert_pem,
            private: false,
        },
        TransactionFile {
            target: &paths.ca_key,
            contents: &bundle.ca_key_pem,
            private: true,
        },
        TransactionFile {
            target: &paths.leaf_cert,
            contents: &bundle.leaf_cert_pem,
            private: false,
        },
        TransactionFile {
            target: &paths.leaf_key,
            contents: &bundle.leaf_key_pem,
            private: true,
        },
    ];
    let stages: [PathBuf; 4] = array::from_fn(|index| sibling_path(files[index].target, "stage"));
    let backups: [PathBuf; 4] = array::from_fn(|index| sibling_path(files[index].target, "backup"));
    let marker = directory.join(TRANSACTION_MARKER);

    let mut staged_files = Vec::with_capacity(files.len());
    for stage in &stages {
        best_effort_remove(stage);
    }
    for backup in &backups {
        remove_if_exists(backup)?;
    }
    for (entry, stage) in files.iter().zip(&stages) {
        match write_staged_file(
            stage,
            entry.contents,
            entry.private && protect_private_keys,
            fault,
        ) {
            Ok(staged) => staged_files.push(staged),
            Err(error) => {
                drop(staged_files);
                cleanup_stages(&stages, &marker);
                return Err(error);
            }
        }
    }

    let mut original_mask = 0_u8;
    for (index, entry) in files.iter().enumerate() {
        match transaction_path_exists(entry.target) {
            Ok(true) => original_mask |= 1 << index,
            Ok(false) => {}
            Err(error) => {
                drop(staged_files);
                cleanup_stages(&stages, &marker);
                return Err(error);
            }
        }
    }
    if let Err(error) = publish_marker(&marker, original_mask, TransactionState::Prepared, fault) {
        drop(staged_files);
        cleanup_stages(&stages, &marker);
        return Err(error);
    }
    #[cfg(test)]
    if fault == PersistenceFault::CrashAfterPreparedMarker {
        return Err(TlsError::InjectedCrash("durable prepared marker"));
    }

    let commit_result = (|| {
        for (index, ((entry, staged), backup)) in
            files.iter().zip(&staged_files).zip(&backups).enumerate()
        {
            #[cfg(not(test))]
            let _ = index;
            if transaction_path_exists(entry.target)? {
                durable_rename_path(entry.target, backup, false)
                    .map_err(|source| io_error("back up", entry.target, source))?;
                #[cfg(test)]
                if fault == PersistenceFault::CrashAfterBackup(index) {
                    return Err(TlsError::InjectedCrash("target-to-backup boundary"));
                }
            }
            rename_staged_file(staged, entry.target)?;
            #[cfg(test)]
            if fault == PersistenceFault::CrashAfterReplacement(index) {
                return Err(TlsError::InjectedCrash("target replacement boundary"));
            }
            #[cfg(test)]
            if matches!(fault, PersistenceFault::RollbackStep(_)) && index == 2 {
                return Err(io_error(
                    "injected commit failure",
                    entry.target,
                    std::io::Error::other("injected commit failure before rollback"),
                ));
            }
        }
        publish_marker(&marker, original_mask, TransactionState::Committed, fault)
    })();

    if let Err(commit_error) = commit_result {
        #[cfg(test)]
        if matches!(commit_error, TlsError::InjectedCrash(_)) {
            return Err(commit_error);
        }
        if let Err(rollback_error) = rollback_files_impl(
            &files,
            &stages,
            &backups,
            original_mask,
            &marker,
            directory,
            fault,
        ) {
            return Err(TlsError::Rollback(format!(
                "{rollback_error}; original write error: {commit_error}"
            )));
        }
        return Err(commit_error);
    }

    #[cfg(test)]
    if fault == PersistenceFault::Cleanup {
        return Ok(());
    }
    cleanup_after_commit(&stages, &backups, &marker);
    Ok(())
}

#[cfg(test)]
fn persist_bundle_for_test(
    paths: &OriginTlsPaths,
    directory: &Path,
    bundle: &Bundle,
    protect_private_keys: bool,
    fault: TestPersistenceFault,
) -> Result<(), TlsError> {
    persist_bundle_impl(paths, directory, bundle, protect_private_keys, fault)
}

fn write_staged_file(
    path: &Path,
    contents: &[u8],
    private: bool,
    fault: PersistenceFault,
) -> Result<StagedFile, TlsError> {
    #[cfg(not(test))]
    let _ = fault;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::{
            Foundation::GENERIC_WRITE,
            Storage::FileSystem::{
                DELETE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
                WRITE_DAC,
            },
        };

        options
            .access_mode(GENERIC_WRITE | DELETE | READ_CONTROL | WRITE_DAC)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    }
    let mut file = options
        .open(path)
        .map_err(|source| io_error("create temporary", path, source))?;
    if private {
        #[cfg(test)]
        if fault == PersistenceFault::PrivateKeyAcl {
            return Err(TlsError::PrivateKeyAcl(AclError::Descriptor(
                std::io::Error::other("injected private-key ACL failure"),
            )));
        }
        #[cfg(windows)]
        restrict_private_key_handle(&file, PRIVATE_KEY_SERVICE_NAME)?;
        #[cfg(not(windows))]
        set_owner_only_permissions(path)?;
    }
    file.write_all(contents)
        .map_err(|source| io_error("write temporary", path, source))?;
    file.flush()
        .map_err(|source| io_error("flush temporary", path, source))?;
    file.sync_all()
        .map_err(|source| io_error("sync temporary", path, source))?;
    Ok(StagedFile {
        #[cfg(not(windows))]
        path: path.to_path_buf(),
        file,
    })
}

#[cfg(windows)]
fn rename_staged_file(staged: &StagedFile, target: &Path) -> Result<(), TlsError> {
    use std::{
        mem::{offset_of, size_of},
        os::windows::{ffi::OsStrExt, io::AsRawHandle},
        ptr,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_RENAME_INFO, FileRenameInfo, SetFileInformationByHandle,
    };

    let parent = target.parent().ok_or_else(|| TlsError::MissingParent {
        path: target.to_path_buf(),
    })?;
    let absolute_parent = parent
        .canonicalize()
        .map_err(|source| io_error("resolve target directory", parent, source))?;
    let absolute_target = absolute_parent.join(target.file_name().unwrap_or_default());
    let name: Vec<u16> = absolute_target.as_os_str().encode_wide().collect();
    let name_offset = offset_of!(FILE_RENAME_INFO, FileName);
    let byte_len = name_offset + name.len() * size_of::<u16>();
    let words = byte_len.div_ceil(size_of::<u64>());
    let mut storage = vec![0_u64; words];
    let information = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*information).Anonymous.ReplaceIfExists = false;
        (*information).RootDirectory = ptr::null_mut();
        (*information).FileNameLength = (name.len() * size_of::<u16>()) as u32;
        ptr::copy_nonoverlapping(
            name.as_ptr(),
            storage.as_mut_ptr().cast::<u8>().add(name_offset).cast(),
            name.len(),
        );
    }
    let renamed = unsafe {
        SetFileInformationByHandle(
            staged.file.as_raw_handle(),
            FileRenameInfo,
            storage.as_ptr().cast(),
            byte_len as u32,
        )
    };
    if renamed == 0 {
        return Err(io_error("replace", target, std::io::Error::last_os_error()));
    }
    staged
        .file
        .sync_all()
        .map_err(|source| io_error("sync replaced", target, source))
}

#[cfg(unix)]
fn rename_staged_file(staged: &StagedFile, target: &Path) -> Result<(), TlsError> {
    fs::rename(&staged.path, target).map_err(|source| io_error("replace", target, source))?;
    staged
        .file
        .sync_all()
        .map_err(|source| io_error("sync replaced", target, source))?;
    sync_directory(target.parent().ok_or_else(|| TlsError::MissingParent {
        path: target.to_path_buf(),
    })?)
}

#[cfg(not(any(unix, windows)))]
fn rename_staged_file(_staged: &StagedFile, target: &Path) -> Result<(), TlsError> {
    Err(io_error(
        "replace",
        target,
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "durable rename is unsupported on this platform",
        ),
    ))
}

#[cfg(unix)]
fn set_owner_only_permissions(path: &Path) -> Result<(), TlsError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|source| io_error("secure temporary", path, source))
}

#[cfg(not(any(unix, windows)))]
fn set_owner_only_permissions(_path: &Path) -> Result<(), TlsError> {
    Err(TlsError::PrivateKeyAcl(AclError::UnsupportedPlatform))
}

fn publish_marker(
    path: &Path,
    original_mask: u8,
    state: TransactionState,
    fault: PersistenceFault,
) -> Result<(), TlsError> {
    #[cfg(not(test))]
    let _ = fault;
    let marker_stage = sibling_path(path, "stage");
    remove_if_exists(&marker_stage)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker_stage)
        .map_err(|source| io_error("create transaction marker", &marker_stage, source))?;
    let state_text = match state {
        TransactionState::Prepared => "prepared",
        TransactionState::Committed => "committed",
    };
    write!(file, "{state_text}:{original_mask:02x}")
        .map_err(|source| io_error("write transaction marker", &marker_stage, source))?;
    file.flush()
        .map_err(|source| io_error("flush transaction marker", &marker_stage, source))?;
    file.sync_all()
        .map_err(|source| io_error("sync transaction marker", &marker_stage, source))?;
    drop(file);
    #[cfg(test)]
    if fault == PersistenceFault::MarkerReplacement(state) {
        return Err(io_error(
            "replace transaction marker",
            path,
            std::io::Error::other("injected marker replacement failure"),
        ));
    }
    durable_rename_path(&marker_stage, path, true)
        .map_err(|source| io_error("durably publish transaction marker", path, source))
}

fn recover_transaction(paths: &OriginTlsPaths, directory: &Path) -> Result<(), TlsError> {
    let targets = [
        paths.ca_cert.as_path(),
        paths.ca_key.as_path(),
        paths.leaf_cert.as_path(),
        paths.leaf_key.as_path(),
    ];
    let stages: [PathBuf; 4] = array::from_fn(|index| sibling_path(targets[index], "stage"));
    let backups: [PathBuf; 4] = array::from_fn(|index| sibling_path(targets[index], "backup"));
    let marker = directory.join(TRANSACTION_MARKER);

    if transaction_path_exists(&marker)? {
        let mut text = String::new();
        File::open(&marker)
            .and_then(|mut file| file.read_to_string(&mut text))
            .map_err(|source| io_error("read transaction marker", &marker, source))?;
        let (state, mask) = text
            .trim()
            .split_once(':')
            .ok_or_else(|| TlsError::Recovery("invalid transaction marker format".to_owned()))?;
        let state = match state {
            "prepared" => TransactionState::Prepared,
            "committed" => TransactionState::Committed,
            _ => {
                return Err(TlsError::Recovery(
                    "invalid transaction marker state".to_owned(),
                ));
            }
        };
        let original_mask = u8::from_str_radix(mask, 16)
            .map_err(|error| TlsError::Recovery(format!("invalid transaction marker: {error}")))?;
        let files = targets.map(|target| TransactionFile {
            target,
            contents: &[],
            private: false,
        });
        let mut all_targets_are_files = true;
        for target in targets {
            all_targets_are_files &= transaction_path_is_file(target)?;
        }
        if state == TransactionState::Committed && all_targets_are_files {
            cleanup_after_commit(&stages, &backups, &marker);
        } else {
            rollback_files(&files, &stages, &backups, original_mask, &marker, directory)
                .map_err(|error| TlsError::Recovery(error.to_string()))?;
        }
    } else {
        for stage in &stages {
            best_effort_remove(stage);
        }
        for backup in &backups {
            best_effort_remove(backup);
        }
        best_effort_remove(&sibling_path(&marker, "stage"));
        best_effort_remove(&sibling_path(&marker, "consumed"));
    }
    Ok(())
}

fn rollback_files(
    files: &[TransactionFile<'_>; 4],
    stages: &[PathBuf; 4],
    backups: &[PathBuf; 4],
    original_mask: u8,
    marker: &Path,
    _directory: &Path,
) -> Result<(), TlsError> {
    rollback_files_impl(
        files,
        stages,
        backups,
        original_mask,
        marker,
        _directory,
        PersistenceFault::None,
    )
}

fn rollback_files_impl(
    files: &[TransactionFile<'_>; 4],
    stages: &[PathBuf; 4],
    backups: &[PathBuf; 4],
    original_mask: u8,
    marker: &Path,
    _directory: &Path,
    fault: PersistenceFault,
) -> Result<(), TlsError> {
    #[cfg(not(test))]
    let _ = fault;
    for (index, (entry, backup)) in files.iter().zip(backups).enumerate().rev() {
        #[cfg(test)]
        if fault == PersistenceFault::RollbackStep(index) {
            return Err(io_error(
                "injected rollback step",
                entry.target,
                std::io::Error::other("injected rollback-step failure"),
            ));
        }
        if transaction_path_exists(backup)? {
            remove_if_exists(entry.target)?;
            durable_rename_path(backup, entry.target, false)
                .map_err(|source| io_error("restore", entry.target, source))?;
        } else if original_mask & (1 << index) == 0 {
            durable_discard_path(entry.target)?;
        }
    }
    for stage in stages {
        remove_if_exists(stage)?;
    }
    remove_if_exists(&sibling_path(marker, "stage"))?;
    durable_remove_marker(marker)?;
    #[cfg(unix)]
    sync_directory(_directory)?;
    Ok(())
}

fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(format!(".cellar-{suffix}"));
    path.with_file_name(name)
}

fn remove_if_exists(path: &Path) -> Result<(), TlsError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error("remove temporary", path, source)),
    }
}

fn transaction_path_exists(path: &Path) -> Result<bool, TlsError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(io_error("inspect transaction path", path, source)),
    }
}

fn transaction_path_is_file(path: &Path) -> Result<bool, TlsError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_file()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(io_error("inspect transaction target", path, source)),
    }
}

fn cleanup_stages(stages: &[PathBuf; 4], marker: &Path) {
    for stage in stages {
        best_effort_remove(stage);
    }
    best_effort_remove(&sibling_path(marker, "stage"));
}

fn cleanup_after_commit(stages: &[PathBuf; 4], backups: &[PathBuf; 4], marker: &Path) {
    let mut cleanup_complete = true;
    for stage in stages {
        cleanup_complete &= remove_if_exists(stage).is_ok();
    }
    for backup in backups {
        cleanup_complete &= remove_if_exists(backup).is_ok();
    }
    let marker_stage = sibling_path(marker, "stage");
    cleanup_complete &= remove_if_exists(&marker_stage).is_ok();
    if cleanup_complete {
        let _ = durable_remove_marker(marker);
    }
}

fn best_effort_remove(path: &Path) {
    let _ = fs::remove_file(path);
}

#[cfg(windows)]
fn durable_discard_path(path: &Path) -> Result<(), TlsError> {
    if !transaction_path_exists(path)? {
        return Ok(());
    }
    let discarded = sibling_path(path, "discarded");
    best_effort_remove(&discarded);
    durable_rename_path(path, &discarded, true)
        .map_err(|source| io_error("durably discard interrupted target", path, source))?;
    best_effort_remove(&discarded);
    Ok(())
}

#[cfg(unix)]
fn durable_discard_path(path: &Path) -> Result<(), TlsError> {
    remove_if_exists(path)?;
    sync_directory(path.parent().ok_or_else(|| TlsError::MissingParent {
        path: path.to_path_buf(),
    })?)
}

#[cfg(not(any(unix, windows)))]
fn durable_discard_path(path: &Path) -> Result<(), TlsError> {
    Err(io_error(
        "durably discard interrupted target",
        path,
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "durable removal is unsupported on this platform",
        ),
    ))
}

#[cfg(windows)]
fn durable_rename_path(source: &Path, target: &Path, replace: bool) -> std::io::Result<()> {
    use std::{iter, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source: Vec<u16> = source
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    let target: Vec<u16> = target
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    let mut flags = MOVEFILE_WRITE_THROUGH;
    if replace {
        flags |= MOVEFILE_REPLACE_EXISTING;
    }
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), flags) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn durable_rename_path(source: &Path, target: &Path, _replace: bool) -> std::io::Result<()> {
    fs::rename(source, target)?;
    let directory = target.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "target has no parent")
    })?;
    File::open(directory)?.sync_all()
}

#[cfg(not(any(unix, windows)))]
fn durable_rename_path(_source: &Path, _target: &Path, _replace: bool) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "durable rename is unsupported on this platform",
    ))
}

#[cfg(windows)]
fn durable_remove_marker(marker: &Path) -> Result<(), TlsError> {
    if !transaction_path_exists(marker)? {
        return Ok(());
    }
    let consumed = sibling_path(marker, "consumed");
    best_effort_remove(&consumed);
    durable_rename_path(marker, &consumed, true)
        .map_err(|source| io_error("durably consume transaction marker", marker, source))?;
    best_effort_remove(&consumed);
    Ok(())
}

#[cfg(unix)]
fn durable_remove_marker(marker: &Path) -> Result<(), TlsError> {
    remove_if_exists(marker)?;
    sync_directory(marker.parent().ok_or_else(|| TlsError::MissingParent {
        path: marker.to_path_buf(),
    })?)
}

#[cfg(not(any(unix, windows)))]
fn durable_remove_marker(marker: &Path) -> Result<(), TlsError> {
    Err(io_error(
        "durably consume transaction marker",
        marker,
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "durable marker removal is unsupported on this platform",
        ),
    ))
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> Result<(), TlsError> {
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|source| io_error("sync directory", directory, source))
}

fn io_error(operation: &'static str, path: &Path, source: std::io::Error) -> TlsError {
    TlsError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn paths(temp: &TempDir) -> OriginTlsPaths {
        OriginTlsPaths {
            ca_cert: temp.path().join("ca.pem"),
            ca_key: temp.path().join("ca-key.pem"),
            leaf_cert: temp.path().join("origin.pem"),
            leaf_key: temp.path().join("origin-key.pem"),
        }
    }

    fn serial(certificate: &CertificateDer<'_>) -> Vec<u8> {
        let (_, certificate) = parse_x509_certificate(certificate.as_ref()).unwrap();
        certificate.raw_serial().to_vec()
    }

    #[test]
    fn lifecycle_reuses_then_renews_only_the_leaf() {
        let temp = TempDir::new().unwrap();
        let paths = paths(&temp);
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();

        let first = ensure_origin_tls_for_test(&paths, now).unwrap();
        let marker = temp.path().join(ESTABLISHMENT_MARKER);
        let marker_before = fs::read(&marker).unwrap();
        let reused = ensure_origin_tls_for_test(&paths, now + Duration::days(300)).unwrap();
        assert_eq!(
            serial(first.ca_certificate()),
            serial(reused.ca_certificate())
        );
        assert_eq!(
            serial(&first.certificate_chain()[0]),
            serial(&reused.certificate_chain()[0])
        );

        let renewed = ensure_origin_tls_for_test(&paths, now + Duration::days(335)).unwrap();
        assert_eq!(
            serial(first.ca_certificate()),
            serial(renewed.ca_certificate())
        );
        assert_ne!(
            serial(&first.certificate_chain()[0]),
            serial(&renewed.certificate_chain()[0])
        );
        assert_eq!(fs::read(marker).unwrap(), marker_before);
    }

    #[test]
    fn near_ca_expiry_keeps_valid_material_and_warns_until_explicit_rotation() {
        let temp = TempDir::new().unwrap();
        let paths = paths(&temp);
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let first = ensure_origin_tls_for_test(&paths, now).unwrap();
        for day in [335, 670, 1005, 1340] {
            ensure_origin_tls_for_test(&paths, now + Duration::days(day)).unwrap();
        }
        let before = [
            fs::read(&paths.ca_cert).unwrap(),
            fs::read(&paths.ca_key).unwrap(),
            fs::read(&paths.leaf_cert).unwrap(),
            fs::read(&paths.leaf_key).unwrap(),
        ];

        let warning = ensure_origin_tls_for_test(&paths, now + Duration::days(1675)).unwrap();

        assert_eq!(warning.warnings(), &[TlsWarning::CaRotationRequired]);
        assert_eq!(
            serial(first.ca_certificate()),
            serial(warning.ca_certificate())
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

        let rotated = rotate_origin_ca_impl(&paths, now + Duration::days(1675), false).unwrap();
        assert!(rotated.cloudflared_ca_pool_and_route_update_required());
        assert_ne!(
            serial(first.ca_certificate()),
            serial(rotated.material().ca_certificate())
        );
    }

    #[test]
    fn invalid_established_bundle_fails_closed_without_replacing_ca() {
        let first_dir = TempDir::new().unwrap();
        let second_dir = TempDir::new().unwrap();
        let first_paths = paths(&first_dir);
        let second_paths = paths(&second_dir);
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let first = ensure_origin_tls_for_test(&first_paths, now).unwrap();
        ensure_origin_tls_for_test(&second_paths, now).unwrap();
        fs::copy(&second_paths.leaf_key, &first_paths.leaf_key).unwrap();

        let ca_before = fs::read(&first_paths.ca_cert).unwrap();
        let error = ensure_origin_tls_for_test(&first_paths, now).unwrap_err();

        assert!(matches!(error, TlsError::EstablishedBundleInvalid));
        assert_eq!(
            serial(first.ca_certificate()),
            serial(&CertificateDer::from(
                certificate_der_from_pem(&ca_before).unwrap()
            ))
        );
    }

    #[test]
    fn corrupt_certificate_and_key_files_fail_closed() {
        for corrupt_index in 0..4 {
            let temp = TempDir::new().unwrap();
            let paths = paths(&temp);
            let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
            let first = ensure_origin_tls_for_test(&paths, now).unwrap();
            let files = [
                &paths.ca_cert,
                &paths.ca_key,
                &paths.leaf_cert,
                &paths.leaf_key,
            ];
            fs::write(files[corrupt_index], b"definitely not PEM or DER").unwrap();

            let error = ensure_origin_tls_for_test(&paths, now).unwrap_err();

            assert!(matches!(error, TlsError::EstablishedBundleInvalid));
            if corrupt_index != 0 {
                assert_eq!(
                    serial(first.ca_certificate()),
                    serial(&CertificateDer::from(
                        certificate_der_from_pem(&fs::read(&paths.ca_cert).unwrap()).unwrap()
                    ))
                );
            }
        }
    }

    #[test]
    fn expired_established_bundle_fails_closed_without_rotation() {
        let temp = TempDir::new().unwrap();
        let paths = paths(&temp);
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let first = ensure_origin_tls_for_test(&paths, now).unwrap();
        let before = [
            fs::read(&paths.ca_cert).unwrap(),
            fs::read(&paths.ca_key).unwrap(),
            fs::read(&paths.leaf_cert).unwrap(),
            fs::read(&paths.leaf_key).unwrap(),
        ];

        let error = ensure_origin_tls_for_test(&paths, now + Duration::days(366)).unwrap_err();

        assert!(matches!(error, TlsError::EstablishedBundleInvalid));
        assert_eq!(
            serial(first.ca_certificate()),
            serial(&CertificateDer::from(
                certificate_der_from_pem(&fs::read(&paths.ca_cert).unwrap()).unwrap()
            ))
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
    fn deleting_all_established_files_fails_closed_until_explicit_rotation() {
        let temp = TempDir::new().unwrap();
        let paths = paths(&temp);
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let first = ensure_origin_tls_for_test(&paths, now).unwrap();
        let marker = temp.path().join(".cellar-origin-tls.established");
        assert!(marker.is_file());
        let marker_before = fs::read(&marker).unwrap();
        for path in [
            &paths.ca_cert,
            &paths.ca_key,
            &paths.leaf_cert,
            &paths.leaf_key,
        ] {
            fs::remove_file(path).unwrap();
        }

        let error = ensure_origin_tls_for_test(&paths, now + Duration::days(1)).unwrap_err();

        assert!(matches!(error, TlsError::EstablishedMaterialMissing));
        assert!(
            [
                &paths.ca_cert,
                &paths.ca_key,
                &paths.leaf_cert,
                &paths.leaf_key,
            ]
            .iter()
            .all(|path| !path.exists())
        );
        assert_eq!(fs::read(&marker).unwrap(), marker_before);

        let rotated = rotate_origin_ca_impl(&paths, now + Duration::days(1), false).unwrap();
        assert_ne!(
            serial(first.ca_certificate()),
            serial(rotated.material().ca_certificate())
        );
        assert_eq!(fs::read(&marker).unwrap(), marker_before);
        load_and_validate(&paths, now + Duration::days(1)).unwrap();
    }

    #[test]
    fn corrupt_establishment_marker_fails_closed_without_touching_bundle() {
        let temp = TempDir::new().unwrap();
        let paths = paths(&temp);
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        ensure_origin_tls_for_test(&paths, now).unwrap();
        let marker = temp.path().join(ESTABLISHMENT_MARKER);
        fs::write(&marker, b"corrupt establishment state").unwrap();
        let before = [
            fs::read(&paths.ca_cert).unwrap(),
            fs::read(&paths.ca_key).unwrap(),
            fs::read(&paths.leaf_cert).unwrap(),
            fs::read(&paths.leaf_key).unwrap(),
        ];

        let error = ensure_origin_tls_for_test(&paths, now).unwrap_err();

        assert!(matches!(error, TlsError::EstablishmentStateInvalid));
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
    fn complete_bundle_from_pre_marker_crash_is_recorded_without_ca_replacement() {
        let temp = TempDir::new().unwrap();
        let paths = paths(&temp);
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let bundle = generate_bundle(now).unwrap();
        let ca_serial = serial(&CertificateDer::from(bundle.ca_cert_der.clone()));
        persist_bundle(&paths, temp.path(), &bundle, false).unwrap();
        assert!(!temp.path().join(ESTABLISHMENT_MARKER).exists());

        let recovered = ensure_origin_tls_for_test(&paths, now).unwrap();

        assert_eq!(serial(recovered.ca_certificate()), ca_serial);
        assert_eq!(
            fs::read(temp.path().join(ESTABLISHMENT_MARKER)).unwrap(),
            ESTABLISHMENT_MARKER_CONTENT
        );
    }

    #[test]
    fn marker_publication_failure_reports_committed_rotation_and_restart_backfills() {
        let temp = TempDir::new().unwrap();
        let paths = paths(&temp);
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let marker = temp.path().join(ESTABLISHMENT_MARKER);
        let marker_stage = sibling_path(&marker, "stage");
        fs::create_dir(&marker_stage).unwrap();

        let error = rotate_origin_ca_impl(&paths, now, false).unwrap_err();
        let committed = error
            .committed_rotation()
            .expect("rotation result must explicitly report committed trust");
        assert!(committed.cloudflared_ca_pool_and_route_update_required());
        let committed_ca_serial = serial(committed.material().ca_certificate());
        assert_eq!(
            committed_ca_serial,
            serial(&CertificateDer::from(
                certificate_der_from_pem(&fs::read(&paths.ca_cert).unwrap()).unwrap()
            ))
        );
        assert!(!marker.exists());

        fs::remove_dir(marker_stage).unwrap();
        let recovered = ensure_origin_tls_for_test(&paths, now).unwrap();

        assert_eq!(serial(recovered.ca_certificate()), committed_ca_serial);
        assert_eq!(fs::read(marker).unwrap(), ESTABLISHMENT_MARKER_CONTENT);
    }

    #[test]
    fn concurrent_initialization_publishes_one_consistent_generation() {
        use std::sync::{Arc, Barrier};

        let temp = TempDir::new().unwrap();
        let paths = Arc::new(paths(&temp));
        let barrier = Arc::new(Barrier::new(8));
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let workers = (0..8)
            .map(|_| {
                let paths = Arc::clone(&paths);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let material = ensure_origin_tls_for_test(&paths, now).unwrap();
                    (
                        serial(material.ca_certificate()),
                        serial(&material.certificate_chain()[0]),
                    )
                })
            })
            .collect::<Vec<_>>();
        let results = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();

        assert!(results.iter().all(|result| result == &results[0]));
        assert!(!temp.path().join(TRANSACTION_MARKER).exists());
        load_and_validate(&paths, now).unwrap();
    }

    #[test]
    fn cross_process_initialization_publishes_one_consistent_generation() {
        run_cross_process_initialization_test(false);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires elevated Administrator or NT SERVICE\\Cellar identity"]
    fn protected_cross_process_lock_serializes_production_initialization() {
        run_cross_process_initialization_test(true);
    }

    fn run_cross_process_initialization_test(protect: bool) {
        use std::process::Command;
        use std::time::Instant;

        let temp = TempDir::new().unwrap();
        let executable = std::env::current_exe().unwrap();
        let start_path = temp.path().join("workers.start");
        let workers = (0..6)
            .map(|index| {
                let result_path = temp.path().join(format!("worker-{index}.serial"));
                let ready_path = temp.path().join(format!("worker-{index}.ready"));
                let mut command = Command::new(&executable);
                command
                    .arg("--exact")
                    .arg("tls::tests::origin_tls_process_worker")
                    .arg("--nocapture")
                    .env("CELLAR_TLS_PROCESS_TEST_DIR", temp.path())
                    .env("CELLAR_TLS_PROCESS_TEST_RESULT", &result_path)
                    .env("CELLAR_TLS_PROCESS_TEST_READY", &ready_path)
                    .env("CELLAR_TLS_PROCESS_TEST_START", &start_path)
                    .env("CELLAR_TLS_PROCESS_TEST_PROTECT", protect.to_string());
                (command.spawn().unwrap(), result_path, ready_path)
            })
            .collect::<Vec<_>>();

        let deadline = Instant::now() + std::time::Duration::from_secs(15);
        while !workers.iter().all(|(_, _, ready)| ready.exists()) {
            if Instant::now() >= deadline {
                fs::write(&start_path, b"release").unwrap();
                panic!("child processes did not reach the cross-process start gate");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        fs::write(&start_path, b"release").unwrap();

        let mut serials = Vec::new();
        for (mut worker, result_path, _) in workers {
            assert!(worker.wait().unwrap().success());
            serials.push(fs::read(result_path).unwrap());
        }
        assert!(serials.iter().all(|serial| serial == &serials[0]));
        let paths = OriginTlsPaths {
            ca_cert: temp.path().join("ca.pem"),
            ca_key: temp.path().join("ca-key.pem"),
            leaf_cert: temp.path().join("origin.pem"),
            leaf_key: temp.path().join("origin-key.pem"),
        };
        load_and_validate(
            &paths,
            OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
        )
        .unwrap();
        assert!(!temp.path().join(TRANSACTION_MARKER).exists());
    }

    #[test]
    fn origin_tls_process_worker() {
        let Ok(directory) = std::env::var("CELLAR_TLS_PROCESS_TEST_DIR") else {
            return;
        };
        let result_path = std::env::var("CELLAR_TLS_PROCESS_TEST_RESULT").unwrap();
        let ready_path = std::env::var("CELLAR_TLS_PROCESS_TEST_READY").unwrap();
        let start_path = std::env::var("CELLAR_TLS_PROCESS_TEST_START").unwrap();
        let protect = std::env::var("CELLAR_TLS_PROCESS_TEST_PROTECT").unwrap() == "true";
        let directory = PathBuf::from(directory);
        let paths = OriginTlsPaths {
            ca_cert: directory.join("ca.pem"),
            ca_key: directory.join("ca-key.pem"),
            leaf_cert: directory.join("origin.pem"),
            leaf_key: directory.join("origin-key.pem"),
        };
        fs::write(ready_path, b"ready").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !Path::new(&start_path).exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for the cross-process start gate"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let material = ensure_origin_tls_impl(
            &paths,
            OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
            protect,
        )
        .unwrap();
        fs::write(result_path, serial(material.ca_certificate())).unwrap();
    }

    #[test]
    fn tls_material_has_secret_drop_and_redacted_debug() {
        let bundle =
            generate_bundle(OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()).unwrap();
        let secret_prefix = bundle.leaf_key_der[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let material = bundle.into_material(None);

        assert!(std::mem::needs_drop::<TlsMaterial>());
        let debug = format!("{material:?}");
        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains(&secret_prefix));
    }

    #[cfg(windows)]
    #[test]
    fn acl_failure_leaves_no_staged_file_that_could_contain_a_key() {
        let temp = TempDir::new().unwrap();
        let paths = paths(&temp);
        let bundle =
            generate_bundle(OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()).unwrap();

        let error = persist_bundle_for_test(
            &paths,
            temp.path(),
            &bundle,
            true,
            TestPersistenceFault::PrivateKeyAcl,
        )
        .unwrap_err();

        let error_text = format!("{error:?}");
        assert!(matches!(error, TlsError::PrivateKeyAcl(_)));
        assert!(!error_text.contains("PRIVATE KEY"));
        assert!(
            !error_text.contains(
                &bundle.leaf_key_der[..16]
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            )
        );
        for target in [
            &paths.ca_cert,
            &paths.ca_key,
            &paths.leaf_cert,
            &paths.leaf_key,
        ] {
            assert!(!target.exists());
            assert!(!sibling_path(target, "stage").exists());
        }
    }

    #[test]
    fn prepared_transaction_recovers_at_marker_and_every_replacement_boundary() {
        let faults = [
            TestPersistenceFault::CrashAfterPreparedMarker,
            TestPersistenceFault::CrashAfterBackup(0),
            TestPersistenceFault::CrashAfterBackup(1),
            TestPersistenceFault::CrashAfterBackup(2),
            TestPersistenceFault::CrashAfterBackup(3),
            TestPersistenceFault::CrashAfterReplacement(0),
            TestPersistenceFault::CrashAfterReplacement(1),
            TestPersistenceFault::CrashAfterReplacement(2),
            TestPersistenceFault::CrashAfterReplacement(3),
        ];
        for fault in faults {
            let temp = TempDir::new().unwrap();
            let paths = paths(&temp);
            let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
            let original = generate_bundle(now).unwrap();
            persist_bundle(&paths, temp.path(), &original, false).unwrap();
            let before = [
                fs::read(&paths.ca_cert).unwrap(),
                fs::read(&paths.ca_key).unwrap(),
                fs::read(&paths.leaf_cert).unwrap(),
                fs::read(&paths.leaf_key).unwrap(),
            ];
            let replacement = generate_bundle(now + Duration::days(1)).unwrap();

            let error = persist_bundle_for_test(&paths, temp.path(), &replacement, false, fault)
                .unwrap_err();
            assert!(matches!(error, TlsError::InjectedCrash(_)));
            assert!(
                fs::read_to_string(temp.path().join(TRANSACTION_MARKER))
                    .unwrap()
                    .starts_with("prepared:")
            );

            recover_transaction(&paths, temp.path()).unwrap();
            let after = [
                fs::read(&paths.ca_cert).unwrap(),
                fs::read(&paths.ca_key).unwrap(),
                fs::read(&paths.leaf_cert).unwrap(),
                fs::read(&paths.leaf_key).unwrap(),
            ];
            assert_eq!(after, before, "recovery failed for {fault:?}");
        }
    }

    #[test]
    fn marker_replacement_and_rollback_step_failures_remain_recoverable() {
        let faults = [
            TestPersistenceFault::MarkerReplacement(TransactionState::Prepared),
            TestPersistenceFault::MarkerReplacement(TransactionState::Committed),
            TestPersistenceFault::RollbackStep(0),
            TestPersistenceFault::RollbackStep(2),
        ];
        for fault in faults {
            let temp = TempDir::new().unwrap();
            let paths = paths(&temp);
            let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
            let original = generate_bundle(now).unwrap();
            persist_bundle(&paths, temp.path(), &original, false).unwrap();
            let before = [
                fs::read(&paths.ca_cert).unwrap(),
                fs::read(&paths.ca_key).unwrap(),
                fs::read(&paths.leaf_cert).unwrap(),
                fs::read(&paths.leaf_key).unwrap(),
            ];
            let replacement = generate_bundle(now + Duration::days(1)).unwrap();

            assert!(
                persist_bundle_for_test(&paths, temp.path(), &replacement, false, fault).is_err()
            );
            recover_transaction(&paths, temp.path()).unwrap();
            let after = [
                fs::read(&paths.ca_cert).unwrap(),
                fs::read(&paths.ca_key).unwrap(),
                fs::read(&paths.leaf_cert).unwrap(),
                fs::read(&paths.leaf_key).unwrap(),
            ];
            assert_eq!(after, before, "recovery failed for {fault:?}");
            assert!(!temp.path().join(TRANSACTION_MARKER).exists());
            assert!(!sibling_path(&temp.path().join(TRANSACTION_MARKER), "stage").exists());
        }
    }

    #[cfg(windows)]
    #[test]
    fn locked_backup_keeps_committed_marker_until_cleanup_can_finish() {
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_SHARE_READ: u32 = 1;

        let temp = TempDir::new().unwrap();
        let paths = paths(&temp);
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let original = generate_bundle(now).unwrap();
        persist_bundle(&paths, temp.path(), &original, false).unwrap();
        let replacement = generate_bundle(now + Duration::days(1)).unwrap();

        persist_bundle_for_test(
            &paths,
            temp.path(),
            &replacement,
            false,
            TestPersistenceFault::Cleanup,
        )
        .unwrap();

        let backup = sibling_path(&paths.ca_cert, "backup");
        assert!(backup.exists());
        let locked_backup = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&backup)
            .unwrap();

        recover_transaction(&paths, temp.path()).unwrap();
        assert!(
            temp.path().join(TRANSACTION_MARKER).exists(),
            "committed marker must remain while any backup cannot be removed"
        );
        assert!(backup.exists());

        drop(locked_backup);
        recover_transaction(&paths, temp.path()).unwrap();
        assert_eq!(fs::read(&paths.ca_cert).unwrap(), replacement.ca_cert_pem);
        assert!(!backup.exists());
        assert!(!temp.path().join(TRANSACTION_MARKER).exists());

        let next = generate_bundle(now + Duration::days(2)).unwrap();
        persist_bundle(&paths, temp.path(), &next, false).unwrap();
        assert_eq!(fs::read(&paths.ca_cert).unwrap(), next.ca_cert_pem);
    }

    #[test]
    fn leaf_validation_rejects_additional_dns_names() {
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let bundle = generate_bundle(now).unwrap();
        let ca_key = KeyPair::from_pem(std::str::from_utf8(&bundle.ca_key_pem).unwrap()).unwrap();
        let issuer =
            Issuer::from_ca_cert_pem(std::str::from_utf8(&bundle.ca_cert_pem).unwrap(), &ca_key)
                .unwrap();
        let leaf_key = KeyPair::generate().unwrap();
        let mut parameters = leaf_parameters(now);
        parameters.subject_alt_names.push(rcgen::SanType::DnsName(
            "unexpected.local".try_into().unwrap(),
        ));
        let leaf_der = parameters.signed_by(&leaf_key, &issuer).unwrap();
        let (_, leaf) = parse_x509_certificate(leaf_der.der()).unwrap();
        let (_, ca) = parse_x509_certificate(&bundle.ca_cert_der).unwrap();

        assert!(validate_leaf(&leaf, &leaf_key, &ca, now).is_err());
    }

    #[test]
    fn leaf_validation_requires_digital_signature_usage() {
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let bundle = generate_bundle(now).unwrap();
        let ca_key = KeyPair::from_pem(std::str::from_utf8(&bundle.ca_key_pem).unwrap()).unwrap();
        let issuer =
            Issuer::from_ca_cert_pem(std::str::from_utf8(&bundle.ca_cert_pem).unwrap(), &ca_key)
                .unwrap();
        let leaf_key = KeyPair::generate().unwrap();
        let mut parameters = leaf_parameters(now);
        parameters.key_usages.clear();
        let leaf_der = parameters.signed_by(&leaf_key, &issuer).unwrap();
        let (_, leaf) = parse_x509_certificate(leaf_der.der()).unwrap();
        let (_, ca) = parse_x509_certificate(&bundle.ca_cert_der).unwrap();

        assert!(validate_leaf(&leaf, &leaf_key, &ca, now).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn a_failed_bundle_replace_rolls_back_every_file() {
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_SHARE_READ: u32 = 1;

        let temp = TempDir::new().unwrap();
        let paths = paths(&temp);
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        ensure_origin_tls_for_test(&paths, now).unwrap();
        let before = [
            fs::read(&paths.ca_cert).unwrap(),
            fs::read(&paths.ca_key).unwrap(),
            fs::read(&paths.leaf_cert).unwrap(),
            fs::read(&paths.leaf_key).unwrap(),
        ];
        let _locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&paths.leaf_key)
            .unwrap();

        assert!(ensure_origin_tls_for_test(&paths, now + Duration::days(335)).is_err());
        let after = [
            fs::read(&paths.ca_cert).unwrap(),
            fs::read(&paths.ca_key).unwrap(),
            fs::read(&paths.leaf_cert).unwrap(),
            fs::read(&paths.leaf_key).unwrap(),
        ];
        assert_eq!(after, before);
    }

    #[cfg(windows)]
    #[test]
    fn rollback_can_remove_an_already_protected_staged_key() {
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_SHARE_READ: u32 = 1;

        let temp = TempDir::new().unwrap();
        let paths = paths(&temp);
        let directory = temp.path();
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let original = generate_bundle(now).unwrap();
        persist_bundle(&paths, directory, &original, false).unwrap();
        let before = [
            fs::read(&paths.ca_cert).unwrap(),
            fs::read(&paths.ca_key).unwrap(),
            fs::read(&paths.leaf_cert).unwrap(),
            fs::read(&paths.leaf_key).unwrap(),
        ];
        let replacement = generate_bundle(now + Duration::days(1)).unwrap();
        let _locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&paths.leaf_cert)
            .unwrap();

        let error = persist_bundle(&paths, directory, &replacement, true).unwrap_err();

        assert!(
            !matches!(error, TlsError::Rollback(_)),
            "rollback must use its retained handle: {error}"
        );
        let after = [
            fs::read(&paths.ca_cert).unwrap(),
            fs::read(&paths.ca_key).unwrap(),
            fs::read(&paths.leaf_cert).unwrap(),
            fs::read(&paths.leaf_key).unwrap(),
        ];
        assert_eq!(after, before);
    }
}
