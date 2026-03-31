use prometheus::{HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Registry};
use std::sync::LazyLock;

// =============================================================================
// Authentication Metrics
// =============================================================================

/// Total IAM authentication requests, labeled by result and error_type.
pub static IAM_AUTH_REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        prometheus::opts!(
            "iam_auth_requests_total",
            "Total IAM authentication requests"
        ),
        &["result", "error_type"],
    )
    .unwrap()
});

/// Duration of SigV4 authentication verification in seconds.
pub static IAM_AUTH_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    HistogramVec::new(
        HistogramOpts::new(
            "iam_auth_duration_seconds",
            "Duration of SigV4 authentication verification",
        )
        .buckets(vec![
            0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0,
        ]),
        &["result"],
    )
    .unwrap()
});

// =============================================================================
// Authorization (Policy Evaluation) Metrics
// =============================================================================

/// Total IAM policy evaluations, labeled by result and S3 action.
pub static IAM_POLICY_EVALUATIONS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        prometheus::opts!(
            "iam_policy_evaluations_total",
            "Total IAM policy evaluations"
        ),
        &["result", "action"],
    )
    .unwrap()
});

/// Duration of IAM policy evaluation in seconds.
pub static IAM_POLICY_EVAL_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    HistogramVec::new(
        HistogramOpts::new(
            "iam_policy_evaluation_duration_seconds",
            "Duration of IAM policy evaluation",
        )
        .buckets(vec![
            0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0,
        ]),
        &[],
    )
    .unwrap()
});

// =============================================================================
// STS Metrics
// =============================================================================

/// Total STS AssumeRoleWithWebIdentity requests.
pub static IAM_STS_REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        prometheus::opts!(
            "iam_sts_requests_total",
            "Total STS AssumeRoleWithWebIdentity requests"
        ),
        &["result", "error_type"],
    )
    .unwrap()
});

/// Duration of STS token issuance in seconds.
pub static IAM_STS_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    HistogramVec::new(
        HistogramOpts::new(
            "iam_sts_token_duration_seconds",
            "Duration of STS token issuance",
        )
        .buckets(vec![
            0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
        ]),
        &[],
    )
    .unwrap()
});

/// Estimated number of active (non-expired) STS sessions.
pub static IAM_STS_ACTIVE_SESSIONS: LazyLock<IntGauge> = LazyLock::new(|| {
    IntGauge::new(
        "iam_sts_active_sessions_gauge",
        "Estimated number of active (non-expired) STS sessions",
    )
    .unwrap()
});

// =============================================================================
// OIDC Metrics
// =============================================================================

/// Total OIDC token validations.
pub static IAM_OIDC_VALIDATIONS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        prometheus::opts!("iam_oidc_validations_total", "Total OIDC token validations"),
        &["result"],
    )
    .unwrap()
});

/// Total JWKS fetch attempts.
pub static IAM_OIDC_JWKS_FETCHES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        prometheus::opts!("iam_oidc_jwks_fetches_total", "Total JWKS fetch attempts"),
        &["result"],
    )
    .unwrap()
});

/// Duration of JWKS fetch operations in seconds.
pub static IAM_OIDC_JWKS_FETCH_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    HistogramVec::new(
        HistogramOpts::new(
            "iam_oidc_jwks_fetch_duration_seconds",
            "Duration of JWKS fetch operations",
        )
        .buckets(vec![0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]),
        &[],
    )
    .unwrap()
});

/// Unix timestamp of last successful JWKS fetch.
pub static IAM_OIDC_JWKS_LAST_FETCH: LazyLock<IntGauge> = LazyLock::new(|| {
    IntGauge::new(
        "iam_oidc_jwks_last_fetch_timestamp",
        "Unix timestamp of last successful JWKS fetch",
    )
    .unwrap()
});

/// Register all IAM metrics to a Prometheus registry.
pub fn register_iam_metrics(registry: &Registry) -> Result<(), prometheus::Error> {
    // Authentication
    registry.register(Box::new(IAM_AUTH_REQUESTS.clone()))?;
    registry.register(Box::new(IAM_AUTH_DURATION.clone()))?;
    // Authorization
    registry.register(Box::new(IAM_POLICY_EVALUATIONS.clone()))?;
    registry.register(Box::new(IAM_POLICY_EVAL_DURATION.clone()))?;
    // STS
    registry.register(Box::new(IAM_STS_REQUESTS.clone()))?;
    registry.register(Box::new(IAM_STS_DURATION.clone()))?;
    registry.register(Box::new(IAM_STS_ACTIVE_SESSIONS.clone()))?;
    // OIDC
    registry.register(Box::new(IAM_OIDC_VALIDATIONS.clone()))?;
    registry.register(Box::new(IAM_OIDC_JWKS_FETCHES.clone()))?;
    registry.register(Box::new(IAM_OIDC_JWKS_FETCH_DURATION.clone()))?;
    registry.register(Box::new(IAM_OIDC_JWKS_LAST_FETCH.clone()))?;
    Ok(())
}
