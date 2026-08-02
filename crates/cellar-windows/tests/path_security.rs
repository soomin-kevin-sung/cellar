use cellar_windows::WindowsName;

#[test]
fn rejects_dangerous_windows_names() {
    for name in [
        "..",
        ".",
        r"..\escape",
        r"C:\escape",
        r"\\server\share",
        "nul\0byte",
        "file.txt:secret",
        "CON",
        "con.txt",
        "COM1.log",
        "COM\u{00B9}.log",
        "LPT\u{00B2}",
        "LPT9",
        "trailing.",
        "trailing ",
        "a/b",
        r"a\b",
    ] {
        assert!(WindowsName::parse(name).is_err(), "accepted {name:?}");
    }
}

#[test]
fn preserves_exact_valid_windows_names() {
    for name in [
        "Report 2026.txt",
        "\u{CF00}\u{B7EC}-File_01.PDF",
        "auxiliary.txt",
        "COM10",
        ".env",
    ] {
        let parsed = WindowsName::parse(name).unwrap();
        assert_eq!(parsed.as_str(), name);
    }
}

#[test]
fn enforces_windows_utf16_component_and_control_character_limits() {
    assert!(WindowsName::parse(format!("{}x", "a".repeat(255))).is_err());
    let maximum = "a".repeat(255);
    assert_eq!(WindowsName::parse(&maximum).unwrap().as_str(), maximum);
    assert!(WindowsName::parse("control\u{001F}name").is_err());
}

#[cfg(windows)]
mod windows {
    use std::fs;
    use std::os::windows::fs::{OpenOptionsExt, symlink_dir};

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_CASE_SENSITIVE_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FileCaseSensitiveInfo, SetFileInformationByHandle,
    };

    use cellar_storage::{SafeName, Storage};
    use cellar_windows::{StorageErrorKind, WindowsStorage};
    use tempfile::tempdir;

    use super::WindowsName;

    #[test]
    fn create_no_replace_preserves_existing_destination() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("present.txt"), b"owner").unwrap();
        let storage = WindowsStorage::open(directory.path()).unwrap();

        let error = storage
            .create_file_no_replace(storage.root(), &name("present.txt"))
            .unwrap_err();

        assert_eq!(error.kind(), StorageErrorKind::Conflict);
        assert_eq!(
            fs::read(directory.path().join("present.txt")).unwrap(),
            b"owner"
        );
    }

    #[test]
    fn rename_no_replace_preserves_both_files_on_conflict() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("source.txt"), b"source").unwrap();
        fs::write(directory.path().join("destination.txt"), b"destination").unwrap();
        let storage = WindowsStorage::open(directory.path()).unwrap();
        let source = storage
            .open_verified(storage.root(), &name("source.txt"))
            .unwrap();

        let error = storage
            .rename_no_replace(&source, storage.root(), &name("destination.txt"))
            .unwrap_err();

        assert_eq!(error.kind(), StorageErrorKind::Conflict);
        assert_eq!(
            fs::read(directory.path().join("source.txt")).unwrap(),
            b"source"
        );
        assert_eq!(
            fs::read(directory.path().join("destination.txt")).unwrap(),
            b"destination"
        );
    }

    #[test]
    fn retained_root_handle_blocks_path_replacement() {
        let parent = tempdir().unwrap();
        let configured = parent.path().join("configured");
        let retained = parent.path().join("retained");
        fs::create_dir(&configured).unwrap();
        let storage = WindowsStorage::open(&configured).unwrap();
        assert!(fs::rename(&configured, &retained).is_err());
        storage
            .create_file_no_replace(storage.root(), &name("trusted.txt"))
            .unwrap();
        assert!(configured.join("trusted.txt").exists());
        assert!(!retained.exists());
    }

    #[test]
    fn rejects_hard_link_aliases_before_mutation() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("original.txt"), b"owner").unwrap();
        fs::hard_link(
            directory.path().join("original.txt"),
            directory.path().join("alias.txt"),
        )
        .unwrap();
        let storage = WindowsStorage::open(directory.path()).unwrap();

        let error = storage
            .open_verified(storage.root(), &name("original.txt"))
            .unwrap_err();

        assert_eq!(error.kind(), StorageErrorKind::Unsupported);
        assert_eq!(
            fs::read(directory.path().join("alias.txt")).unwrap(),
            b"owner"
        );
    }

    #[test]
    fn rejects_reparse_points_that_leave_the_root_when_supported() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("root");
        let outside = parent.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        let link = root.join("escape");
        if symlink_dir(&outside, &link).is_err() {
            let output = std::process::Command::new("cmd")
                .arg("/c")
                .arg("mklink")
                .arg("/J")
                .arg(&link)
                .arg(&outside)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "could not create a test reparse point"
            );
        }
        let storage = WindowsStorage::open(&root).unwrap();

        let error = storage
            .open_verified(storage.root(), &name("escape"))
            .unwrap_err();

        assert_eq!(error.kind(), StorageErrorKind::Unsupported);
    }

    #[test]
    fn opens_nested_components_only_from_verified_directory_handles() {
        let directory = tempdir().unwrap();
        fs::create_dir(directory.path().join("folder")).unwrap();
        fs::write(
            directory.path().join("folder").join("inside.txt"),
            b"inside",
        )
        .unwrap();
        let storage = WindowsStorage::open(directory.path()).unwrap();
        let folder = storage
            .open_verified(storage.root(), &name("folder"))
            .unwrap();
        let inside = storage.open_verified(&folder, &name("inside.txt")).unwrap();

        assert_eq!(storage.read_all(&inside).unwrap(), b"inside");
    }

    #[tokio::test]
    async fn storage_port_revalidates_platform_rules() {
        let directory = tempdir().unwrap();
        let storage = WindowsStorage::open(directory.path()).unwrap();
        let ads = SafeName::parse("file.txt:stream").unwrap();

        let error = Storage::open_verified(&storage, storage.root(), &ads)
            .await
            .unwrap_err();

        assert_eq!(error.kind(), StorageErrorKind::InvalidName);
    }

    #[test]
    fn verified_source_handle_blocks_namespace_replacement() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("source.txt"), b"original").unwrap();
        let storage = WindowsStorage::open(directory.path()).unwrap();
        let source = storage
            .open_verified(storage.root(), &name("source.txt"))
            .unwrap();

        assert!(
            fs::rename(
                directory.path().join("source.txt"),
                directory.path().join("moved.txt")
            )
            .is_err()
        );
        assert_eq!(storage.read_all(&source).unwrap(), b"original");
    }

    #[test]
    fn create_directory_no_replace_preserves_existing_directory() {
        let directory = tempdir().unwrap();
        fs::create_dir(directory.path().join("present")).unwrap();
        fs::write(directory.path().join("present").join("owner.txt"), b"owner").unwrap();
        let storage = WindowsStorage::open(directory.path()).unwrap();

        let error = storage
            .create_directory_no_replace(storage.root(), &name("present"))
            .unwrap_err();

        assert_eq!(error.kind(), StorageErrorKind::Conflict);
        assert_eq!(
            fs::read(directory.path().join("present").join("owner.txt")).unwrap(),
            b"owner"
        );
    }

    #[test]
    fn rejects_case_sensitive_directory_subtrees_when_supported() {
        let directory = tempdir().unwrap();
        let child = directory.path().join("sensitive");
        fs::create_dir(&child).unwrap();
        if !enable_case_sensitivity(&child) {
            return;
        }
        let storage = WindowsStorage::open(directory.path()).unwrap();

        let error = storage
            .open_verified(storage.root(), &name("sensitive"))
            .unwrap_err();

        assert_eq!(error.kind(), StorageErrorKind::Unsupported);
    }

    #[test]
    fn handle_relative_creation_supports_paths_beyond_legacy_max_path() {
        let directory = tempdir().unwrap();
        let storage = WindowsStorage::open(directory.path()).unwrap();
        let component = name(&"n".repeat(100));
        let mut parent = storage.root().clone();
        for _ in 0..30 {
            parent = storage
                .create_directory_no_replace(&parent, &component)
                .unwrap();
        }
        let leaf = storage
            .create_file_no_replace(&parent, &name("leaf.txt"))
            .unwrap();
        assert_eq!(leaf.kind(), cellar_storage::EntryKind::File);
        drop(leaf);
        drop(parent);
        drop(storage);
    }

    fn enable_case_sensitivity(path: &std::path::Path) -> bool {
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .unwrap();
        let information = FILE_CASE_SENSITIVE_INFO { Flags: 1 };
        // SAFETY: the handle is live and the fixed-size input structure is readable.
        unsafe {
            SetFileInformationByHandle(
                std::os::windows::io::AsRawHandle::as_raw_handle(&file),
                FileCaseSensitiveInfo,
                (&raw const information).cast(),
                std::mem::size_of_val(&information) as u32,
            ) != 0
        }
    }

    fn name(value: &str) -> WindowsName {
        WindowsName::parse(value).unwrap()
    }
}
