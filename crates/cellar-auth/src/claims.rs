use std::fmt;

use serde::Deserialize;

use crate::AuthError;

const MAX_SUBJECT_BYTES: usize = 512;
const MAX_EMAIL_BYTES: usize = 512;
const MAX_ISSUER_BYTES: usize = 2_048;
const MAX_AUDIENCES: usize = 32;
const MAX_AUDIENCE_BYTES: usize = 2_048;

#[derive(Clone, Deserialize, Eq, PartialEq)]
pub struct AccessClaims {
    pub iss: String,
    pub aud: Vec<String>,
    pub sub: String,
    pub email: Option<String>,
    pub exp: i64,
    pub nbf: i64,
    pub iat: i64,
    #[serde(rename = "type")]
    pub r#type: String,
}

impl fmt::Debug for AccessClaims {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessClaims")
            .field("iss", &"[redacted]")
            .field("aud", &"[redacted]")
            .field("sub", &"[redacted]")
            .field("email", &"[redacted]")
            .field("exp", &self.exp)
            .field("nbf", &self.nbf)
            .field("iat", &self.iat)
            .field("type", &self.r#type)
            .finish()
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum OwnerMode<'a> {
    Unenrolled { bootstrap_email: &'a str },
    Enrolled { owner_subject: &'a str },
}

impl fmt::Debug for OwnerMode<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unenrolled { .. } => f
                .debug_struct("Unenrolled")
                .field("bootstrap_email", &"[redacted]")
                .finish(),
            Self::Enrolled { .. } => f
                .debug_struct("Enrolled")
                .field("owner_subject", &"[redacted]")
                .finish(),
        }
    }
}

pub(crate) fn validate_claims(
    claims: &AccessClaims,
    issuer: &str,
    configured_audiences: &[String],
    clock_skew_seconds: i64,
    now: i64,
    owner_mode: OwnerMode<'_>,
) -> Result<(), AuthError> {
    if claims.iss.len() > MAX_ISSUER_BYTES || claims.iss != issuer {
        return Err(AuthError::new("invalid_issuer"));
    }
    if claims.aud.is_empty()
        || claims.aud.len() > MAX_AUDIENCES
        || claims
            .aud
            .iter()
            .any(|value| value.is_empty() || value.len() > MAX_AUDIENCE_BYTES)
        || !claims.aud.iter().any(|claim_audience| {
            configured_audiences
                .iter()
                .any(|configured| configured == claim_audience)
        })
    {
        return Err(AuthError::new("invalid_audience"));
    }
    if claims.exp.saturating_add(clock_skew_seconds) <= now {
        return Err(AuthError::new("token_expired"));
    }
    let latest_allowed = now.saturating_add(clock_skew_seconds);
    if claims.nbf > latest_allowed {
        return Err(AuthError::new("token_not_yet_valid"));
    }
    if claims.iat > latest_allowed {
        return Err(AuthError::new("token_issued_in_future"));
    }
    if claims.r#type == "service_token" {
        return Err(AuthError::new("service_token_forbidden"));
    }
    if claims.r#type != "app" {
        return Err(AuthError::new("invalid_token_type"));
    }
    if claims.sub.is_empty() || claims.sub.len() > MAX_SUBJECT_BYTES {
        return Err(AuthError::new("invalid_subject"));
    }
    if claims
        .email
        .as_ref()
        .is_some_and(|email| email.is_empty() || email.len() > MAX_EMAIL_BYTES)
    {
        return Err(AuthError::new("invalid_email"));
    }

    match owner_mode {
        OwnerMode::Unenrolled { bootstrap_email } => {
            if bootstrap_email.is_empty() || claims.email.as_deref() != Some(bootstrap_email) {
                return Err(AuthError::new("bootstrap_email_mismatch"));
            }
        }
        OwnerMode::Enrolled { owner_subject } => {
            if owner_subject.is_empty() || claims.sub != owner_subject {
                return Err(AuthError::new("owner_subject_mismatch"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AccessClaims, OwnerMode, validate_claims};

    const NOW: i64 = 10_000;
    const SKEW: i64 = 30;

    fn claims() -> AccessClaims {
        AccessClaims {
            iss: "https://cellar.cloudflareaccess.com".into(),
            aud: vec!["audience".into()],
            sub: "owner".into(),
            email: None,
            exp: NOW + 300,
            nbf: NOW,
            iat: NOW,
            r#type: "app".into(),
        }
    }

    fn validate(claims: &AccessClaims) -> Result<(), crate::AuthError> {
        let audiences = ["audience".to_owned()];
        validate_claims(
            claims,
            "https://cellar.cloudflareaccess.com",
            &audiences,
            SKEW,
            NOW,
            OwnerMode::Enrolled {
                owner_subject: "owner",
            },
        )
    }

    #[test]
    fn rejects_expiration_at_exact_skew_boundary() {
        let mut claims = claims();
        claims.exp = NOW - SKEW;
        assert_eq!(
            validate(&claims)
                .expect_err("exclusive exp boundary")
                .code(),
            "token_expired"
        );
    }

    #[test]
    fn accepts_nbf_and_iat_at_exact_skew_boundary() {
        let mut claims = claims();
        claims.nbf = NOW + SKEW;
        claims.iat = NOW + SKEW;
        validate(&claims).expect("inclusive nbf/iat skew boundary");
    }
}
