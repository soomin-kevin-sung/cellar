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
        "CONIN$",
        "conout$.txt",
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
    use std::sync::{Arc, Barrier};

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
        let storage = adopted_storage(directory.path());
        fs::write(directory.path().join("present.txt"), b"owner").unwrap();

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
    fn adapter_adopts_the_exact_preflight_root_identity() {
        let directory = tempdir().unwrap();
        let identity = cellar_windows::preflight::run_as_service(directory.path()).unwrap();
        let coordinates = identity.coordinates();

        let storage = WindowsStorage::adopt(identity).unwrap();

        assert_eq!(
            storage.root().identity().volume_serial,
            coordinates.volume_serial
        );
        assert_eq!(storage.root().identity().file_id, coordinates.root_file_id);
    }

    #[test]
    fn download_handle_blocks_writers_and_delete_sharing_until_drop() {
        let directory = tempdir().unwrap();
        let storage = adopted_storage(directory.path());
        fs::write(directory.path().join("payload.bin"), b"abcdef").unwrap();
        let handle = storage
            .open_download_verified(storage.root(), &name("payload.bin"))
            .unwrap();
        let metadata = storage.download_metadata(&handle).unwrap();
        assert_eq!(metadata.length, 6);
        assert_eq!(storage.read_download_exact(&handle, 1, 3).unwrap(), b"bcd");

        let writer = fs::OpenOptions::new()
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(directory.path().join("payload.bin"));
        assert!(writer.is_err());
        drop(handle);
        assert!(
            fs::OpenOptions::new()
                .write(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .open(directory.path().join("payload.bin"))
                .is_ok()
        );
    }

    #[test]
    fn rename_no_replace_preserves_both_files_on_conflict() {
        let directory = tempdir().unwrap();
        let storage = adopted_storage(directory.path());
        fs::write(directory.path().join("source.txt"), b"source").unwrap();
        fs::write(directory.path().join("destination.txt"), b"destination").unwrap();
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
    fn successful_rename_preserves_full_file_identity() {
        let directory = tempdir().unwrap();
        let storage = adopted_storage(directory.path());
        fs::write(directory.path().join("source.txt"), b"source").unwrap();
        let source = storage
            .open_verified(storage.root(), &name("source.txt"))
            .unwrap();
        let source_identity = source.identity();

        let renamed_identity = storage
            .rename_no_replace(&source, storage.root(), &name("destination.txt"))
            .unwrap();
        drop(source);
        let destination = storage
            .open_verified(storage.root(), &name("destination.txt"))
            .unwrap();

        assert_eq!(renamed_identity, source_identity);
        assert_eq!(destination.identity(), source_identity);
        assert_eq!(storage.read_all(&destination).unwrap(), b"source");
        assert!(!directory.path().join("source.txt").exists());
    }

    #[test]
    fn destination_create_race_has_exactly_one_winner_without_loss() {
        let directory = tempdir().unwrap();
        let storage = Arc::new(adopted_storage(directory.path()));

        for iteration in 0..32 {
            let source_name = format!("source-{iteration}.txt");
            let destination_name = format!("destination-{iteration}.txt");
            let source_path = directory.path().join(&source_name);
            let destination_path = directory.path().join(&destination_name);
            fs::write(&source_path, b"source").unwrap();
            let source = storage
                .open_verified(storage.root(), &name(&source_name))
                .unwrap();
            let barrier = Arc::new(Barrier::new(3));

            let create_barrier = Arc::clone(&barrier);
            let create_destination = destination_path.clone();
            let create = std::thread::spawn(move || {
                create_barrier.wait();
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(create_destination)
                    .and_then(|mut file| std::io::Write::write_all(&mut file, b"external"))
            });

            let rename_barrier = Arc::clone(&barrier);
            let rename_storage = Arc::clone(&storage);
            let rename_destination = name(&destination_name);
            let rename = std::thread::spawn(move || {
                rename_barrier.wait();
                rename_storage.rename_no_replace(
                    &source,
                    rename_storage.root(),
                    &rename_destination,
                )
            });

            barrier.wait();
            let create_won = create.join().unwrap().is_ok();
            let rename_won = rename.join().unwrap().is_ok();
            assert_ne!(create_won, rename_won, "iteration {iteration}");

            if create_won {
                assert_eq!(fs::read(&destination_path).unwrap(), b"external");
                assert_eq!(fs::read(&source_path).unwrap(), b"source");
            } else {
                assert_eq!(fs::read(&destination_path).unwrap(), b"source");
                assert!(!source_path.exists());
            }
        }
    }

    #[test]
    fn retained_root_handle_blocks_path_replacement() {
        let parent = tempdir().unwrap();
        let configured = parent.path().join("configured");
        let retained = parent.path().join("retained");
        fs::create_dir(&configured).unwrap();
        let storage = adopted_storage(&configured);
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
        let storage = adopted_storage(directory.path());
        fs::write(directory.path().join("original.txt"), b"owner").unwrap();
        fs::hard_link(
            directory.path().join("original.txt"),
            directory.path().join("alias.txt"),
        )
        .unwrap();
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
        let storage = adopted_storage(&root);
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
        let error = storage
            .open_verified(storage.root(), &name("escape"))
            .unwrap_err();

        assert_eq!(error.kind(), StorageErrorKind::Unsupported);
    }

    #[test]
    fn opens_nested_components_only_from_verified_directory_handles() {
        let directory = tempdir().unwrap();
        let storage = adopted_storage(directory.path());
        fs::create_dir(directory.path().join("folder")).unwrap();
        fs::write(
            directory.path().join("folder").join("inside.txt"),
            b"inside",
        )
        .unwrap();
        let folder = storage
            .open_verified(storage.root(), &name("folder"))
            .unwrap();
        let inside = storage.open_verified(&folder, &name("inside.txt")).unwrap();

        assert_eq!(storage.read_all(&inside).unwrap(), b"inside");
    }

    #[tokio::test]
    async fn storage_port_revalidates_platform_rules() {
        let directory = tempdir().unwrap();
        let storage = adopted_storage(directory.path());
        let ads = SafeName::parse("file.txt:stream").unwrap();

        let error = Storage::open_verified(&storage, storage.root(), &ads)
            .await
            .unwrap_err();

        assert_eq!(error.kind(), StorageErrorKind::InvalidName);
    }

    #[test]
    fn verified_source_handle_blocks_namespace_replacement() {
        let directory = tempdir().unwrap();
        let storage = adopted_storage(directory.path());
        fs::write(directory.path().join("source.txt"), b"original").unwrap();
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
        let storage = adopted_storage(directory.path());
        fs::create_dir(directory.path().join("present")).unwrap();
        fs::write(directory.path().join("present").join("owner.txt"), b"owner").unwrap();

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
        let storage = adopted_storage(directory.path());
        let child = directory.path().join("sensitive");
        fs::create_dir(&child).unwrap();
        if let Err(error) = enable_case_sensitivity(&child) {
            eprintln!(
                "acceptance skipped: this Windows/NTFS environment cannot enable case sensitivity: {error}"
            );
            return;
        }
        let error = storage
            .open_verified(storage.root(), &name("sensitive"))
            .unwrap_err();

        assert_eq!(error.kind(), StorageErrorKind::Unsupported);
    }

    #[test]
    fn handle_relative_creation_supports_paths_beyond_legacy_max_path() {
        let directory = tempdir().unwrap();
        let storage = adopted_storage(directory.path());
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

    fn enable_case_sensitivity(path: &std::path::Path) -> std::io::Result<()> {
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .unwrap();
        let information = FILE_CASE_SENSITIVE_INFO { Flags: 1 };
        // SAFETY: the handle is live and the fixed-size input structure is readable.
        let result = unsafe {
            SetFileInformationByHandle(
                std::os::windows::io::AsRawHandle::as_raw_handle(&file),
                FileCaseSensitiveInfo,
                (&raw const information).cast(),
                std::mem::size_of_val(&information) as u32,
            )
        };
        if result == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn name(value: &str) -> WindowsName {
        WindowsName::parse(value).unwrap()
    }

    fn adopted_storage(path: &std::path::Path) -> WindowsStorage {
        let identity = cellar_windows::preflight::run_as_service(path).unwrap();
        WindowsStorage::adopt(identity).unwrap()
    }
}
