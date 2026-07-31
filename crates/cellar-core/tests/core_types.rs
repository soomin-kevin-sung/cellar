use std::collections::HashSet;
use std::str::FromStr;

use cellar_core::{
    CellarError, FileEntryId, IdParseError, OperationId, ProjectId, ReadinessBlocker, TrashId,
    UploadId,
};
use serde::{Serialize, de::DeserializeOwned};
use uuid::Uuid;

fn assert_id_traits<T: Clone + Copy + std::fmt::Debug + Eq + std::hash::Hash + PartialEq>() {}

fn assert_v7_id<T>(id: T)
where
    T: Copy + std::fmt::Display,
{
    let parsed = Uuid::parse_str(&id.to_string()).expect("ID display should be a UUID");
    assert_eq!(parsed.get_version_num(), 7);
}

fn assert_string_and_json_round_trip<T>(id: T)
where
    T: Copy
        + Eq
        + std::fmt::Debug
        + std::fmt::Display
        + FromStr<Err = IdParseError>
        + Serialize
        + DeserializeOwned,
{
    let text = id.to_string();
    assert_eq!(text.len(), 36);
    assert_eq!(text.as_bytes()[8], b'-');
    assert_eq!(text.as_bytes()[13], b'-');
    assert_eq!(text.as_bytes()[18], b'-');
    assert_eq!(text.as_bytes()[23], b'-');
    assert_eq!(text.parse::<T>().expect("display should parse"), id);

    let json = serde_json::to_string(&id).expect("ID should serialize");
    assert_eq!(
        serde_json::from_str::<T>(&json).expect("ID should deserialize"),
        id
    );
}

macro_rules! check_id_type {
    ($name:ident) => {{
        assert_id_traits::<$name>();

        let fresh = $name::new();
        assert_v7_id(fresh);
        assert_string_and_json_round_trip(fresh);

        let defaulted = $name::default();
        assert_v7_id(defaulted);
        assert_ne!(fresh, defaulted);

        let mut ids = HashSet::new();
        assert!(ids.insert(fresh));
        assert!(!ids.insert(fresh));
    }};
}

#[test]
fn identifiers_are_opaque_uuid_v7_value_types() {
    check_id_type!(ProjectId);
    check_id_type!(FileEntryId);
    check_id_type!(UploadId);
    check_id_type!(OperationId);
    check_id_type!(TrashId);
}

#[test]
fn identifiers_report_a_typed_parse_error() {
    let error: IdParseError = ProjectId::from_str("not-a-uuid").expect_err("input is invalid");
    assert!(!error.to_string().is_empty());
}

#[test]
fn readiness_blockers_have_stable_codes_and_display() {
    let cases = [
        (
            ReadinessBlocker::ConfigurationRequired,
            "configuration_required",
        ),
        (
            ReadinessBlocker::OwnerEnrollmentRequired,
            "owner_enrollment_required",
        ),
        (ReadinessBlocker::MigrationRequired, "migration_required"),
        (ReadinessBlocker::RecoveryRequired, "recovery_required"),
        (ReadinessBlocker::StorageUnavailable, "storage_unavailable"),
        (
            ReadinessBlocker::ReconciliationRequired,
            "reconciliation_required",
        ),
    ];

    for (blocker, expected_code) in cases {
        assert_eq!(blocker.code(), expected_code);
        assert_eq!(blocker.to_string(), expected_code);
    }
}

#[test]
fn cellar_errors_have_stable_category_codes() {
    let cases = [
        (CellarError::InvalidInput, "invalid_input"),
        (CellarError::Unauthenticated, "unauthenticated"),
        (CellarError::Forbidden, "forbidden"),
        (CellarError::Conflict, "conflict"),
        (CellarError::InvalidRange, "invalid_range"),
        (CellarError::StorageFull, "storage_full"),
        (CellarError::Unavailable, "unavailable"),
    ];

    for (error, expected_code) in cases {
        assert_eq!(error.code(), expected_code);
        assert!(!error.to_string().is_empty());
    }

    let internal = CellarError::internal(std::io::Error::other("sensitive backend details"));
    assert_eq!(internal.code(), "internal");
    assert!(!internal.to_string().contains("sensitive backend details"));
}
