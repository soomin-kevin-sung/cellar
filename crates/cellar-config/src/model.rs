use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use url::Url;

use crate::ConfigError;

/// Security-sensitive Cellar configuration supplied by the service host.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CellarConfig {
    pub external_origin: Url,
    pub team_domain: Url,
    pub aud_tags: Vec<String>,
    pub bootstrap_owner_email: Option<String>,
    pub owner_subject: Option<String>,
    pub storage_root: PathBuf,
    pub origin_port: u16,
    pub health_port: u16,
}

impl CellarConfig {
    /// Validates all security-sensitive configuration without supplying defaults.
    pub fn validate(&self) -> Result<(), ConfigError> {
        crate::validate::validate(self)
    }
}

/// Persisted bootstrap-claim metadata. The plaintext claim is never retained.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct BootstrapClaim {
    claim_code_sha256: [u8; 32],
    pub expires_at_unix_seconds: i64,
}

impl BootstrapClaim {
    #[must_use]
    pub fn new(claim_code: &[u8; 32], expires_at_unix_seconds: i64) -> Self {
        Self {
            claim_code_sha256: hash_claim_code(claim_code),
            expires_at_unix_seconds,
        }
    }

    /// Verifies the claim hash in constant time and rejects claims at or after expiry.
    #[must_use]
    pub fn verify(&self, candidate: &[u8; 32], now_unix_seconds: i64) -> bool {
        let candidate_hash = hash_claim_code(candidate);
        let hash_matches = self.claim_code_sha256.ct_eq(&candidate_hash);
        bool::from(hash_matches) && now_unix_seconds < self.expires_at_unix_seconds
    }
}

fn hash_claim_code(claim_code: &[u8; 32]) -> [u8; 32] {
    Sha256::digest(claim_code).into()
}

/// On-disk `config.toml` representation.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct PersistedConfig {
    #[serde(flatten)]
    pub config: CellarConfig,
    pub bootstrap_claim: Option<BootstrapClaim>,
}
