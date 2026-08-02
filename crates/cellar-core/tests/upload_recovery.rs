use cellar_core::{
    PublicationPresence, RecoveryDecision, UploadPublicationObservation, decide_upload_recovery,
};

#[test]
fn upload_recovery_decision_table_never_invents_or_duplicates_publication() {
    use PublicationPresence::{Absent, Expected, Unexpected};

    let cases = [
        ((Expected, Absent), RecoveryDecision::Publish),
        ((Absent, Expected), RecoveryDecision::CompleteCatalog),
        ((Expected, Expected), RecoveryDecision::FailConflict),
        ((Expected, Unexpected), RecoveryDecision::FailConflict),
        ((Unexpected, Absent), RecoveryDecision::FailConflict),
        ((Absent, Unexpected), RecoveryDecision::FailConflict),
        ((Unexpected, Unexpected), RecoveryDecision::FailConflict),
        ((Absent, Absent), RecoveryDecision::FailMissing),
    ];

    for ((staging, destination), expected) in cases {
        assert_eq!(
            decide_upload_recovery(UploadPublicationObservation {
                staging,
                destination,
            }),
            expected,
        );
    }
}
