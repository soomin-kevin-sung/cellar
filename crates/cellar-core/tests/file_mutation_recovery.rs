use cellar_core::{
    CaseRenameObservation, CaseRenameRecoveryDecision, CopyMutationObservation,
    CopyMutationRecoveryDecision, NamespaceMutationObservation, NamespaceMutationRecoveryDecision,
    PublicationPresence, decide_case_rename_recovery, decide_copy_mutation_recovery,
    decide_namespace_mutation_recovery,
};

use PublicationPresence::{Absent, Expected, Unexpected};

#[test]
fn namespace_recovery_preserves_first_writer_and_never_invents_success() {
    for (source, destination, decision) in [
        (Expected, Absent, NamespaceMutationRecoveryDecision::Retry),
        (
            Absent,
            Expected,
            NamespaceMutationRecoveryDecision::Complete,
        ),
        (
            Expected,
            Expected,
            NamespaceMutationRecoveryDecision::Conflict,
        ),
        (
            Unexpected,
            Absent,
            NamespaceMutationRecoveryDecision::Conflict,
        ),
        (
            Absent,
            Unexpected,
            NamespaceMutationRecoveryDecision::Conflict,
        ),
        (
            Unexpected,
            Unexpected,
            NamespaceMutationRecoveryDecision::Conflict,
        ),
        (Absent, Absent, NamespaceMutationRecoveryDecision::Missing),
    ] {
        assert_eq!(
            decide_namespace_mutation_recovery(NamespaceMutationObservation {
                source,
                destination,
            }),
            decision,
        );
    }
}

#[test]
fn copy_recovery_only_recopies_from_the_expected_source() {
    for (source, staging, destination, decision) in [
        (
            Expected,
            Expected,
            Absent,
            CopyMutationRecoveryDecision::Publish,
        ),
        (
            Expected,
            Absent,
            Expected,
            CopyMutationRecoveryDecision::Complete,
        ),
        (
            Expected,
            Absent,
            Absent,
            CopyMutationRecoveryDecision::Recopy,
        ),
        (
            Absent,
            Absent,
            Absent,
            CopyMutationRecoveryDecision::Missing,
        ),
        (
            Unexpected,
            Absent,
            Absent,
            CopyMutationRecoveryDecision::Conflict,
        ),
        (
            Expected,
            Expected,
            Expected,
            CopyMutationRecoveryDecision::Conflict,
        ),
        (
            Expected,
            Absent,
            Unexpected,
            CopyMutationRecoveryDecision::Conflict,
        ),
    ] {
        assert_eq!(
            decide_copy_mutation_recovery(CopyMutationObservation {
                source,
                staging,
                destination,
            }),
            decision,
        );
    }
}

#[test]
fn case_only_recovery_advances_exactly_one_identity_location() {
    for (source, temporary, final_name, decision) in [
        (
            Expected,
            Absent,
            Absent,
            CaseRenameRecoveryDecision::RenameToTemporary,
        ),
        (
            Absent,
            Expected,
            Absent,
            CaseRenameRecoveryDecision::RenameToFinal,
        ),
        (
            Absent,
            Absent,
            Expected,
            CaseRenameRecoveryDecision::Complete,
        ),
        (Absent, Absent, Absent, CaseRenameRecoveryDecision::Missing),
        (
            Expected,
            Expected,
            Absent,
            CaseRenameRecoveryDecision::Conflict,
        ),
        (
            Unexpected,
            Absent,
            Absent,
            CaseRenameRecoveryDecision::Conflict,
        ),
        (
            Absent,
            Absent,
            Unexpected,
            CaseRenameRecoveryDecision::Conflict,
        ),
    ] {
        assert_eq!(
            decide_case_rename_recovery(CaseRenameObservation {
                source,
                temporary,
                final_name,
            }),
            decision,
        );
    }
}
