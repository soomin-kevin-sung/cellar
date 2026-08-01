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
    fn secure(&self, probe: &mut Self::Probe) -> Result<(), AdapterError>;
    fn write_all(&self, probe: &mut Self::Probe, bytes: &[u8]) -> Result<(), AdapterError>;
    fn flush(&self, probe: &mut Self::Probe) -> Result<(), AdapterError>;
    fn rename_no_replace(
        &self,
        root: &Self::RootHandle,
        probe: &mut Self::Probe,
        destination: &ProbeName,
    ) -> Result<(), AdapterError>;
    fn delete_owned(&self, probe: Self::Probe) -> Result<(), AdapterError>;
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
    CleanupFailed,
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
            Self::CleanupFailed => "probe_cleanup_failed",
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
    let probe = run_probe(adapter, &root_handle)?;
    let current = match adapter.current_coordinates(&root_handle) {
        Ok(current) => current,
        Err(_) => return fail_after_cleanup(adapter, probe, PreflightError::IoFailed),
    };
    if current != inspection.coordinates {
        return fail_after_cleanup(adapter, probe, PreflightError::IdentityMismatch);
    }
    adapter
        .delete_owned(probe)
        .map_err(|_| PreflightError::CleanupFailed)?;
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

fn run_probe<A: PreflightAdapter>(
    adapter: &A,
    root: &A::RootHandle,
) -> Result<A::Probe, PreflightError> {
    for _ in 0..MAX_COLLISION_RETRIES {
        let source = ProbeName::random("source");
        let destination = ProbeName::random("renamed");
        let mut probe = match adapter.create_new(root, &source) {
            Ok(probe) => probe,
            Err(error) if error.is_collision() => continue,
            Err(_) => return Err(PreflightError::IoFailed),
        };
        if adapter.secure(&mut probe).is_err() {
            return fail_after_cleanup(adapter, probe, PreflightError::IoFailed);
        }
        if adapter.write_all(&mut probe, PROBE_SENTINEL).is_err() {
            return fail_after_cleanup(adapter, probe, PreflightError::IoFailed);
        }
        if adapter.flush(&mut probe).is_err() {
            return fail_after_cleanup(adapter, probe, PreflightError::IoFailed);
        }
        match adapter.rename_no_replace(root, &mut probe, &destination) {
            Ok(()) => return Ok(probe),
            Err(error) => {
                if adapter.delete_owned(probe).is_err() {
                    return Err(PreflightError::CleanupFailed);
                }
                if error.is_collision() {
                    continue;
                }
                return Err(PreflightError::IoFailed);
            }
        }
    }
    Err(PreflightError::IoFailed)
}

fn fail_after_cleanup<A: PreflightAdapter, T>(
    adapter: &A,
    probe: A::Probe,
    original: PreflightError,
) -> Result<T, PreflightError> {
    match adapter.delete_owned(probe) {
        Ok(()) => Err(original),
        Err(_) => Err(PreflightError::CleanupFailed),
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::{self, File, OpenOptions};
    use std::io::Write;
    use std::mem::{offset_of, size_of};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::{Path, PathBuf};
    use std::ptr;
    use std::sync::Arc;

    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_ENCRYPTED,
        FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
        FILE_ATTRIBUTE_RECALL_ON_OPEN, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_WRITE, FILE_LIST_DIRECTORY, FILE_RENAME_INFO,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfo, GetDriveTypeW,
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
                .access_mode(FILE_LIST_DIRECTORY)
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
            let empty = is_directory && !is_reparse && retained_directory_is_empty(&file)?;
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
                },
            ))
        }

        fn create_new(
            &self,
            root: &Self::RootHandle,
            name: &ProbeName,
        ) -> Result<Self::Probe, AdapterError> {
            create_relative_file(root.file.as_raw_handle(), name).map(|file| WindowsProbe { file })
        }

        fn secure(&self, probe: &mut Self::Probe) -> Result<(), AdapterError> {
            restrict_private_key_handle(&probe.file, PRIVATE_KEY_SERVICE_NAME)
                .map_err(|_| AdapterError::io())
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
            let name: Vec<u16> = destination.as_str().encode_utf16().collect();
            let name_offset = offset_of!(FILE_RENAME_INFO, FileName);
            let byte_len = name_offset + name.len() * size_of::<u16>();
            let mut storage = vec![0_u64; byte_len.div_ceil(size_of::<u64>())];
            let information = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
            // SAFETY: the u64 backing storage is suitably aligned and sized
            // for the fixed header plus the exact UTF-16 name bytes copied.
            unsafe {
                (*information).Anonymous.ReplaceIfExists = false;
                (*information).RootDirectory = root.file.as_raw_handle();
                (*information).FileNameLength = (name.len() * size_of::<u16>()) as u32;
                ptr::copy_nonoverlapping(
                    name.as_ptr(),
                    storage.as_mut_ptr().cast::<u8>().add(name_offset).cast(),
                    name.len(),
                );
            }
            let mut status_block = IoStatusBlock {
                status_or_pointer: 0,
                information: 0,
            };
            // SAFETY: both handles stay live and `storage` contains the
            // initialized FILE_RENAME_INFORMATION for the synchronous call.
            let status = unsafe {
                NtSetInformationFile(
                    probe.file.as_raw_handle(),
                    &mut status_block,
                    storage.as_mut_ptr().cast(),
                    byte_len as u32,
                    10,
                )
            };
            if status < 0 {
                return Err(map_ntstatus(status));
            }
            Ok(())
        }

        fn delete_owned(&self, probe: Self::Probe) -> Result<(), AdapterError> {
            let information =
                windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO { DeleteFile: true };
            // SAFETY: the owned probe handle was opened with DELETE access and
            // `information` matches FileDispositionInfo for this synchronous call.
            let result = unsafe {
                SetFileInformationByHandle(
                    probe.file.as_raw_handle(),
                    FileDispositionInfo,
                    (&raw const information).cast(),
                    size_of_val(&information) as u32,
                )
            };
            drop(probe);
            if result == 0 {
                Err(AdapterError::io())
            } else {
                Ok(())
            }
        }

        fn current_coordinates(
            &self,
            root: &Self::RootHandle,
        ) -> Result<StorageCoordinates, AdapterError> {
            let mut information = BY_HANDLE_FILE_INFORMATION::default();
            // SAFETY: the retained root `File` owns a live handle and the
            // output structure is writable for the call.
            if unsafe { GetFileInformationByHandle(root.file.as_raw_handle(), &mut information) }
                == 0
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

    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }

    #[repr(C)]
    struct ObjectAttributes {
        length: u32,
        root_directory: *mut core::ffi::c_void,
        object_name: *mut UnicodeString,
        attributes: u32,
        security_descriptor: *mut core::ffi::c_void,
        security_quality_of_service: *mut core::ffi::c_void,
    }

    #[repr(C)]
    struct IoStatusBlock {
        status_or_pointer: usize,
        information: usize,
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtCreateFile(
            file_handle: *mut *mut core::ffi::c_void,
            desired_access: u32,
            object_attributes: *mut ObjectAttributes,
            io_status_block: *mut IoStatusBlock,
            allocation_size: *mut i64,
            file_attributes: u32,
            share_access: u32,
            create_disposition: u32,
            create_options: u32,
            ea_buffer: *mut core::ffi::c_void,
            ea_length: u32,
        ) -> i32;
        fn NtSetInformationFile(
            file_handle: *mut core::ffi::c_void,
            io_status_block: *mut IoStatusBlock,
            file_information: *mut core::ffi::c_void,
            length: u32,
            file_information_class: u32,
        ) -> i32;
        fn NtQueryDirectoryFile(
            file_handle: *mut core::ffi::c_void,
            event: *mut core::ffi::c_void,
            apc_routine: *mut core::ffi::c_void,
            apc_context: *mut core::ffi::c_void,
            io_status_block: *mut IoStatusBlock,
            file_information: *mut core::ffi::c_void,
            length: u32,
            file_information_class: u32,
            return_single_entry: bool,
            file_name: *mut core::ffi::c_void,
            restart_scan: bool,
        ) -> i32;
    }

    pub(super) fn retained_directory_is_empty(root: &File) -> Result<bool, AdapterError> {
        const STATUS_SUCCESS: i32 = 0;
        const STATUS_NO_MORE_FILES: i32 = 0x8000_0006_u32 as i32;
        const FILE_NAMES_INFORMATION: u32 = 12;
        const BUFFER_BYTES: usize = 64 * 1024;

        // `u64` backing storage guarantees the alignment required for the
        // variable-length FILE_NAMES_INFORMATION records returned by NTFS.
        let mut storage = vec![0_u64; BUFFER_BYTES / size_of::<u64>()];
        let mut restart_scan = true;
        loop {
            let mut status_block = IoStatusBlock {
                status_or_pointer: 0,
                information: 0,
            };
            // SAFETY: `root` retains the directory object opened with
            // FILE_LIST_DIRECTORY. The aligned output buffer and status block
            // remain live for this synchronous query; all optional pointers
            // are null and no pathname is resolved.
            let status = unsafe {
                NtQueryDirectoryFile(
                    root.as_raw_handle(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut status_block,
                    storage.as_mut_ptr().cast(),
                    BUFFER_BYTES as u32,
                    FILE_NAMES_INFORMATION,
                    false,
                    ptr::null_mut(),
                    restart_scan,
                )
            };
            restart_scan = false;
            if status == STATUS_NO_MORE_FILES {
                return Ok(true);
            }
            if status != STATUS_SUCCESS {
                return Err(AdapterError::io());
            }
            let returned = status_block.information;
            if returned == 0 || returned > BUFFER_BYTES {
                return Err(AdapterError::io());
            }
            // SAFETY: `returned` was range-checked against the initialized
            // output allocation and is used only for byte-level validation.
            let bytes =
                unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<u8>(), returned) };
            if records_contain_non_dot_entry(bytes)? {
                return Ok(false);
            }
        }
    }

    pub(super) fn records_contain_non_dot_entry(bytes: &[u8]) -> Result<bool, AdapterError> {
        const HEADER_BYTES: usize = 3 * size_of::<u32>();
        let mut record_offset = 0_usize;
        let mut found_non_dot = false;
        loop {
            if !record_offset.is_multiple_of(size_of::<u32>()) {
                return Err(AdapterError::io());
            }
            let header_end = record_offset
                .checked_add(HEADER_BYTES)
                .filter(|end| *end <= bytes.len())
                .ok_or_else(AdapterError::io)?;
            let next_offset = read_u32(bytes, record_offset)? as usize;
            let name_length = read_u32(bytes, record_offset + 2 * size_of::<u32>())? as usize;
            if name_length == 0 || !name_length.is_multiple_of(size_of::<u16>()) {
                return Err(AdapterError::io());
            }
            let name_end = header_end
                .checked_add(name_length)
                .filter(|end| *end <= bytes.len())
                .ok_or_else(AdapterError::io)?;
            let name = &bytes[header_end..name_end];
            if name != b".\0" && name != b".\0.\0" {
                found_non_dot = true;
            }
            if next_offset == 0 {
                return if name_end == bytes.len() {
                    Ok(found_non_dot)
                } else {
                    Err(AdapterError::io())
                };
            }
            if !next_offset.is_multiple_of(size_of::<u32>())
                || next_offset < HEADER_BYTES + name_length
            {
                return Err(AdapterError::io());
            }
            record_offset = record_offset
                .checked_add(next_offset)
                .filter(|offset| *offset < bytes.len())
                .ok_or_else(AdapterError::io)?;
        }
    }

    fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, AdapterError> {
        let end = offset
            .checked_add(size_of::<u32>())
            .filter(|end| *end <= bytes.len())
            .ok_or_else(AdapterError::io)?;
        let mut value = [0_u8; size_of::<u32>()];
        value.copy_from_slice(&bytes[offset..end]);
        Ok(u32::from_ne_bytes(value))
    }

    fn map_ntstatus(status: i32) -> AdapterError {
        const STATUS_OBJECT_NAME_COLLISION: i32 = 0xC000_0035_u32 as i32;
        if status == STATUS_OBJECT_NAME_COLLISION {
            AdapterError::already_exists()
        } else {
            AdapterError::io()
        }
    }

    fn create_relative_file(
        root: *mut core::ffi::c_void,
        name: &ProbeName,
    ) -> Result<File, AdapterError> {
        const OBJ_CASE_INSENSITIVE: u32 = 0x0000_0040;
        const FILE_CREATE: u32 = 2;
        const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
        const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
        const STATUS_OBJECT_NAME_COLLISION: i32 = 0xC000_0035_u32 as i32;

        let mut encoded: Vec<u16> = name.as_str().encode_utf16().collect();
        let byte_length = encoded
            .len()
            .checked_mul(size_of::<u16>())
            .and_then(|length| u16::try_from(length).ok())
            .ok_or_else(AdapterError::io)?;
        let mut object_name = UnicodeString {
            length: byte_length,
            maximum_length: byte_length,
            buffer: encoded.as_mut_ptr(),
        };
        let mut attributes = ObjectAttributes {
            length: size_of::<ObjectAttributes>() as u32,
            root_directory: root,
            object_name: &mut object_name,
            attributes: OBJ_CASE_INSENSITIVE,
            security_descriptor: ptr::null_mut(),
            security_quality_of_service: ptr::null_mut(),
        };
        let mut status_block = IoStatusBlock {
            status_or_pointer: 0,
            information: 0,
        };
        let mut handle = ptr::null_mut();
        // SAFETY: every pointer references a live, correctly laid-out Windows
        // structure for this synchronous call. The object name is a private,
        // separator-free name relative to the retained directory handle. On
        // success, ownership of the returned handle is transferred once.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                FILE_GENERIC_WRITE | DELETE | WRITE_DAC,
                &mut attributes,
                &mut status_block,
                ptr::null_mut(),
                FILE_ATTRIBUTE_NORMAL,
                FILE_SHARE_DELETE,
                FILE_CREATE,
                FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT,
                ptr::null_mut(),
                0,
            )
        };
        if status == STATUS_OBJECT_NAME_COLLISION {
            return Err(AdapterError::already_exists());
        }
        if status < 0 || handle.is_null() {
            return Err(AdapterError::io());
        }
        // SAFETY: NtCreateFile returned a successful uniquely owned handle.
        Ok(unsafe { File::from_raw_handle(handle) })
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
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    use super::platform::{
        TrustedRootHandle, WindowsPreflight, records_contain_non_dot_entry,
        retained_directory_is_empty,
    };
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
        };
        let source = ProbeName("source.tmp".into());
        let destination = ProbeName("destination.tmp".into());
        let mut probe = WindowsPreflight.create_new(&root, &source).unwrap();

        WindowsPreflight
            .rename_no_replace(&root, &mut probe, &destination)
            .unwrap();
        assert!(!directory.path().join(source.as_str()).exists());
        assert!(directory.path().join(destination.as_str()).exists());
        WindowsPreflight.delete_owned(probe).unwrap();
    }

    #[test]
    fn retained_root_handle_cannot_be_redirected_by_path_swap() {
        let parent = tempdir().unwrap();
        let configured = parent.path().join("configured");
        let retained = parent.path().join("retained");
        std::fs::create_dir(&configured).unwrap();
        let (_, root) = WindowsPreflight.inspect(&configured).unwrap();
        std::fs::rename(&configured, &retained).unwrap();
        std::fs::create_dir(&configured).unwrap();
        let source = ProbeName("source.tmp".into());
        let destination = ProbeName("destination.tmp".into());

        let mut probe = WindowsPreflight.create_new(&root, &source).unwrap();
        WindowsPreflight
            .write_all(&mut probe, b"trusted-root")
            .unwrap();
        WindowsPreflight.flush(&mut probe).unwrap();

        assert!(retained.join(source.as_str()).exists());
        assert!(!configured.join(source.as_str()).exists());
        WindowsPreflight
            .rename_no_replace(&root, &mut probe, &destination)
            .unwrap();
        assert!(retained.join(destination.as_str()).exists());
        assert!(!configured.join(destination.as_str()).exists());
        WindowsPreflight.delete_owned(probe).unwrap();
        assert!(!retained.join(destination.as_str()).exists());
        assert!(!configured.join(destination.as_str()).exists());
    }

    #[test]
    fn retained_root_emptiness_cannot_be_redirected_by_path_swap() {
        let parent = tempdir().unwrap();
        let configured = parent.path().join("configured");
        let retained = parent.path().join("retained");
        std::fs::create_dir(&configured).unwrap();
        std::fs::write(configured.join("existing-data"), b"must be detected").unwrap();
        let root = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&configured)
            .unwrap();

        std::fs::rename(&configured, &retained).unwrap();
        std::fs::create_dir(&configured).unwrap();

        assert!(!retained_directory_is_empty(&root).unwrap());
        assert!(configured.read_dir().unwrap().next().is_none());
        assert!(retained.join("existing-data").exists());
    }

    #[test]
    fn names_records_accept_a_valid_four_byte_aligned_chain() {
        let mut bytes = vec![0_u8; 34];
        bytes[0..4].copy_from_slice(&20_u32.to_ne_bytes());
        bytes[8..12].copy_from_slice(&6_u32.to_ne_bytes());
        bytes[12..18].copy_from_slice(b"a\0b\0c\0");
        bytes[28..32].copy_from_slice(&2_u32.to_ne_bytes());
        bytes[32..34].copy_from_slice(b"x\0");

        assert!(records_contain_non_dot_entry(&bytes).unwrap());
    }

    #[test]
    fn names_records_reject_trailing_bytes_after_the_terminal_record() {
        let mut bytes = vec![0_u8; 15];
        bytes[8..12].copy_from_slice(&2_u32.to_ne_bytes());
        bytes[12..14].copy_from_slice(b".\0");

        assert!(records_contain_non_dot_entry(&bytes).is_err());
    }
}
