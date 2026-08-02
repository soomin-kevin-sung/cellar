use cellar_storage::{FileIdentity, SafeName, Storage, StorageError, StorageErrorKind};

use crate::WindowsName;

#[cfg(windows)]
mod platform {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::mem::{offset_of, size_of, size_of_val};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::{Path, PathBuf};
    use std::ptr;
    use std::sync::Arc;

    use cellar_storage::{EntryKind, FileIdentity, StorageError, StorageErrorKind};
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_DISK_FULL, ERROR_FILE_EXISTS,
        ERROR_FILE_NOT_FOUND, ERROR_HANDLE_DISK_FULL, ERROR_PATH_NOT_FOUND, GetLastError,
    };
    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_CASE_SENSITIVE_INFO, FILE_GENERIC_READ,
        FILE_GENERIC_WRITE, FILE_ID_INFO, FILE_RENAME_INFO, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FileCaseSensitiveInfo, FileDispositionInfo, FileIdInfo, GetDiskFreeSpaceExW,
        GetFileInformationByHandle, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
        SetFileInformationByHandle,
    };

    use crate::WindowsName;
    use crate::preflight::{StorageIdentity, TrustedRootHandle};

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

    #[derive(Clone)]
    pub struct WindowsStorage {
        root: VerifiedHandle,
        root_volume: u64,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct VerifiedFileMetadata {
        pub identity: FileIdentity,
        pub length: u64,
        pub mtime_filetime_100ns: i64,
    }

    impl WindowsStorage {
        pub fn adopt(identity: StorageIdentity<TrustedRootHandle>) -> Result<Self, StorageError> {
            let (preflight_coordinates, trusted_root) = identity.into_parts();
            let file = trusted_root.into_file();
            let facts = inspect(&file)?;
            if facts.kind != EntryKind::Directory || facts.reparse || facts.case_sensitive {
                return Err(unsupported());
            }
            if facts.identity.volume_serial != preflight_coordinates.volume_serial
                || facts.identity.file_id != preflight_coordinates.root_file_id
            {
                return Err(unsupported());
            }
            let root = VerifiedHandle {
                file,
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

        pub fn open_verified_writable(
            &self,
            parent: &VerifiedHandle,
            name: &WindowsName,
        ) -> Result<VerifiedHandle, StorageError> {
            self.verify_parent(parent)?;
            let file = open_relative(
                parent.file.as_raw_handle(),
                name,
                OpenMode::ExistingWritable,
            )?;
            self.verify_new_handle(file)
        }

        pub fn open_download_verified(
            &self,
            parent: &VerifiedHandle,
            name: &WindowsName,
        ) -> Result<VerifiedHandle, StorageError> {
            self.verify_parent(parent)?;
            let file = open_relative(
                parent.file.as_raw_handle(),
                name,
                OpenMode::DownloadExisting,
            )?;
            let handle = self.verify_new_handle(file)?;
            if handle.kind != EntryKind::File {
                return Err(unsupported());
            }
            Ok(handle)
        }

        pub fn download_metadata(
            &self,
            handle: &VerifiedHandle,
        ) -> Result<VerifiedFileMetadata, StorageError> {
            let facts = self.verify_existing(handle)?;
            if facts.kind != EntryKind::File {
                return Err(unsupported());
            }
            Ok(VerifiedFileMetadata {
                identity: facts.identity,
                length: facts.length,
                mtime_filetime_100ns: facts.mtime_filetime_100ns,
            })
        }

        pub fn read_download_exact(
            &self,
            handle: &VerifiedHandle,
            offset: u64,
            length: usize,
        ) -> Result<Vec<u8>, StorageError> {
            self.download_metadata(handle)?;
            let mut file = handle.file.try_clone().map_err(map_io)?;
            file.seek(SeekFrom::Start(offset)).map_err(map_io)?;
            let mut bytes = vec![0_u8; length];
            file.read_exact(&mut bytes).map_err(map_io)?;
            Ok(bytes)
        }

        /// Opens a stable publication source with read/delete access while
        /// denying all concurrent write and delete opens.
        pub fn open_verified_for_publication(
            &self,
            parent: &VerifiedHandle,
            name: &WindowsName,
        ) -> Result<VerifiedHandle, StorageError> {
            self.verify_parent(parent)?;
            let file = open_relative(
                parent.file.as_raw_handle(),
                name,
                OpenMode::ExistingPublication,
            )?;
            self.verify_new_handle(file)
        }

        pub fn create_file_no_replace(
            &self,
            parent: &VerifiedHandle,
            name: &WindowsName,
        ) -> Result<VerifiedHandle, StorageError> {
            self.verify_parent(parent)?;
            let file = open_relative(parent.file.as_raw_handle(), name, OpenMode::CreateFile)?;
            self.verify_created_handle(file)
        }

        pub fn create_staging_file_no_replace(
            &self,
            parent: &VerifiedHandle,
            name: &WindowsName,
        ) -> Result<VerifiedHandle, StorageError> {
            self.verify_parent(parent)?;
            let file = open_relative(
                parent.file.as_raw_handle(),
                name,
                OpenMode::CreateExclusiveFile,
            )?;
            self.verify_created_handle(file)
        }

        pub fn create_directory_no_replace(
            &self,
            parent: &VerifiedHandle,
            name: &WindowsName,
        ) -> Result<VerifiedHandle, StorageError> {
            self.verify_parent(parent)?;
            let file = open_relative(parent.file.as_raw_handle(), name, OpenMode::CreateDirectory)?;
            self.verify_created_handle(file)
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

        pub fn file_length(&self, handle: &VerifiedHandle) -> Result<i64, StorageError> {
            let facts = self.verify_existing(handle)?;
            if facts.kind != EntryKind::File {
                return Err(unsupported());
            }
            i64::try_from(handle.file.metadata().map_err(map_io)?.len()).map_err(|_| io_error())
        }

        pub fn flush_file(&self, handle: &VerifiedHandle) -> Result<(), StorageError> {
            let facts = self.verify_existing(handle)?;
            if facts.kind != EntryKind::File {
                return Err(unsupported());
            }
            handle.file.sync_all().map_err(map_io)
        }

        pub fn file_length_and_mtime(
            &self,
            handle: &VerifiedHandle,
        ) -> Result<(i64, i64), StorageError> {
            let facts = self.verify_existing(handle)?;
            if facts.kind != EntryKind::File {
                return Err(unsupported());
            }
            let mut information = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
            // SAFETY: the verified file handle is live and `information` is writable.
            if unsafe { GetFileInformationByHandle(handle.file.as_raw_handle(), &mut information) }
                == 0
            {
                return Err(last_error());
            }
            let size =
                (u64::from(information.nFileSizeHigh) << 32) | u64::from(information.nFileSizeLow);
            let mtime = (u64::from(information.ftLastWriteTime.dwHighDateTime) << 32)
                | u64::from(information.ftLastWriteTime.dwLowDateTime);
            Ok((
                i64::try_from(size).map_err(|_| io_error())?,
                i64::try_from(mtime).map_err(|_| io_error())?,
            ))
        }

        pub fn read_exact_at(
            &self,
            handle: &VerifiedHandle,
            offset: i64,
            length: i64,
        ) -> Result<Vec<u8>, StorageError> {
            self.verify_existing(handle)?;
            let offset = u64::try_from(offset).map_err(|_| io_error())?;
            let length = usize::try_from(length).map_err(|_| io_error())?;
            let mut file = handle.file.try_clone().map_err(map_io)?;
            file.seek(SeekFrom::Start(offset)).map_err(map_io)?;
            let mut bytes = vec![0; length];
            file.read_exact(&mut bytes).map_err(map_io)?;
            Ok(bytes)
        }

        pub fn truncate_file(
            &self,
            handle: &VerifiedHandle,
            length: i64,
        ) -> Result<(), StorageError> {
            self.verify_existing(handle)?;
            let length = u64::try_from(length).map_err(|_| io_error())?;
            handle.file.set_len(length).map_err(map_io)?;
            handle.file.sync_all().map_err(map_io)
        }

        pub fn write_exact_at_and_flush(
            &self,
            handle: &VerifiedHandle,
            offset: i64,
            bytes: &[u8],
        ) -> Result<(), StorageError> {
            self.verify_existing(handle)?;
            let offset = u64::try_from(offset).map_err(|_| io_error())?;
            let mut file = handle.file.try_clone().map_err(map_io)?;
            file.seek(SeekFrom::Start(offset)).map_err(map_io)?;
            file.write_all(bytes).map_err(map_io)?;
            file.sync_all().map_err(map_io)
        }

        pub fn remove_file(&self, handle: VerifiedHandle) -> Result<(), StorageError> {
            let facts = self.verify_existing(&handle)?;
            if facts.kind != EntryKind::File {
                return Err(unsupported());
            }
            let information =
                windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO { DeleteFile: true };
            // SAFETY: the retained verified handle has DELETE access and the
            // information value has the exact FileDispositionInfo layout.
            if unsafe {
                SetFileInformationByHandle(
                    handle.file.as_raw_handle(),
                    FileDispositionInfo,
                    (&raw const information).cast(),
                    size_of_val(&information) as u32,
                )
            } == 0
            {
                return Err(last_error());
            }
            drop(handle);
            Ok(())
        }

        pub fn available_space(&self) -> Result<i64, StorageError> {
            let path = final_path(&self.root.file)?;
            let mut wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let mut available = 0_u64;
            // SAFETY: `wide` is a live NUL-terminated path and `available` is writable.
            if unsafe {
                GetDiskFreeSpaceExW(
                    wide.as_mut_ptr(),
                    &mut available,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            } == 0
            {
                return Err(last_error());
            }
            i64::try_from(available).map_err(|_| io_error())
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

        fn verify_created_handle(&self, file: File) -> Result<VerifiedHandle, StorageError> {
            let (file, facts) = verify_created_with_cleanup(
                file,
                |file| {
                    let facts = inspect(file)?;
                    self.verify_facts(&facts)?;
                    Ok(facts)
                },
                delete_created,
            )?;
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
            if facts.identity.volume_serial != self.root_volume {
                return Err(unsupported());
            }
            reject_unsupported_characteristics(
                facts.reparse,
                facts.hard_linked,
                facts.kind == EntryKind::Directory && facts.case_sensitive,
            )?;
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
        length: u64,
        mtime_filetime_100ns: i64,
    }

    pub(super) fn reject_unsupported_characteristics(
        reparse: bool,
        hard_linked: bool,
        case_sensitive_directory: bool,
    ) -> Result<(), StorageError> {
        if reparse || hard_linked || case_sensitive_directory {
            Err(unsupported())
        } else {
            Ok(())
        }
    }

    pub(super) fn verify_created_with_cleanup<T, V>(
        owned: T,
        verify: impl FnOnce(&T) -> Result<V, StorageError>,
        cleanup: impl FnOnce(T) -> Result<(), StorageError>,
    ) -> Result<(T, V), StorageError> {
        match verify(&owned) {
            Ok(verified) => Ok((owned, verified)),
            Err(verification_error) => match cleanup(owned) {
                Ok(()) => Err(verification_error),
                Err(_) => Err(StorageError::new(StorageErrorKind::CleanupFailed)),
            },
        }
    }

    fn delete_created(file: File) -> Result<(), StorageError> {
        let information =
            windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: the exact newly-created owned handle has DELETE access and
        // `information` matches FileDispositionInfo for this synchronous call.
        let result = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfo,
                (&raw const information).cast(),
                size_of_val(&information) as u32,
            )
        };
        let failure = (result == 0).then(last_error);
        drop(file);
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
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
            length: u64::from(information.nFileSizeHigh) << 32
                | u64::from(information.nFileSizeLow),
            mtime_filetime_100ns: i64::try_from(
                u64::from(information.ftLastWriteTime.dwHighDateTime) << 32
                    | u64::from(information.ftLastWriteTime.dwLowDateTime),
            )
            .map_err(|_| io_error())?,
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

    #[derive(Clone, Copy)]
    enum OpenMode {
        Existing,
        ExistingWritable,
        DownloadExisting,
        ExistingPublication,
        CreateFile,
        CreateExclusiveFile,
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
        let (disposition, type_option, desired_access, share_access) = match mode {
            OpenMode::Existing => (
                FILE_OPEN,
                0,
                FILE_GENERIC_READ | DELETE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
            ),
            OpenMode::ExistingWritable => (
                FILE_OPEN,
                0,
                FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE,
                FILE_SHARE_READ,
            ),
            OpenMode::DownloadExisting => (
                FILE_OPEN,
                FILE_NON_DIRECTORY_FILE,
                FILE_GENERIC_READ,
                FILE_SHARE_READ,
            ),
            OpenMode::ExistingPublication => (
                FILE_OPEN,
                FILE_NON_DIRECTORY_FILE,
                FILE_GENERIC_READ | DELETE,
                FILE_SHARE_READ,
            ),
            OpenMode::CreateFile => (
                FILE_CREATE,
                FILE_NON_DIRECTORY_FILE,
                FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
            ),
            OpenMode::CreateExclusiveFile => (
                FILE_CREATE,
                FILE_NON_DIRECTORY_FILE,
                FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE,
                FILE_SHARE_READ,
            ),
            OpenMode::CreateDirectory => (
                FILE_CREATE,
                FILE_DIRECTORY_FILE,
                FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
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
                share_access,
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
            Some(ERROR_DISK_FULL | ERROR_HANDLE_DISK_FULL) => {
                StorageError::new(StorageErrorKind::InsufficientStorage)
            }
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
pub use platform::{VerifiedFileMetadata, VerifiedHandle, WindowsStorage};

#[cfg(not(windows))]
mod platform_stub {
    use super::*;
    use crate::preflight::{StorageIdentity, TrustedRootHandle};
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

    #[derive(Clone)]
    pub struct WindowsStorage {
        _private: (),
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct VerifiedFileMetadata {
        pub identity: FileIdentity,
        pub length: u64,
        pub mtime_filetime_100ns: i64,
    }

    impl WindowsStorage {
        pub fn adopt(_identity: StorageIdentity<TrustedRootHandle>) -> Result<Self, StorageError> {
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

        pub fn open_download_verified(
            &self,
            _parent: &VerifiedHandle,
            _name: &WindowsName,
        ) -> Result<VerifiedHandle, StorageError> {
            Err(unsupported())
        }

        pub fn download_metadata(
            &self,
            _handle: &VerifiedHandle,
        ) -> Result<VerifiedFileMetadata, StorageError> {
            Err(unsupported())
        }

        pub fn read_download_exact(
            &self,
            _handle: &VerifiedHandle,
            _offset: u64,
            _length: usize,
        ) -> Result<Vec<u8>, StorageError> {
            Err(unsupported())
        }

        pub fn create_file_no_replace(
            &self,
            _parent: &VerifiedHandle,
            _name: &WindowsName,
        ) -> Result<VerifiedHandle, StorageError> {
            Err(unsupported())
        }

        pub fn create_staging_file_no_replace(
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

        pub fn open_verified_writable(
            &self,
            _parent: &VerifiedHandle,
            _name: &WindowsName,
        ) -> Result<VerifiedHandle, StorageError> {
            Err(unsupported())
        }

        pub fn open_verified_for_publication(
            &self,
            _parent: &VerifiedHandle,
            _name: &WindowsName,
        ) -> Result<VerifiedHandle, StorageError> {
            Err(unsupported())
        }

        pub fn file_length(&self, _handle: &VerifiedHandle) -> Result<i64, StorageError> {
            Err(unsupported())
        }
        pub fn flush_file(&self, _handle: &VerifiedHandle) -> Result<(), StorageError> {
            Err(unsupported())
        }
        pub fn file_length_and_mtime(
            &self,
            _handle: &VerifiedHandle,
        ) -> Result<(i64, i64), StorageError> {
            Err(unsupported())
        }
        pub fn read_exact_at(
            &self,
            _handle: &VerifiedHandle,
            _offset: i64,
            _length: i64,
        ) -> Result<Vec<u8>, StorageError> {
            Err(unsupported())
        }
        pub fn truncate_file(
            &self,
            _handle: &VerifiedHandle,
            _length: i64,
        ) -> Result<(), StorageError> {
            Err(unsupported())
        }
        pub fn write_exact_at_and_flush(
            &self,
            _handle: &VerifiedHandle,
            _offset: i64,
            _bytes: &[u8],
        ) -> Result<(), StorageError> {
            Err(unsupported())
        }
        pub fn remove_file(&self, _handle: VerifiedHandle) -> Result<(), StorageError> {
            Err(unsupported())
        }
        pub fn available_space(&self) -> Result<i64, StorageError> {
            Err(unsupported())
        }
    }

    fn unsupported() -> StorageError {
        StorageError::new(StorageErrorKind::Unsupported)
    }
}

#[cfg(not(windows))]
pub use platform_stub::{VerifiedFileMetadata, VerifiedHandle, WindowsStorage};

async fn dispatch_blocking<T>(
    operation: impl FnOnce() -> Result<T, StorageError> + Send + 'static,
) -> Result<T, StorageError>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| StorageError::new(StorageErrorKind::WorkerFailed))?
}

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
        let storage = self.clone();
        let parent = parent.clone();
        dispatch_blocking(move || WindowsStorage::open_verified(&storage, &parent, &name)).await
    }

    async fn rename_no_replace(
        &self,
        source: &Self::Handle,
        destination_parent: &Self::Handle,
        destination_name: &SafeName,
    ) -> Result<FileIdentity, StorageError> {
        let name = WindowsName::parse(destination_name.as_str())
            .map_err(|_| StorageError::new(StorageErrorKind::InvalidName))?;
        let storage = self.clone();
        let source = source.clone();
        let destination_parent = destination_parent.clone();
        dispatch_blocking(move || {
            WindowsStorage::rename_no_replace(&storage, &source, &destination_parent, &name)
        })
        .await
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::cell::Cell;

    use cellar_storage::{StorageError, StorageErrorKind};

    use super::dispatch_blocking;
    use super::platform::{reject_unsupported_characteristics, verify_created_with_cleanup};

    #[tokio::test(flavor = "current_thread")]
    async fn async_storage_dispatches_sync_work_off_the_runtime_thread() {
        let runtime_thread = std::thread::current().id();

        let worker_thread = dispatch_blocking(|| Ok(std::thread::current().id()))
            .await
            .unwrap();

        assert_ne!(worker_thread, runtime_thread);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_worker_failure_is_stable_and_redacted() {
        let error =
            dispatch_blocking(|| -> Result<(), StorageError> { panic!("private worker detail") })
                .await
                .unwrap_err();

        assert_eq!(error.kind(), StorageErrorKind::WorkerFailed);
        assert_eq!(error.code(), "worker_failed");
        assert!(!error.to_string().contains("private worker detail"));
    }

    #[test]
    fn verification_failure_deletes_the_exact_owned_creation() {
        let cleaned = Cell::new(None);
        let error = verify_created_with_cleanup(
            73_u32,
            |_| Err::<(), _>(StorageError::new(StorageErrorKind::Unsupported)),
            |owned| {
                cleaned.set(Some(owned));
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(error.kind(), StorageErrorKind::Unsupported);
        assert_eq!(cleaned.get(), Some(73));
    }

    #[test]
    fn cleanup_failure_returns_distinct_fail_closed_evidence() {
        let error = verify_created_with_cleanup(
            91_u32,
            |_| Err::<(), _>(StorageError::new(StorageErrorKind::Unsupported)),
            |_| Err(StorageError::new(StorageErrorKind::AccessDenied)),
        )
        .unwrap_err();

        assert_eq!(error.kind(), StorageErrorKind::CleanupFailed);
        assert_eq!(error.code(), "cleanup_failed");
    }

    #[test]
    fn case_sensitive_directory_flag_is_always_rejected_by_the_pure_seam() {
        let error = reject_unsupported_characteristics(false, false, true).unwrap_err();
        assert_eq!(error.kind(), StorageErrorKind::Unsupported);
    }
}
