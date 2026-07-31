use axum::http::StatusCode;
use cellar_api::routes::session::{SessionRouteError, csrf_status, get_session};
use cellar_auth::{AccessClaims, CsrfError, CsrfManager, MutationHeaders};
use std::sync::{Arc, Barrier};
use std::thread;

const NOW: i64 = 50_000;
const ORIGIN: &str = "https://cellar.example";
const SUBJECT: &str = "owner-subject";

fn claims() -> AccessClaims {
    AccessClaims {
        iss: "https://issuer.example".into(),
        aud: vec!["aud".into()],
        sub: SUBJECT.into(),
        email: Some("owner@example.com".into()),
        exp: NOW + 40_000,
        nbf: NOW,
        iat: NOW,
        r#type: "app".into(),
    }
}

fn headers<'a>(origin: &'a [u8], token: &'a [u8]) -> MutationHeaders<'a> {
    MutationHeaders {
        origins: vec![origin],
        csrf_tokens: vec![token],
        sec_fetch_site: vec![],
    }
}

#[test]
fn csrf_errors_have_stable_http_mappings() {
    assert_eq!(
        csrf_status(&CsrfError::Unauthenticated),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(csrf_status(&CsrfError::Forbidden), StatusCode::FORBIDDEN);
    assert_eq!(
        csrf_status(&CsrfError::Unavailable),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[test]
fn session_issue_returns_base64url_token_and_rotation_invalidates_prior_token() {
    let manager = CsrfManager::new();
    let first = manager.issue(&claims(), SUBJECT, NOW).unwrap();
    assert_eq!(first.len(), 43);
    assert!(!first.contains('='));
    let second = manager.issue(&claims(), SUBJECT, NOW + 1).unwrap();
    assert_ne!(first, second);
    assert_eq!(
        manager
            .validate_mutation(
                "POST",
                headers(ORIGIN.as_bytes(), first.as_bytes()),
                &claims(),
                SUBJECT,
                ORIGIN,
                NOW + 1,
            )
            .unwrap_err(),
        CsrfError::Forbidden
    );
    manager
        .validate_mutation(
            "POST",
            headers(ORIGIN.as_bytes(), second.as_bytes()),
            &claims(),
            SUBJECT,
            ORIGIN,
            NOW + 1,
        )
        .unwrap();
}

#[test]
fn restart_and_access_window_change_invalidate_tokens() {
    let manager = CsrfManager::new();
    let token = manager.issue(&claims(), SUBJECT, NOW).unwrap();
    assert_eq!(
        CsrfManager::new()
            .validate_mutation(
                "DELETE",
                headers(ORIGIN.as_bytes(), token.as_bytes()),
                &claims(),
                SUBJECT,
                ORIGIN,
                NOW + 1,
            )
            .unwrap_err(),
        CsrfError::Forbidden
    );
    let mut changed = claims();
    changed.iat += 1;
    assert_eq!(
        manager
            .validate_mutation(
                "PUT",
                headers(ORIGIN.as_bytes(), token.as_bytes()),
                &changed,
                SUBJECT,
                ORIGIN,
                NOW + 1,
            )
            .unwrap_err(),
        CsrfError::Forbidden
    );
}

#[test]
fn mutation_guard_rejects_duplicate_missing_malformed_and_cross_site_headers() {
    let manager = CsrfManager::new();
    let token = manager.issue(&claims(), SUBJECT, NOW).unwrap();
    let origin = ORIGIN.as_bytes();
    let token = token.as_bytes();
    let cases = [
        MutationHeaders {
            origins: vec![],
            csrf_tokens: vec![token],
            sec_fetch_site: vec![],
        },
        MutationHeaders {
            origins: vec![origin, origin],
            csrf_tokens: vec![token],
            sec_fetch_site: vec![],
        },
        MutationHeaders {
            origins: vec![b"null"],
            csrf_tokens: vec![token],
            sec_fetch_site: vec![],
        },
        MutationHeaders {
            origins: vec![origin],
            csrf_tokens: vec![],
            sec_fetch_site: vec![],
        },
        MutationHeaders {
            origins: vec![origin],
            csrf_tokens: vec![token, token],
            sec_fetch_site: vec![],
        },
        MutationHeaders {
            origins: vec![origin],
            csrf_tokens: vec![b"not base64!"],
            sec_fetch_site: vec![],
        },
        MutationHeaders {
            origins: vec![origin],
            csrf_tokens: vec![token],
            sec_fetch_site: vec![b"cross-site"],
        },
    ];
    for candidate in cases {
        assert_eq!(
            manager
                .validate_mutation("PATCH", candidate, &claims(), SUBJECT, ORIGIN, NOW + 1)
                .unwrap_err(),
            CsrfError::Forbidden
        );
    }
}

#[test]
fn mutation_guard_rejects_wrong_subject_token_and_exact_expiries() {
    let manager = CsrfManager::new();
    let token = manager.issue(&claims(), SUBJECT, NOW).unwrap();
    let token_headers = || headers(ORIGIN.as_bytes(), token.as_bytes());
    let mut wrong = claims();
    wrong.sub = "other".into();
    assert_eq!(
        manager
            .validate_mutation("POST", token_headers(), &wrong, SUBJECT, ORIGIN, NOW + 1)
            .unwrap_err(),
        CsrfError::Unauthenticated
    );
    assert_eq!(
        manager
            .validate_mutation(
                "POST",
                headers(
                    ORIGIN.as_bytes(),
                    b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                ),
                &claims(),
                SUBJECT,
                ORIGIN,
                NOW + 1
            )
            .unwrap_err(),
        CsrfError::Forbidden
    );
    assert_eq!(
        manager
            .validate_mutation(
                "POST",
                token_headers(),
                &claims(),
                SUBJECT,
                ORIGIN,
                NOW + 8 * 60 * 60
            )
            .unwrap_err(),
        CsrfError::Forbidden
    );
    let mut expiring = claims();
    expiring.exp = NOW + 2;
    let expiring_token = manager.issue(&expiring, SUBJECT, NOW + 1).unwrap();
    assert_eq!(
        manager
            .validate_mutation(
                "POST",
                headers(ORIGIN.as_bytes(), expiring_token.as_bytes()),
                &expiring,
                SUBJECT,
                ORIGIN,
                NOW + 2
            )
            .unwrap_err(),
        CsrfError::Unauthenticated
    );
}

#[test]
fn safe_methods_do_not_require_csrf() {
    let manager = CsrfManager::new();
    for method in ["GET", "HEAD", "OPTIONS"] {
        manager
            .validate_mutation(
                method,
                MutationHeaders::empty(),
                &claims(),
                SUBJECT,
                ORIGIN,
                NOW,
            )
            .unwrap();
    }
}

#[test]
fn concurrent_issue_leaves_exactly_one_current_token_without_mismatch() {
    let manager = Arc::new(CsrfManager::new());
    let barrier = Arc::new(Barrier::new(9));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let manager = Arc::clone(&manager);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                manager.issue(&claims(), SUBJECT, NOW).unwrap()
            })
        })
        .collect();
    barrier.wait();
    let tokens: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    let valid = tokens
        .iter()
        .filter(|token| {
            manager
                .validate_mutation(
                    "POST",
                    headers(ORIGIN.as_bytes(), token.as_bytes()),
                    &claims(),
                    SUBJECT,
                    ORIGIN,
                    NOW,
                )
                .is_ok()
        })
        .count();
    assert_eq!(valid, 1);
}

#[test]
fn session_route_issues_only_for_authenticated_get() {
    let manager = CsrfManager::new();
    let response = get_session("GET", &manager, &claims(), SUBJECT, NOW).unwrap();
    assert_eq!(response.csrf_token.len(), 43);
    assert_eq!(
        get_session("POST", &manager, &claims(), SUBJECT, NOW).unwrap_err(),
        SessionRouteError::NotFound
    );
    let mut other = claims();
    other.sub = "other".into();
    assert_eq!(
        get_session("GET", &manager, &other, SUBJECT, NOW).unwrap_err(),
        SessionRouteError::Csrf(CsrfError::Unauthenticated)
    );
}
