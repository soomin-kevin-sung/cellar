use std::{
    array,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use cellar_windows::acl::{AclError, PRIVATE_KEY_SERVICE_NAME, restrict_private_key_access};
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

const CA_COMMON_NAME: &str = "Cellar Local CA";
const ORIGIN_DNS_NAME: &str = "cellar.local";
const BACKDATE: Duration = Duration::minutes(5);
const CA_LIFETIME: Duration = Duration::days(5 * 365);
const LEAF_LIFETIME: Duration = Duration::days(365);
const RENEWAL_WINDOW: Duration = Duration::days(30);
const TRANSACTION_MARKER: &str = ".cellar-origin-tls.transaction";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OriginTlsPaths {
    pub ca_cert: PathBuf,
    pub ca_key: PathBuf,
    pub leaf_cert: PathBuf,
    pub leaf_key: PathBuf,
}

pub struct TlsMaterial {
    certificate_chain: Vec<CertificateDer<'static>>,
    private_key: PrivateKeyDer<'static>,
    ca_certificate: CertificateDer<'static>,
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
        &self.private_key
    }

    pub fn ca_certificate(&self) -> &CertificateDer<'static> {
        &self.ca_certificate
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
    #[error("could not recover the previous origin TLS transaction: {0}")]
    Recovery(String),
    #[error("could not preserve the previous origin TLS bundle after a write failure: {0}")]
    Rollback(String),
}

struct Bundle {
    ca_cert_pem: Vec<u8>,
    ca_key_pem: Vec<u8>,
    leaf_cert_pem: Vec<u8>,
    leaf_key_pem: Vec<u8>,
    ca_cert_der: Vec<u8>,
    leaf_cert_der: Vec<u8>,
    leaf_key_der: Vec<u8>,
}

struct ExistingBundle {
    bundle: Bundle,
    ca_key: KeyPair,
    leaf_not_after: OffsetDateTime,
    ca_not_after: OffsetDateTime,
}

enum ExistingState {
    Valid(Box<ExistingBundle>),
    AbsentOrInvalid,
}

pub fn ensure_origin_tls(
    paths: &OriginTlsPaths,
    now: OffsetDateTime,
) -> Result<TlsMaterial, TlsError> {
    ensure_origin_tls_impl(paths, now, true)
}

fn ensure_origin_tls_impl(
    paths: &OriginTlsPaths,
    now: OffsetDateTime,
    protect_private_keys: bool,
) -> Result<TlsMaterial, TlsError> {
    let directory = common_directory(paths)?;
    fs::create_dir_all(directory)
        .map_err(|source| io_error("create directory", directory, source))?;
    recover_transaction(paths, directory)?;

    match load_existing(paths, now) {
        ExistingState::Valid(existing) => {
            if existing.leaf_not_after - now > RENEWAL_WINDOW {
                if protect_private_keys {
                    secure_existing_keys(paths)?;
                }
                return Ok(existing.bundle.into_material());
            }

            if existing.ca_not_after >= now + LEAF_LIFETIME {
                let renewed = renew_leaf(*existing, now)?;
                persist_bundle(paths, directory, &renewed, protect_private_keys)?;
                return Ok(renewed.into_material());
            }
        }
        ExistingState::AbsentOrInvalid => {}
    }

    let generated = generate_bundle(now)?;
    persist_bundle(paths, directory, &generated, protect_private_keys)?;
    Ok(generated.into_material())
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

fn secure_existing_keys(paths: &OriginTlsPaths) -> Result<(), TlsError> {
    #[cfg(windows)]
    {
        restrict_private_key_access(&paths.ca_key, PRIVATE_KEY_SERVICE_NAME)?;
        restrict_private_key_access(&paths.leaf_key, PRIVATE_KEY_SERVICE_NAME)?;
    }
    #[cfg(not(windows))]
    {
        let _ = paths;
    }
    Ok(())
}

fn load_existing(paths: &OriginTlsPaths, now: OffsetDateTime) -> ExistingState {
    match load_and_validate(paths, now) {
        Ok(bundle) => ExistingState::Valid(Box::new(bundle)),
        Err(()) => ExistingState::AbsentOrInvalid,
    }
}

fn load_and_validate(paths: &OriginTlsPaths, now: OffsetDateTime) -> Result<ExistingBundle, ()> {
    let ca_cert_pem = fs::read(&paths.ca_cert).map_err(|_| ())?;
    let ca_key_pem = fs::read(&paths.ca_key).map_err(|_| ())?;
    let leaf_cert_pem = fs::read(&paths.leaf_cert).map_err(|_| ())?;
    let leaf_key_pem = fs::read(&paths.leaf_key).map_err(|_| ())?;
    let ca_cert_der = certificate_der_from_pem(&ca_cert_pem)?;
    let leaf_cert_der = certificate_der_from_pem(&leaf_cert_pem)?;
    let ca_key_text = std::str::from_utf8(&ca_key_pem).map_err(|_| ())?;
    let leaf_key_text = std::str::from_utf8(&leaf_key_pem).map_err(|_| ())?;
    let ca_key = KeyPair::from_pem(ca_key_text).map_err(|_| ())?;
    let leaf_key = KeyPair::from_pem(leaf_key_text).map_err(|_| ())?;

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
    let leaf_key_der = leaf_key.serialize_der();
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
    let ca_key = KeyPair::generate()?;
    let leaf_key = KeyPair::generate()?;
    let ca_params = ca_parameters(now);
    let ca_cert = ca_params.self_signed(&ca_key)?;
    let issuer = Issuer::from_params(&ca_params, &ca_key);
    let leaf_cert = leaf_parameters(now).signed_by(&leaf_key, &issuer)?;

    Ok(Bundle {
        ca_cert_pem: ca_cert.pem().into_bytes(),
        ca_key_pem: ca_key.serialize_pem().into_bytes(),
        leaf_cert_pem: leaf_cert.pem().into_bytes(),
        leaf_key_pem: leaf_key.serialize_pem().into_bytes(),
        ca_cert_der: ca_cert.der().to_vec(),
        leaf_cert_der: leaf_cert.der().to_vec(),
        leaf_key_der: leaf_key.serialize_der(),
    })
}

fn renew_leaf(existing: ExistingBundle, now: OffsetDateTime) -> Result<Bundle, TlsError> {
    let leaf_key = KeyPair::generate()?;
    let ca_pem = std::str::from_utf8(&existing.bundle.ca_cert_pem)
        .map_err(|_| TlsError::Recovery("validated CA PEM was not UTF-8".to_owned()))?;
    let issuer = Issuer::from_ca_cert_pem(ca_pem, &existing.ca_key)?;
    let leaf_cert = leaf_parameters(now).signed_by(&leaf_key, &issuer)?;
    Ok(Bundle {
        ca_cert_pem: existing.bundle.ca_cert_pem,
        ca_key_pem: existing.bundle.ca_key_pem,
        leaf_cert_pem: leaf_cert.pem().into_bytes(),
        leaf_key_pem: leaf_key.serialize_pem().into_bytes(),
        ca_cert_der: existing.bundle.ca_cert_der,
        leaf_cert_der: leaf_cert.der().to_vec(),
        leaf_key_der: leaf_key.serialize_der(),
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
    fn into_material(self) -> TlsMaterial {
        let ca_certificate = CertificateDer::from(self.ca_cert_der);
        let leaf_certificate = CertificateDer::from(self.leaf_cert_der);
        TlsMaterial {
            certificate_chain: vec![leaf_certificate, ca_certificate.clone()],
            private_key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.leaf_key_der)),
            ca_certificate,
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

fn persist_bundle(
    paths: &OriginTlsPaths,
    directory: &Path,
    bundle: &Bundle,
    protect_private_keys: bool,
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
    for ((entry, stage), backup) in files.iter().zip(&stages).zip(&backups) {
        remove_if_exists(stage)?;
        remove_if_exists(backup)?;
        staged_files.push(write_staged_file(
            stage,
            entry.contents,
            entry.private && protect_private_keys,
        )?);
    }

    let original_mask = files.iter().enumerate().fold(0_u8, |mask, (index, entry)| {
        mask | (u8::from(entry.target.exists()) << index)
    });
    write_marker(&marker, original_mask)?;

    let commit_result = (|| {
        for ((entry, staged), backup) in files.iter().zip(&staged_files).zip(&backups) {
            if entry.target.exists() {
                fs::rename(entry.target, backup)
                    .map_err(|source| io_error("back up", entry.target, source))?;
            }
            rename_staged_file(staged, entry.target)?;
        }
        sync_directory(directory)?;
        fs::remove_file(&marker).map_err(|source| io_error("commit", &marker, source))?;
        sync_directory(directory)
    })();

    if let Err(commit_error) = commit_result {
        if let Err(rollback_error) =
            rollback_files(&files, &stages, &backups, original_mask, &marker, directory)
        {
            return Err(TlsError::Rollback(format!(
                "{rollback_error}; original write error: {commit_error}"
            )));
        }
        return Err(commit_error);
    }

    for backup in &backups {
        remove_if_exists(backup)?;
    }
    Ok(())
}

fn write_staged_file(path: &Path, contents: &[u8], private: bool) -> Result<StagedFile, TlsError> {
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
    file.write_all(contents)
        .map_err(|source| io_error("write temporary", path, source))?;
    file.flush()
        .map_err(|source| io_error("flush temporary", path, source))?;
    file.sync_all()
        .map_err(|source| io_error("sync temporary", path, source))?;
    if private {
        #[cfg(windows)]
        restrict_private_key_access(path, PRIVATE_KEY_SERVICE_NAME)?;
        #[cfg(not(windows))]
        set_owner_only_permissions(path)?;
    }
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
    Ok(())
}

#[cfg(not(windows))]
fn rename_staged_file(staged: &StagedFile, target: &Path) -> Result<(), TlsError> {
    fs::rename(&staged.path, target).map_err(|source| io_error("replace", target, source))
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

fn write_marker(path: &Path, original_mask: u8) -> Result<(), TlsError> {
    let marker_stage = sibling_path(path, "stage");
    remove_if_exists(&marker_stage)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker_stage)
        .map_err(|source| io_error("create transaction marker", &marker_stage, source))?;
    write!(file, "{original_mask:02x}")
        .map_err(|source| io_error("write transaction marker", &marker_stage, source))?;
    file.flush()
        .map_err(|source| io_error("flush transaction marker", &marker_stage, source))?;
    file.sync_all()
        .map_err(|source| io_error("sync transaction marker", &marker_stage, source))?;
    drop(file);
    fs::rename(&marker_stage, path)
        .map_err(|source| io_error("publish transaction marker", path, source))
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

    if marker.exists() {
        let mut text = String::new();
        File::open(&marker)
            .and_then(|mut file| file.read_to_string(&mut text))
            .map_err(|source| io_error("read transaction marker", &marker, source))?;
        let original_mask = u8::from_str_radix(text.trim(), 16)
            .map_err(|error| TlsError::Recovery(format!("invalid transaction marker: {error}")))?;
        let files = targets.map(|target| TransactionFile {
            target,
            contents: &[],
            private: false,
        });
        rollback_files(&files, &stages, &backups, original_mask, &marker, directory)
            .map_err(|error| TlsError::Recovery(error.to_string()))?;
    } else {
        for stage in &stages {
            remove_if_exists(stage)?;
        }
        for backup in &backups {
            remove_if_exists(backup)?;
        }
    }
    Ok(())
}

fn rollback_files(
    files: &[TransactionFile<'_>; 4],
    stages: &[PathBuf; 4],
    backups: &[PathBuf; 4],
    original_mask: u8,
    marker: &Path,
    directory: &Path,
) -> Result<(), TlsError> {
    for (index, (entry, backup)) in files.iter().zip(backups).enumerate().rev() {
        if backup.exists() {
            remove_if_exists(entry.target)?;
            fs::rename(backup, entry.target)
                .map_err(|source| io_error("restore", entry.target, source))?;
        } else if original_mask & (1 << index) == 0 {
            remove_if_exists(entry.target)?;
        }
    }
    for stage in stages {
        remove_if_exists(stage)?;
    }
    remove_if_exists(marker)?;
    sync_directory(directory)
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

#[cfg(unix)]
fn sync_directory(directory: &Path) -> Result<(), TlsError> {
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|source| io_error("sync directory", directory, source))
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> Result<(), TlsError> {
    Ok(())
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
    }

    #[test]
    fn invalid_partial_bundle_is_replaced_completely() {
        let first_dir = TempDir::new().unwrap();
        let second_dir = TempDir::new().unwrap();
        let first_paths = paths(&first_dir);
        let second_paths = paths(&second_dir);
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let first = ensure_origin_tls_for_test(&first_paths, now).unwrap();
        ensure_origin_tls_for_test(&second_paths, now).unwrap();
        fs::copy(&second_paths.leaf_key, &first_paths.leaf_key).unwrap();

        let repaired = ensure_origin_tls_for_test(&first_paths, now).unwrap();

        assert_ne!(
            serial(first.ca_certificate()),
            serial(repaired.ca_certificate())
        );
        let (_, ca) = parse_x509_certificate(repaired.ca_certificate().as_ref()).unwrap();
        let (_, leaf) = parse_x509_certificate(repaired.certificate_chain()[0].as_ref()).unwrap();
        leaf.verify_signature(Some(ca.public_key())).unwrap();
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
