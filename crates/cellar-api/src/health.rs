use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router, extract::State};
use cellar_core::ReadinessBlocker;
use serde::Serialize;

const ORDERED_BLOCKERS: [ReadinessBlocker; 7] = [
    ReadinessBlocker::ConfigurationRequired,
    ReadinessBlocker::OwnerEnrollmentRequired,
    ReadinessBlocker::MigrationRequired,
    ReadinessBlocker::RecoveryRequired,
    ReadinessBlocker::StorageUnavailable,
    ReadinessBlocker::ReconciliationRequired,
    ReadinessBlocker::OriginTrustUpdateRequired,
];
const ALL_BLOCKED: u8 = (1 << ORDERED_BLOCKERS.len()) - 1;

#[derive(Clone, Debug)]
pub struct Readiness {
    blockers: Arc<AtomicU8>,
}

impl Readiness {
    #[must_use]
    pub fn new(blockers: impl IntoIterator<Item = ReadinessBlocker>) -> Self {
        let state = Self {
            blockers: Arc::new(AtomicU8::new(0)),
        };
        for blocker in blockers {
            state.block(blocker);
        }
        state
    }

    #[must_use]
    pub fn all_blocked() -> Self {
        Self {
            blockers: Arc::new(AtomicU8::new(ALL_BLOCKED)),
        }
    }

    pub fn block(&self, blocker: ReadinessBlocker) {
        self.blockers.fetch_or(bit(blocker), Ordering::AcqRel);
    }

    pub fn clear(&self, blocker: ReadinessBlocker) {
        self.blockers.fetch_and(!bit(blocker), Ordering::AcqRel);
    }

    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.blockers.load(Ordering::Acquire) == 0
    }

    #[must_use]
    pub fn blocker_codes(&self) -> Vec<&'static str> {
        let bits = self.blockers.load(Ordering::Acquire);
        ORDERED_BLOCKERS
            .iter()
            .copied()
            .filter(|blocker| bits & bit(*blocker) != 0)
            .map(ReadinessBlocker::code)
            .collect()
    }
}

const fn bit(blocker: ReadinessBlocker) -> u8 {
    match blocker {
        ReadinessBlocker::ConfigurationRequired => 1 << 0,
        ReadinessBlocker::OwnerEnrollmentRequired => 1 << 1,
        ReadinessBlocker::MigrationRequired => 1 << 2,
        ReadinessBlocker::RecoveryRequired => 1 << 3,
        ReadinessBlocker::StorageUnavailable => 1 << 4,
        ReadinessBlocker::ReconciliationRequired => 1 << 5,
        ReadinessBlocker::OriginTrustUpdateRequired => 1 << 6,
    }
}

#[derive(Serialize)]
struct LiveBody {
    status: &'static str,
}

#[derive(Serialize)]
struct ReadyBody {
    status: &'static str,
    blockers: Vec<&'static str>,
}

pub fn health_router(readiness: Readiness) -> Router {
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .with_state(readiness)
}

async fn live() -> Json<LiveBody> {
    Json(LiveBody { status: "live" })
}

async fn ready(State(readiness): State<Readiness>) -> (StatusCode, Json<ReadyBody>) {
    let blockers = readiness.blocker_codes();
    let (status, state) = if blockers.is_empty() {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "blocked")
    };
    (
        status,
        Json(ReadyBody {
            status: state,
            blockers,
        }),
    )
}
