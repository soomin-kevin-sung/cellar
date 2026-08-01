use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::Mutex;

use cellar_windows::preflight::{
    AdapterError, Filesystem, PreflightAdapter, PreflightError, ProbeName, RootAttributes,
    RootInspection, StorageCoordinates, VolumeKind, preflight_with,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Failure {
    Inspect,
    Create,
    Write,
    Flush,
    Rename,
    Delete,
    Identity,
}

#[derive(Default)]
struct FakeState {
    events: Vec<&'static str>,
    entries: HashSet<String>,
    contents: Vec<u8>,
    failures: VecDeque<Failure>,
    create_collisions: usize,
    rename_collisions: usize,
    mismatch: bool,
}

struct FakeAdapter {
    inspection: RootInspection,
    state: Mutex<FakeState>,
}

impl FakeAdapter {
    fn allowed() -> Self {
        Self {
            inspection: RootInspection {
                filesystem: Filesystem::Ntfs,
                volume_kind: VolumeKind::FixedLocal,
                attributes: RootAttributes::ordinary_empty(),
                coordinates: StorageCoordinates {
                    volume_serial: 17,
                    root_file_id: 29,
                },
            },
            state: Mutex::new(FakeState::default()),
        }
    }

    fn fail_at(self, failure: Failure) -> Self {
        self.state.lock().unwrap().failures.push_back(failure);
        self
    }

    fn events(&self) -> Vec<&'static str> {
        self.state.lock().unwrap().events.clone()
    }

    fn entries(&self) -> HashSet<String> {
        self.state.lock().unwrap().entries.clone()
    }

    fn fail(state: &mut FakeState, operation: Failure) -> Result<(), AdapterError> {
        if state.failures.front() == Some(&operation) {
            state.failures.pop_front();
            return Err(AdapterError::io());
        }
        Ok(())
    }
}

impl PreflightAdapter for FakeAdapter {
    type Probe = (String, Vec<u8>);
    type RootHandle = ();

    fn inspect(&self, _root: &Path) -> Result<(RootInspection, Self::RootHandle), AdapterError> {
        let mut state = self.state.lock().unwrap();
        state.events.push("inspect");
        Self::fail(&mut state, Failure::Inspect)?;
        Ok((self.inspection, ()))
    }

    fn create_new(
        &self,
        _root: &Self::RootHandle,
        name: &ProbeName,
    ) -> Result<Self::Probe, AdapterError> {
        let mut state = self.state.lock().unwrap();
        state.events.push("create");
        Self::fail(&mut state, Failure::Create)?;
        if state.create_collisions > 0 {
            state.create_collisions -= 1;
            state.entries.insert(name.as_str().to_owned());
            return Err(AdapterError::already_exists());
        }
        assert!(state.entries.insert(name.as_str().to_owned()));
        Ok((name.as_str().to_owned(), Vec::new()))
    }

    fn write_all(&self, probe: &mut Self::Probe, bytes: &[u8]) -> Result<(), AdapterError> {
        let mut state = self.state.lock().unwrap();
        state.events.push("write");
        Self::fail(&mut state, Failure::Write)?;
        probe.1.extend_from_slice(bytes);
        state.contents = probe.1.clone();
        Ok(())
    }

    fn flush(&self, _probe: &mut Self::Probe) -> Result<(), AdapterError> {
        let mut state = self.state.lock().unwrap();
        state.events.push("flush");
        Self::fail(&mut state, Failure::Flush)
    }

    fn rename_no_replace(
        &self,
        _root: &Self::RootHandle,
        probe: &mut Self::Probe,
        destination: &ProbeName,
    ) -> Result<(), AdapterError> {
        let mut state = self.state.lock().unwrap();
        state.events.push("rename_no_replace");
        Self::fail(&mut state, Failure::Rename)?;
        if state.rename_collisions > 0 {
            state.rename_collisions -= 1;
            state.entries.insert(destination.as_str().to_owned());
            return Err(AdapterError::already_exists());
        }
        if state.entries.contains(destination.as_str()) {
            return Err(AdapterError::already_exists());
        }
        assert!(state.entries.remove(&probe.0));
        state.entries.insert(destination.as_str().to_owned());
        probe.0 = destination.as_str().to_owned();
        Ok(())
    }

    fn delete(&self, _root: &Self::RootHandle, name: &ProbeName) -> Result<(), AdapterError> {
        let mut state = self.state.lock().unwrap();
        state.events.push("delete");
        Self::fail(&mut state, Failure::Delete)?;
        state.entries.remove(name.as_str());
        Ok(())
    }

    fn current_coordinates(
        &self,
        _root: &Self::RootHandle,
    ) -> Result<StorageCoordinates, AdapterError> {
        let mut state = self.state.lock().unwrap();
        state.events.push("identity");
        Self::fail(&mut state, Failure::Identity)?;
        if state.mismatch {
            Ok(StorageCoordinates {
                volume_serial: 17,
                root_file_id: 30,
            })
        } else {
            Ok(self.inspection.coordinates)
        }
    }
}

fn error_for(mutator: impl FnOnce(&mut RootInspection)) -> PreflightError {
    let mut adapter = FakeAdapter::allowed();
    mutator(&mut adapter.inspection);
    preflight_with(&adapter, Path::new("opaque-root")).unwrap_err()
}

#[test]
fn rejects_non_ntfs_reparse_and_nonlocal_roots() {
    assert_eq!(
        error_for(|facts| facts.filesystem = Filesystem::Other).code(),
        "ntfs_required"
    );
    assert_eq!(
        error_for(|facts| facts.attributes.reparse_point = true).code(),
        "reparse_root_forbidden"
    );
    for volume_kind in [
        VolumeKind::Network,
        VolumeKind::Removable,
        VolumeKind::Unknown,
    ] {
        assert_eq!(
            error_for(|facts| facts.volume_kind = volume_kind).code(),
            "fixed_local_volume_required"
        );
    }
}

#[test]
fn rejects_volume_protected_nonempty_encrypted_and_placeholder_roots() {
    assert_eq!(
        error_for(|facts| facts.attributes.volume_root = true).code(),
        "root_must_be_new_or_empty"
    );
    assert_eq!(
        error_for(|facts| facts.attributes.protected_root = true).code(),
        "protected_root_forbidden"
    );
    assert_eq!(
        error_for(|facts| facts.attributes.empty = false).code(),
        "root_must_be_new_or_empty"
    );
    assert_eq!(
        error_for(|facts| facts.attributes.encrypted = true).code(),
        "encrypted_root_forbidden"
    );
    assert_eq!(
        error_for(|facts| facts.attributes.offline_placeholder = true).code(),
        "offline_placeholder_forbidden"
    );
}

#[test]
fn probe_is_ordered_and_returns_the_inspected_identity() {
    let adapter = FakeAdapter::allowed();
    let identity = preflight_with(&adapter, Path::new("opaque-root")).unwrap();

    assert_eq!(identity.coordinates().volume_serial, 17);
    assert_eq!(identity.coordinates().root_file_id, 29);
    assert_eq!(
        adapter.events(),
        [
            "inspect",
            "create",
            "write",
            "flush",
            "rename_no_replace",
            "delete",
            "identity"
        ]
    );
    assert!(adapter.entries().is_empty());
}

#[test]
fn every_injected_probe_failure_cleans_up_owned_entries() {
    for failure in [
        Failure::Write,
        Failure::Flush,
        Failure::Rename,
        Failure::Delete,
        Failure::Identity,
    ] {
        let adapter = FakeAdapter::allowed().fail_at(failure);
        let error = preflight_with(&adapter, Path::new("opaque-root")).unwrap_err();
        assert_eq!(error.code(), "preflight_io_failed");
        assert!(adapter.entries().is_empty(), "leak after {failure:?}");
    }
}

#[test]
fn collisions_never_overwrite_or_delete_existing_entries() {
    let adapter = FakeAdapter::allowed();
    adapter.state.lock().unwrap().create_collisions = 1;
    preflight_with(&adapter, Path::new("opaque-root")).unwrap();
    let entries = adapter.entries();
    assert_eq!(entries.len(), 1, "colliding attacker entry is preserved");

    let adapter = FakeAdapter::allowed();
    adapter.state.lock().unwrap().rename_collisions = 1;
    preflight_with(&adapter, Path::new("opaque-root")).unwrap();
    let entries = adapter.entries();
    assert_eq!(entries.len(), 1, "rename collision is not overwritten");
}

#[test]
fn identity_mismatch_and_adapter_errors_are_stable_and_redacted() {
    let adapter = FakeAdapter::allowed();
    adapter.state.lock().unwrap().mismatch = true;
    let mismatch =
        preflight_with(&adapter, Path::new(r"C:\secret\owner@example.test")).unwrap_err();
    assert_eq!(mismatch.code(), "identity_mismatch");

    let io = preflight_with(
        &FakeAdapter::allowed().fail_at(Failure::Inspect),
        Path::new(r"C:\secret\owner@example.test"),
    )
    .unwrap_err();
    assert_eq!(io.code(), "preflight_io_failed");
    for rendered in [format!("{io}"), format!("{io:?}"), format!("{mismatch:?}")] {
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("owner@example.test"));
    }
}

#[cfg(not(windows))]
#[test]
fn production_adapter_fails_closed_off_windows() {
    let error = cellar_windows::preflight::run_as_service(Path::new("/tmp/cellar"))
        .expect_err("unsupported hosts must not pretend storage is safe");
    assert_eq!(error.code(), "preflight_unsupported");
}

#[cfg(windows)]
#[test]
#[ignore = "acceptance: depends on the actual NTFS volume and must run under the installed service identity"]
fn service_identity_real_ntfs_preflight() {
    let directory = tempfile::tempdir().unwrap();
    cellar_windows::preflight::run_as_service(directory.path()).unwrap();
}
