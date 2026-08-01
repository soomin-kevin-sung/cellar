use std::fmt;
use std::path::Path;

const PROBE_SENTINEL: &[u8] = b"cellar-storage-preflight-v1\n";
const MAX_COLLISION_RETRIES: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Filesystem {
    Ntfs,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VolumeKind {
    FixedLocal,
    Network,
    Removable,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RootAttributes {
    pub directory: bool,
    pub empty: bool,
    pub volume_root: bool,
    pub protected_root: bool,
    pub reparse_point: bool,
    pub encrypted: bool,
    pub offline_placeholder: bool,
}

impl RootAttributes {
    #[must_use]
    pub const fn ordinary_empty() -> Self {
        Self {
            directory: true,
            empty: true,
            volume_root: false,
            protected_root: false,
            reparse_point: false,
            encrypted: false,
            offline_placeholder: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StorageCoordinates {
    pub volume_serial: u64,
    pub root_file_id: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RootInspection {
    pub filesystem: Filesystem,
    pub volume_kind: VolumeKind,
    pub attributes: RootAttributes,
    pub coordinates: StorageCoordinates,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AdapterErrorKind {
    AlreadyExists,
    Io,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdapterError(AdapterErrorKind);

impl AdapterError {
    #[must_use]
    pub const fn already_exists() -> Self {
        Self(AdapterErrorKind::AlreadyExists)
    }

    #[must_use]
    pub const fn io() -> Self {
        Self(AdapterErrorKind::Io)
    }

    const fn is_collision(self) -> bool {
        matches!(self.0, AdapterErrorKind::AlreadyExists)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ProbeName(String);

impl ProbeName {
    fn random(suffix: &str) -> Self {
        Self(format!(
            ".cellar-preflight-{}-{suffix}",
            uuid::Uuid::now_v7().simple()
        ))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ProbeName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProbeName(<random>)")
    }
}

pub trait PreflightAdapter {
    type RootHandle: Send + Sync;
    type Probe;

    fn inspect(&self, root: &Path) -> Result<(RootInspection, Self::RootHandle), AdapterError>;
    fn create_new(
        &self,
        root: &Self::RootHandle,
        name: &ProbeName,
    ) -> Result<Self::Probe, AdapterError>;
    fn write_all(&self, probe: &mut Self::Probe, bytes: &[u8]) -> Result<(), AdapterError>;
    fn flush(&self, probe: &mut Self::Probe) -> Result<(), AdapterError>;
    fn rename_no_replace(
        &self,
        root: &Self::RootHandle,
        probe: &mut Self::Probe,
        destination: &ProbeName,
    ) -> Result<(), AdapterError>;
    fn delete(&self, root: &Self::RootHandle, name: &ProbeName) -> Result<(), AdapterError>;
    fn current_coordinates(
        &self,
        root: &Self::RootHandle,
    ) -> Result<StorageCoordinates, AdapterError>;
}

pub struct StorageIdentity<H> {
    coordinates: StorageCoordinates,
    trusted_root: H,
}

impl<H> StorageIdentity<H> {
    #[must_use]
    pub const fn coordinates(&self) -> StorageCoordinates {
        self.coordinates
    }

    #[must_use]
    pub const fn trusted_root(&self) -> &H {
        &self.trusted_root
    }
}

impl<H> fmt::Debug for StorageIdentity<H> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StorageIdentity")
            .field("volume_serial", &self.coordinates.volume_serial)
            .field("root_file_id", &self.coordinates.root_file_id)
            .field("trusted_root", &"<retained>")
            .finish()
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum PreflightError {
    NtfsRequired,
    ReparseRootForbidden,
    FixedLocalVolumeRequired,
    RootMustBeNewOrEmpty,
    ProtectedRootForbidden,
    EncryptedRootForbidden,
    OfflinePlaceholderForbidden,
    IoFailed,
    IdentityMismatch,
    UnsupportedPlatform,
}

impl PreflightError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NtfsRequired => "ntfs_required",
            Self::ReparseRootForbidden => "reparse_root_forbidden",
            Self::FixedLocalVolumeRequired => "fixed_local_volume_required",
            Self::RootMustBeNewOrEmpty => "root_must_be_new_or_empty",
            Self::ProtectedRootForbidden => "protected_root_forbidden",
            Self::EncryptedRootForbidden => "encrypted_root_forbidden",
            Self::OfflinePlaceholderForbidden => "offline_placeholder_forbidden",
            Self::IoFailed => "preflight_io_failed",
            Self::IdentityMismatch => "identity_mismatch",
            Self::UnsupportedPlatform => "preflight_unsupported",
        }
    }
}

impl fmt::Debug for PreflightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl fmt::Display for PreflightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for PreflightError {}

pub fn preflight_with<A: PreflightAdapter>(
    adapter: &A,
    root: &Path,
) -> Result<StorageIdentity<A::RootHandle>, PreflightError> {
    let (inspection, root_handle) = adapter
        .inspect(root)
        .map_err(|_| PreflightError::IoFailed)?;
    validate(inspection)?;
    run_probe(adapter, &root_handle)?;
    let current = adapter
        .current_coordinates(&root_handle)
        .map_err(|_| PreflightError::IoFailed)?;
    if current != inspection.coordinates {
        return Err(PreflightError::IdentityMismatch);
    }
    Ok(StorageIdentity {
        coordinates: inspection.coordinates,
        trusted_root: root_handle,
    })
}

fn validate(inspection: RootInspection) -> Result<(), PreflightError> {
    if inspection.filesystem != Filesystem::Ntfs {
        return Err(PreflightError::NtfsRequired);
    }
    if inspection.volume_kind != VolumeKind::FixedLocal {
        return Err(PreflightError::FixedLocalVolumeRequired);
    }
    let attributes = inspection.attributes;
    if attributes.reparse_point {
        return Err(PreflightError::ReparseRootForbidden);
    }
    if attributes.protected_root {
        return Err(PreflightError::ProtectedRootForbidden);
    }
    if attributes.encrypted {
        return Err(PreflightError::EncryptedRootForbidden);
    }
    if attributes.offline_placeholder {
        return Err(PreflightError::OfflinePlaceholderForbidden);
    }
    if !attributes.directory || !attributes.empty || attributes.volume_root {
        return Err(PreflightError::RootMustBeNewOrEmpty);
    }
    Ok(())
}

fn run_probe<A: PreflightAdapter>(adapter: &A, root: &A::RootHandle) -> Result<(), PreflightError> {
    for _ in 0..MAX_COLLISION_RETRIES {
        let source = ProbeName::random("source");
        let destination = ProbeName::random("renamed");
        let mut probe = match adapter.create_new(root, &source) {
            Ok(probe) => probe,
            Err(error) if error.is_collision() => continue,
            Err(_) => return Err(PreflightError::IoFailed),
        };
        if adapter.write_all(&mut probe, PROBE_SENTINEL).is_err()
            || adapter.flush(&mut probe).is_err()
        {
            cleanup(adapter, root, &source);
            return Err(PreflightError::IoFailed);
        }
        match adapter.rename_no_replace(root, &mut probe, &destination) {
            Ok(()) => {
                if adapter.delete(root, &destination).is_err() {
                    cleanup(adapter, root, &destination);
                    return Err(PreflightError::IoFailed);
                }
                return Ok(());
            }
            Err(error) => {
                cleanup(adapter, root, &source);
                if error.is_collision() {
                    continue;
                }
                return Err(PreflightError::IoFailed);
            }
        }
    }
    Err(PreflightError::IoFailed)
}

fn cleanup<A: PreflightAdapter>(adapter: &A, root: &A::RootHandle, name: &ProbeName) {
    let _ = adapter.delete(root, name);
}

#[cfg(windows)]
mod platform {
    use std::fs::{self, File, OpenOptions};
    use std::io::Write;
    use std::mem::{offset_of, size_of};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use std::ptr;
    use std::sync::Arc;

    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_ENCRYPTED,
        FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
        FILE_ATTRIBUTE_RECALL_ON_OPEN, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_WRITE, FILE_RENAME_INFO, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FileRenameInfo, GetDriveTypeW,
        GetFileInformationByHandle, GetFinalPathNameByHandleW, GetVolumeInformationByHandleW,
        GetVolumePathNameW, SetFileInformationByHandle, WRITE_DAC,
    };
    use windows_sys::Win32::System::WindowsProgramming::{
        DRIVE_FIXED, DRIVE_REMOTE, DRIVE_REMOVABLE,
    };

    use crate::acl::{PRIVATE_KEY_SERVICE_NAME, restrict_private_key_handle};

    use super::{
        AdapterError, Filesystem, PreflightAdapter, PreflightError, ProbeName, RootAttributes,
        RootInspection, StorageCoordinates, StorageIdentity, VolumeKind, preflight_with,
    };

    const MAX_PATH_CHARS: usize = 32_768;

    #[derive(Clone)]
    pub struct TrustedRootHandle {
        pub(super) file: Arc<File>,
        pub(super) path: Arc<PathBuf>,
    }

    impl std::fmt::Debug for TrustedRootHandle {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("TrustedRootHandle(<redacted>)")
        }
    }

    pub struct WindowsPreflight;

    pub struct WindowsProbe {
        pub(super) file: File,
    }

    impl PreflightAdapter for WindowsPreflight {
        type Probe = WindowsProbe;
        type RootHandle = TrustedRootHandle;

        fn inspect(&self, root: &Path) -> Result<(RootInspection, Self::RootHandle), AdapterError> {
            if !root.exists() {
                fs::create_dir(root).map_err(map_io)?;
            }
            let file = OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(root)
                .map_err(map_io)?;
            let raw = file.as_raw_handle();
            let mut information = BY_HANDLE_FILE_INFORMATION::default();
            // SAFETY: `raw` is a live `File` handle and `information` is a
            // writable structure for the duration of the call.
            if unsafe { GetFileInformationByHandle(raw, &mut information) } == 0 {
                return Err(AdapterError::io());
            }
            let attributes = information.dwFileAttributes;
            let filesystem = filesystem(raw)?;
            let (volume_kind, volume_root) = volume_kind(root)?;
            let final_path = final_path(raw)?;
            let is_directory = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
            let is_reparse = attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
            // Do not enumerate through a reparse root. Validation rejects it
            // using the attributes obtained from the non-following handle.
            let empty =
                is_directory && !is_reparse && fs::read_dir(root).map_err(map_io)?.next().is_none();
            let coordinates = StorageCoordinates {
                volume_serial: u64::from(information.dwVolumeSerialNumber),
                root_file_id: (u128::from(information.nFileIndexHigh) << 32)
                    | u128::from(information.nFileIndexLow),
            };
            let inspection = RootInspection {
                filesystem,
                volume_kind,
                attributes: RootAttributes {
                    directory: is_directory,
                    empty,
                    volume_root,
                    protected_root: is_protected_root(&final_path),
                    reparse_point: is_reparse,
                    encrypted: attributes & FILE_ATTRIBUTE_ENCRYPTED != 0,
                    offline_placeholder: attributes
                        & (FILE_ATTRIBUTE_OFFLINE
                            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
                            | FILE_ATTRIBUTE_RECALL_ON_OPEN)
                        != 0,
                },
                coordinates,
            };
            Ok((
                inspection,
                TrustedRootHandle {
                    file: Arc::new(file),
                    path: Arc::new(root.to_path_buf()),
                },
            ))
        }

        fn create_new(
            &self,
            root: &Self::RootHandle,
            name: &ProbeName,
        ) -> Result<Self::Probe, AdapterError> {
            let path = root.path.join(name.as_str());
            let file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .access_mode(FILE_GENERIC_WRITE | DELETE | WRITE_DAC)
                .share_mode(FILE_SHARE_DELETE)
                .open(&path)
                .map_err(map_io)?;
            if restrict_private_key_handle(&file, PRIVATE_KEY_SERVICE_NAME).is_err() {
                drop(file);
                let _ = fs::remove_file(path);
                return Err(AdapterError::io());
            }
            Ok(WindowsProbe { file })
        }

        fn write_all(&self, probe: &mut Self::Probe, bytes: &[u8]) -> Result<(), AdapterError> {
            probe.file.write_all(bytes).map_err(map_io)
        }

        fn flush(&self, probe: &mut Self::Probe) -> Result<(), AdapterError> {
            probe.file.sync_all().map_err(map_io)
        }

        fn rename_no_replace(
            &self,
            root: &Self::RootHandle,
            probe: &mut Self::Probe,
            destination: &ProbeName,
        ) -> Result<(), AdapterError> {
            let target = root.path.join(destination.as_str());
            let absolute = root
                .path
                .canonicalize()
                .map_err(map_io)?
                .join(target.file_name().ok_or_else(AdapterError::io)?);
            let name: Vec<u16> = absolute.as_os_str().encode_wide().collect();
            let name_offset = offset_of!(FILE_RENAME_INFO, FileName);
            let byte_len = name_offset + name.len() * size_of::<u16>();
            let mut storage = vec![0_u64; byte_len.div_ceil(size_of::<u64>())];
            let information = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
            // SAFETY: the u64 backing storage is suitably aligned and sized
            // for the fixed header plus the exact UTF-16 name bytes copied.
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
            // SAFETY: the probe handle stays live and `storage` contains the
            // initialized FILE_RENAME_INFO for the full system call.
            if unsafe {
                SetFileInformationByHandle(
                    probe.file.as_raw_handle(),
                    FileRenameInfo,
                    storage.as_ptr().cast(),
                    byte_len as u32,
                )
            } == 0
            {
                return Err(map_io(std::io::Error::last_os_error()));
            }
            Ok(())
        }

        fn delete(&self, root: &Self::RootHandle, name: &ProbeName) -> Result<(), AdapterError> {
            fs::remove_file(root.path.join(name.as_str())).map_err(map_io)
        }

        fn current_coordinates(
            &self,
            root: &Self::RootHandle,
        ) -> Result<StorageCoordinates, AdapterError> {
            let mut retained_information = BY_HANDLE_FILE_INFORMATION::default();
            // SAFETY: the retained root `File` owns a live handle and the
            // output structure is writable for the call.
            if unsafe {
                GetFileInformationByHandle(root.file.as_raw_handle(), &mut retained_information)
            } == 0
            {
                return Err(AdapterError::io());
            }
            let current = OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(root.path.as_path())
                .map_err(map_io)?;
            let mut information = BY_HANDLE_FILE_INFORMATION::default();
            // SAFETY: `current` remains live and the output structure is
            // writable for the duration of the call.
            if unsafe { GetFileInformationByHandle(current.as_raw_handle(), &mut information) } == 0
            {
                return Err(AdapterError::io());
            }
            Ok(StorageCoordinates {
                volume_serial: u64::from(information.dwVolumeSerialNumber),
                root_file_id: (u128::from(information.nFileIndexHigh) << 32)
                    | u128::from(information.nFileIndexLow),
            })
        }
    }

    fn filesystem(handle: *mut core::ffi::c_void) -> Result<Filesystem, AdapterError> {
        let mut name = [0_u16; 32];
        // SAFETY: `handle` comes from a live root `File`; unused output
        // pointers are null and `name` has the advertised writable capacity.
        if unsafe {
            GetVolumeInformationByHandleW(
                handle,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                name.as_mut_ptr(),
                name.len() as u32,
            )
        } == 0
        {
            return Err(AdapterError::io());
        }
        let length = name
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(name.len());
        let filesystem = String::from_utf16_lossy(&name[..length]);
        Ok(if filesystem.eq_ignore_ascii_case("NTFS") {
            Filesystem::Ntfs
        } else {
            Filesystem::Other
        })
    }

    fn volume_kind(root: &Path) -> Result<(VolumeKind, bool), AdapterError> {
        let input = wide_null(root);
        let mut volume = [0_u16; MAX_PATH_CHARS];
        // SAFETY: both buffers are NUL-terminated/readable or writable for
        // their advertised lengths.
        if unsafe { GetVolumePathNameW(input.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) }
            == 0
        {
            return Err(AdapterError::io());
        }
        // SAFETY: the successful call above populated a NUL-terminated volume path.
        let drive = unsafe { GetDriveTypeW(volume.as_ptr()) };
        let kind = match drive {
            DRIVE_FIXED => VolumeKind::FixedLocal,
            DRIVE_REMOTE => VolumeKind::Network,
            DRIVE_REMOVABLE => VolumeKind::Removable,
            _ => VolumeKind::Unknown,
        };
        let volume_len = volume
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(volume.len());
        let volume_path = PathBuf::from(std::ffi::OsString::from_wide(&volume[..volume_len]));
        let volume_root = same_path(root, &volume_path);
        Ok((kind, volume_root))
    }

    fn final_path(handle: *mut core::ffi::c_void) -> Result<PathBuf, AdapterError> {
        let mut output = vec![0_u16; MAX_PATH_CHARS];
        // SAFETY: `handle` is live and `output` is writable for the supplied capacity.
        let length = unsafe {
            GetFinalPathNameByHandleW(handle, output.as_mut_ptr(), output.len() as u32, 0)
        };
        let length = usize::try_from(length).map_err(|_| AdapterError::io())?;
        if length == 0 || length >= output.len() {
            return Err(AdapterError::io());
        }
        Ok(PathBuf::from(std::ffi::OsString::from_wide(
            &output[..length],
        )))
    }

    fn is_protected_root(path: &Path) -> bool {
        let normalized = path
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .trim_end_matches(['\\', '/'])
            .to_ascii_lowercase();
        let Some((_, tail)) = normalized.split_once(':') else {
            return true;
        };
        let tail = tail.replace('/', "\\");
        [
            r"\windows",
            r"\program files",
            r"\program files (x86)",
            r"\programdata",
            r"\system volume information",
            r"\$recycle.bin",
        ]
        .iter()
        .any(|protected| tail == *protected || tail.starts_with(&format!("{protected}\\")))
    }

    fn same_path(left: &Path, right: &Path) -> bool {
        left.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .eq_ignore_ascii_case(right.to_string_lossy().trim_end_matches(['\\', '/']))
    }

    fn wide_null(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn map_io(error: std::io::Error) -> AdapterError {
        match error.raw_os_error().map(|code| code as u32) {
            Some(ERROR_ALREADY_EXISTS | ERROR_FILE_EXISTS) => AdapterError::already_exists(),
            _ => AdapterError::io(),
        }
    }

    pub fn run_as_service(
        root: &Path,
    ) -> Result<StorageIdentity<TrustedRootHandle>, PreflightError> {
        preflight_with(&WindowsPreflight, root)
    }
}

#[cfg(windows)]
pub use platform::{TrustedRootHandle, run_as_service};

#[cfg(not(windows))]
#[derive(Debug)]
pub struct TrustedRootHandle;

#[cfg(not(windows))]
pub fn run_as_service(_root: &Path) -> Result<StorageIdentity<TrustedRootHandle>, PreflightError> {
    Err(PreflightError::UnsupportedPlatform)
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use std::sync::Arc;

    use tempfile::tempdir;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    use super::platform::{TrustedRootHandle, WindowsPreflight};
    use super::{PreflightAdapter, ProbeName};

    #[test]
    fn non_elevated_handle_probe_has_delete_access_for_no_replace_rename() {
        let directory = tempdir().unwrap();
        let root_file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(directory.path())
            .unwrap();
        let root = TrustedRootHandle {
            file: Arc::new(root_file),
            path: Arc::new(directory.path().to_path_buf()),
        };
        let source = ProbeName("source.tmp".into());
        let destination = ProbeName("destination.tmp".into());
        let mut probe = WindowsPreflight.create_new(&root, &source).unwrap();

        WindowsPreflight
            .rename_no_replace(&root, &mut probe, &destination)
            .unwrap();
        assert!(!directory.path().join(source.as_str()).exists());
        assert!(directory.path().join(destination.as_str()).exists());
        WindowsPreflight.delete(&root, &destination).unwrap();
    }
}
