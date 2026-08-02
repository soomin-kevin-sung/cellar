#[cfg(not(windows))]
use std::path::Path;

use cellar_storage::{FileIdentity, SafeName, Storage, StorageError, StorageErrorKind};

use crate::WindowsName;

#[cfg(windows)]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Seek, SeekFrom};
    use std::mem::{offset_of, size_of, size_of_val};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::{Path, PathBuf};
    use std::ptr;
    use std::sync::Arc;

    use cellar_storage::{EntryKind, FileIdentity, StorageError, StorageErrorKind};
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND,
        ERROR_PATH_NOT_FOUND, GetLastError,
    };
    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_CASE_SENSITIVE_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_ID_INFO,
        FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FileCaseSensitiveInfo, FileIdInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    };

    use crate::WindowsName;

    const MAX_PATH_CHARS: usize = 32_768;
    const FILE_CS_FLAG_CASE_SENSITIVE_DIR: u32 = 0x0000_0001;

    #[derive(Clone)]
    pub struct VerifiedHandle {
        file: Arc<File>,
        identity: FileIdentity,
        kind: EntryKind,
    }

    impl VerifiedHandle {
        pub fn identity(&self) -> FileIdentity {
            self.identity
        }

        pub fn kind(&self) -> EntryKind {
            self.kind
        }
    }

    impl std::fmt::Debug for VerifiedHandle {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("VerifiedHandle")
                .field("identity", &self.identity)
                .field("kind", &self.kind)
                .finish_non_exhaustive()
        }
    }

    pub struct WindowsStorage {
        root: VerifiedHandle,
        root_volume: u64,
    }

    impl WindowsStorage {
        pub fn open(path: &Path) -> Result<Self, StorageError> {
            let file = OpenOptions::new()
                .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(path)
                .map_err(map_io)?;
            let facts = inspect(&file)?;
            if facts.kind != EntryKind::Directory || facts.reparse || facts.case_sensitive {
                return Err(unsupported());
            }
            let root = VerifiedHandle {
                file: Arc::new(file),
                identity: facts.identity,
                kind: facts.kind,
            };
            Ok(Self {
                root_volume: facts.identity.volume_serial,
                root,
            })
        }

        pub fn root(&self) -> &VerifiedHandle {
            &self.root
        }

        pub fn open_verified(
            &self,
            parent: &VerifiedHandle,
            name: &WindowsName,
        ) -> Result<VerifiedHandle, StorageError> {
            self.verify_parent(parent)?;
            let file = open_relative(parent.file.as_raw_handle(), name, OpenMode::Existing)?;
            self.verify_new_handle(file)
        }

        pub fn create_file_no_replace(
            &self,
            parent: &VerifiedHandle,
            name: &WindowsName,
        ) -> Result<VerifiedHandle, StorageError> {
            self.verify_parent(parent)?;
            let file = open_relative(parent.file.as_raw_handle(), name, OpenMode::CreateFile)?;
            self.verify_new_handle(file)
        }

        pub fn create_directory_no_replace(
            &self,
            parent: &VerifiedHandle,
            name: &WindowsName,
        ) -> Result<VerifiedHandle, StorageError> {
            self.verify_parent(parent)?;
            let file = open_relative(parent.file.as_raw_handle(), name, OpenMode::CreateDirectory)?;
            self.verify_new_handle(file)
        }

        pub fn rename_no_replace(
            &self,
            source: &VerifiedHandle,
            destination_parent: &VerifiedHandle,
            destination_name: &WindowsName,
        ) -> Result<FileIdentity, StorageError> {
            let source_before = self.verify_existing(source)?;
            self.verify_parent(destination_parent)?;
            if source_before.identity.volume_serial != destination_parent.identity.volume_serial {
                return Err(unsupported());
            }
            rename_by_handle(
                source.file.as_raw_handle(),
                destination_parent.file.as_raw_handle(),
                destination_name,
            )?;
            let source_after = self.verify_existing(source)?;
            if source_after.identity != source_before.identity {
                return Err(unsupported());
            }
            Ok(source_after.identity)
        }

        pub fn read_all(&self, handle: &VerifiedHandle) -> Result<Vec<u8>, StorageError> {
            let facts = self.verify_existing(handle)?;
            if facts.kind != EntryKind::File {
                return Err(unsupported());
            }
            let mut file = handle.file.try_clone().map_err(map_io)?;
            file.seek(SeekFrom::Start(0)).map_err(map_io)?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).map_err(map_io)?;
            Ok(bytes)
        }

        fn verify_parent(&self, parent: &VerifiedHandle) -> Result<(), StorageError> {
            let facts = self.verify_existing(parent)?;
            if facts.kind != EntryKind::Directory || facts.case_sensitive {
                return Err(unsupported());
            }
            Ok(())
        }

        fn verify_new_handle(&self, file: File) -> Result<VerifiedHandle, StorageError> {
            let facts = inspect(&file)?;
            self.verify_facts(&facts)?;
            Ok(VerifiedHandle {
                file: Arc::new(file),
                identity: facts.identity,
                kind: facts.kind,
            })
        }

        fn verify_existing(&self, handle: &VerifiedHandle) -> Result<HandleFacts, StorageError> {
            let facts = inspect(&handle.file)?;
            if facts.identity != handle.identity || facts.kind != handle.kind {
                return Err(unsupported());
            }
            self.verify_facts(&facts)?;
            Ok(facts)
        }

        fn verify_facts(&self, facts: &HandleFacts) -> Result<(), StorageError> {
            if facts.identity.volume_serial != self.root_volume
                || facts.reparse
                || facts.hard_linked
                || (facts.kind == EntryKind::Directory && facts.case_sensitive)
            {
                return Err(unsupported());
            }
            let root_path = final_path(&self.root.file)?;
            if !is_within(&root_path, &facts.final_path) {
                return Err(unsupported());
            }
            Ok(())
        }
    }

    struct HandleFacts {
        identity: FileIdentity,
        kind: EntryKind,
        reparse: bool,
        hard_linked: bool,
        case_sensitive: bool,
        final_path: PathBuf,
    }

    fn inspect(file: &File) -> Result<HandleFacts, StorageError> {
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `file` owns a live handle and `information` is writable.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
            return Err(last_error());
        }
        let kind = if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            EntryKind::Directory
        } else {
            EntryKind::File
        };
        let case_sensitive = if kind == EntryKind::Directory {
            directory_is_case_sensitive(file)?
        } else {
            false
        };
        let mut identity = FILE_ID_INFO::default();
        // SAFETY: `file` is live and the fixed-size identity output is writable.
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                (&mut identity as *mut FILE_ID_INFO).cast(),
                size_of_val(&identity) as u32,
            )
        } == 0
        {
            return Err(last_error());
        }
        Ok(HandleFacts {
            identity: FileIdentity {
                volume_serial: identity.VolumeSerialNumber,
                file_id: u128::from_le_bytes(identity.FileId.Identifier),
            },
            kind,
            reparse: information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
            hard_linked: kind == EntryKind::File && information.nNumberOfLinks > 1,
            case_sensitive,
            final_path: final_path(file)?,
        })
    }

    fn directory_is_case_sensitive(file: &File) -> Result<bool, StorageError> {
        let mut information = FILE_CASE_SENSITIVE_INFO::default();
        // SAFETY: the handle denotes a directory and the fixed output structure is writable.
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileCaseSensitiveInfo,
                (&mut information as *mut FILE_CASE_SENSITIVE_INFO).cast(),
                size_of_val(&information) as u32,
            )
        } == 0
        {
            return Err(last_error());
        }
        Ok(information.Flags & FILE_CS_FLAG_CASE_SENSITIVE_DIR != 0)
    }

    fn final_path(file: &File) -> Result<PathBuf, StorageError> {
        let mut output = vec![0_u16; MAX_PATH_CHARS];
        // SAFETY: `file` is live and `output` is writable for its advertised capacity.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                output.as_mut_ptr(),
                output.len() as u32,
                0,
            )
        };
        let length = usize::try_from(length).map_err(|_| io_error())?;
        if length == 0 || length >= output.len() {
            return Err(last_error());
        }
        Ok(PathBuf::from(std::ffi::OsString::from_wide(
            &output[..length],
        )))
    }

    fn is_within(root: &Path, candidate: &Path) -> bool {
        let root = normalized_wide_path(root);
        let candidate = normalized_wide_path(candidate);
        if candidate.len() < root.len() {
            return false;
        }
        let Ok(root_len) = i32::try_from(root.len()) else {
            return false;
        };
        // SAFETY: both UTF-16 buffers are readable for `root_len` code units.
        let equal = unsafe {
            CompareStringOrdinal(
                root.as_ptr(),
                root_len,
                candidate.as_ptr(),
                root_len,
                true.into(),
            )
        } == CSTR_EQUAL;
        equal && (candidate.len() == root.len() || candidate[root.len()] == b'\\' as u16)
    }

    fn normalized_wide_path(path: &Path) -> Vec<u16> {
        let mut value: Vec<u16> = path.as_os_str().encode_wide().collect();
        for character in &mut value {
            if *character == b'/' as u16 {
                *character = b'\\' as u16;
            }
        }
        while value.last() == Some(&(b'\\' as u16)) {
            value.pop();
        }
        value
    }

    enum OpenMode {
        Existing,
        CreateFile,
        CreateDirectory,
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
    }

    fn open_relative(
        parent: *mut core::ffi::c_void,
        name: &WindowsName,
        mode: OpenMode,
    ) -> Result<File, StorageError> {
        const OBJ_CASE_INSENSITIVE: u32 = 0x0000_0040;
        const OBJ_DONT_REPARSE: u32 = 0x0000_1000;
        const FILE_OPEN: u32 = 1;
        const FILE_CREATE: u32 = 2;
        const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
        const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
        const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
        const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

        let mut encoded: Vec<u16> = name.as_str().encode_utf16().collect();
        let byte_length = encoded
            .len()
            .checked_mul(size_of::<u16>())
            .and_then(|length| u16::try_from(length).ok())
            .ok_or_else(io_error)?;
        let mut object_name = UnicodeString {
            length: byte_length,
            maximum_length: byte_length,
            buffer: encoded.as_mut_ptr(),
        };
        let mut attributes = ObjectAttributes {
            length: size_of::<ObjectAttributes>() as u32,
            root_directory: parent,
            object_name: &mut object_name,
            attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
            security_descriptor: ptr::null_mut(),
            security_quality_of_service: ptr::null_mut(),
        };
        let mut status_block = IoStatusBlock {
            status_or_pointer: 0,
            information: 0,
        };
        let mut handle = ptr::null_mut();
        let (disposition, type_option, desired_access) = match mode {
            OpenMode::Existing => (FILE_OPEN, 0, FILE_GENERIC_READ | DELETE),
            OpenMode::CreateFile => (
                FILE_CREATE,
                FILE_NON_DIRECTORY_FILE,
                FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE,
            ),
            OpenMode::CreateDirectory => (
                FILE_CREATE,
                FILE_DIRECTORY_FILE,
                FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE,
            ),
        };
        // SAFETY: all pointers reference live, correctly laid-out structures.
        // The validated name is one separator-free component interpreted
        // relative to the retained parent handle, never as a DOS path.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                desired_access,
                &mut attributes,
                &mut status_block,
                ptr::null_mut(),
                FILE_ATTRIBUTE_NORMAL,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                disposition,
                type_option | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
                ptr::null_mut(),
                0,
            )
        };
        if status < 0 || handle.is_null() {
            return Err(map_ntstatus(status));
        }
        // SAFETY: NtCreateFile returned a new owned handle on success.
        Ok(unsafe { File::from_raw_handle(handle) })
    }

    fn rename_by_handle(
        source: *mut core::ffi::c_void,
        destination_parent: *mut core::ffi::c_void,
        destination_name: &WindowsName,
    ) -> Result<(), StorageError> {
        let name: Vec<u16> = destination_name.as_str().encode_utf16().collect();
        let name_offset = offset_of!(FILE_RENAME_INFO, FileName);
        let byte_len = name_offset + name.len() * size_of::<u16>();
        let mut storage = vec![0_u64; byte_len.div_ceil(size_of::<u64>())];
        let information = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        // SAFETY: aligned backing storage is sized for the fixed header and exact name bytes.
        unsafe {
            (*information).Anonymous.ReplaceIfExists = false;
            (*information).RootDirectory = destination_parent;
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
        // SAFETY: source and destination parent handles remain live and the
        // information buffer has FILE_RENAME_INFORMATION layout.
        let status = unsafe {
            NtSetInformationFile(
                source,
                &mut status_block,
                storage.as_mut_ptr().cast(),
                byte_len as u32,
                10,
            )
        };
        if status < 0 {
            Err(map_ntstatus(status))
        } else {
            Ok(())
        }
    }

    fn map_ntstatus(status: i32) -> StorageError {
        match status as u32 {
            0xC000_0035 => StorageError::new(StorageErrorKind::Conflict),
            0xC000_0034 | 0xC000_003A => StorageError::new(StorageErrorKind::NotFound),
            0xC000_0022 | 0xC000_0043 => StorageError::new(StorageErrorKind::AccessDenied),
            _ => io_error(),
        }
    }

    fn map_io(error: std::io::Error) -> StorageError {
        match error.raw_os_error().map(|code| code as u32) {
            Some(ERROR_FILE_EXISTS | ERROR_ALREADY_EXISTS) => {
                StorageError::new(StorageErrorKind::Conflict)
            }
            Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => {
                StorageError::new(StorageErrorKind::NotFound)
            }
            Some(ERROR_ACCESS_DENIED) => StorageError::new(StorageErrorKind::AccessDenied),
            _ => io_error(),
        }
    }

    fn last_error() -> StorageError {
        // SAFETY: GetLastError has no preconditions and is read immediately after failure.
        map_io(std::io::Error::from_raw_os_error(
            unsafe { GetLastError() } as i32
        ))
    }

    fn unsupported() -> StorageError {
        StorageError::new(StorageErrorKind::Unsupported)
    }

    fn io_error() -> StorageError {
        StorageError::new(StorageErrorKind::Io)
    }
}

#[cfg(windows)]
pub use platform::{VerifiedHandle, WindowsStorage};

#[cfg(not(windows))]
mod platform_stub {
    use super::*;
    use cellar_storage::EntryKind;

    #[derive(Clone, Debug)]
    pub struct VerifiedHandle {
        identity: FileIdentity,
        kind: EntryKind,
    }

    impl VerifiedHandle {
        pub fn identity(&self) -> FileIdentity {
            self.identity
        }

        pub fn kind(&self) -> EntryKind {
            self.kind
        }
    }

    pub struct WindowsStorage;

    impl WindowsStorage {
        pub fn open(_path: &Path) -> Result<Self, StorageError> {
            Err(StorageError::new(StorageErrorKind::Unsupported))
        }

        pub fn root(&self) -> &VerifiedHandle {
            unreachable!("WindowsStorage cannot be constructed off Windows")
        }

        pub fn open_verified(
            &self,
            _parent: &VerifiedHandle,
            _name: &WindowsName,
        ) -> Result<VerifiedHandle, StorageError> {
            Err(unsupported())
        }

        pub fn create_file_no_replace(
            &self,
            _parent: &VerifiedHandle,
            _name: &WindowsName,
        ) -> Result<VerifiedHandle, StorageError> {
            Err(unsupported())
        }

        pub fn create_directory_no_replace(
            &self,
            _parent: &VerifiedHandle,
            _name: &WindowsName,
        ) -> Result<VerifiedHandle, StorageError> {
            Err(unsupported())
        }

        pub fn rename_no_replace(
            &self,
            _source: &VerifiedHandle,
            _destination_parent: &VerifiedHandle,
            _destination_name: &WindowsName,
        ) -> Result<FileIdentity, StorageError> {
            Err(unsupported())
        }

        pub fn read_all(&self, _handle: &VerifiedHandle) -> Result<Vec<u8>, StorageError> {
            Err(unsupported())
        }
    }

    fn unsupported() -> StorageError {
        StorageError::new(StorageErrorKind::Unsupported)
    }
}

#[cfg(not(windows))]
pub use platform_stub::{VerifiedHandle, WindowsStorage};

#[async_trait::async_trait]
impl Storage for WindowsStorage {
    type Handle = VerifiedHandle;

    async fn open_verified(
        &self,
        parent: &Self::Handle,
        name: &SafeName,
    ) -> Result<Self::Handle, StorageError> {
        let name = WindowsName::parse(name.as_str())
            .map_err(|_| StorageError::new(StorageErrorKind::InvalidName))?;
        #[cfg(windows)]
        {
            WindowsStorage::open_verified(self, parent, &name)
        }
        #[cfg(not(windows))]
        {
            let _ = (parent, name);
            Err(StorageError::new(StorageErrorKind::Unsupported))
        }
    }

    async fn rename_no_replace(
        &self,
        source: &Self::Handle,
        destination_parent: &Self::Handle,
        destination_name: &SafeName,
    ) -> Result<FileIdentity, StorageError> {
        let name = WindowsName::parse(destination_name.as_str())
            .map_err(|_| StorageError::new(StorageErrorKind::InvalidName))?;
        #[cfg(windows)]
        {
            WindowsStorage::rename_no_replace(self, source, destination_parent, &name)
        }
        #[cfg(not(windows))]
        {
            let _ = (source, destination_parent, name);
            Err(StorageError::new(StorageErrorKind::Unsupported))
        }
    }
}
