use std::collections::HashSet;
use std::str::FromStr;

use cellar_core::{
    CellarError, FileEntryId, OperationId, ParseIdError, ProjectId, ReadinessBlocker, TrashId,
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
        + FromStr<Err = ParseIdError>
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
    assert_eq!(json, format!("\"{text}\""));
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
    let error: ParseIdError = ProjectId::from_str("not-a-uuid").expect_err("input is invalid");
    assert!(matches!(error, ParseIdError::InvalidUuid(_)));
    assert!(!error.to_string().is_empty());
}

fn assert_rejects_non_v7<T>()
where
    T: std::fmt::Debug + FromStr<Err = ParseIdError> + DeserializeOwned,
{
    let non_v7_ids = [
        "00000000-0000-0000-0000-000000000000",
        "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
        "550e8400-e29b-41d4-a716-446655440000",
    ];

    for value in non_v7_ids {
        assert!(matches!(
            value.parse::<T>(),
            Err(ParseIdError::WrongVersion)
        ));

        let json = format!("\"{value}\"");
        assert!(
            serde_json::from_str::<T>(&json).is_err(),
            "{value} must be rejected during JSON deserialization"
        );
    }
}

#[test]
fn identifiers_reject_non_v7_text_and_json() {
    assert_rejects_non_v7::<ProjectId>();
    assert_rejects_non_v7::<FileEntryId>();
    assert_rejects_non_v7::<UploadId>();
    assert_rejects_non_v7::<OperationId>();
    assert_rejects_non_v7::<TrashId>();
}

fn assert_binary_serde_invariants<T>()
where
    T: Copy + Default + Eq + std::fmt::Debug + Serialize + DeserializeOwned,
{
    let id = T::default();
    let encoded =
        bincode::serde::encode_to_vec(id, bincode::config::standard()).expect("ID should encode");
    let (decoded, bytes_read): (T, usize) =
        bincode::serde::decode_from_slice(&encoded, bincode::config::standard())
            .expect("ID should decode");
    assert_eq!(bytes_read, encoded.len());
    assert_eq!(decoded, id);

    let non_v7 = String::from("550e8400-e29b-41d4-a716-446655440000");
    let encoded_non_v7 =
        bincode::serde::encode_to_vec(non_v7, bincode::config::standard()).expect("text encodes");
    assert!(
        bincode::serde::decode_from_slice::<T, _>(&encoded_non_v7, bincode::config::standard())
            .is_err()
    );
}

#[test]
fn identifiers_have_format_invariant_binary_serde() {
    assert_binary_serde_invariants::<ProjectId>();
    assert_binary_serde_invariants::<FileEntryId>();
    assert_binary_serde_invariants::<UploadId>();
    assert_binary_serde_invariants::<OperationId>();
    assert_binary_serde_invariants::<TrashId>();
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
        (
            ReadinessBlocker::OriginTrustUpdateRequired,
            "origin_trust_update_required",
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
}

#[test]
fn internal_errors_redact_sources_from_outward_formatting() {
    const SENSITIVE_MARKER: &str = "sensitive-backend-marker";
    let internal = CellarError::internal(std::io::Error::other(SENSITIVE_MARKER));

    assert_eq!(internal.code(), "internal");
    assert!(std::error::Error::source(&internal).is_some());
    assert!(!internal.to_string().contains(SENSITIVE_MARKER));
    assert!(!format!("{internal:?}").contains(SENSITIVE_MARKER));
}
