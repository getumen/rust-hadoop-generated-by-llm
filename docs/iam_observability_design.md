# IAM Observability 設計書

> **対象タスク**: TODO.md §1 Security & Identity — IAM Observability
> **前提**: OIDC & STS Integration (IAM 代替)、Audit Logging は実装済み
> **最終更新**: 2026-03-23

---

## 1. 概要と目標

### 1.1 背景

OIDC/STS/IAMポリシー評価エンジン・Audit Loggingの実装は完了しているが、
IAMパイプライン全体の **運用可視性** が不足している。現在のメトリクスは以下に限定されている:

| カテゴリ | 既存メトリクス | 不足 |
|:---------|:-------------|:-----|
| S3 API | `s3_requests_total{method, path, status}` | IAMの成否の区別なし |
| Audit | `audit_log_total`, `audit_log_dropped_total`, `audit_log_flush_errors_total` | IAMイベント分類なし |
| IAM全般 | ― | 認証・認可・STS・OIDC 個別のメトリクスなし |

### 1.2 ゴール

1. **認証 (Authentication)** の成功/失敗率を即座に把握できるメトリクスの導入。
2. **認可 (Authorization / Policy Evaluation)** のレイテンシと拒否率の計測。
3. **STS トークン発行** の成否・レートの追跡。
4. **OIDC トークン検証** の成否・JWKS フェッチ状況の監視。
5. 既存 Grafana ダッシュボードへの IAM 専用パネル群の追加。
6. アラートルール (PrometheusRule) の追加。

---

## 2. メトリクス設計

### 2.1 メトリクス一覧

すべてのメトリクスは `iam_` プレフィックスで統一し、既存の `s3_` / `audit_log_` と区別する。

#### 2.1.1 認証メトリクス

| メトリクス名 | 型 | ラベル | 説明 |
|:------------|:---|:-------|:-----|
| `iam_auth_requests_total` | Counter | `result` (`success`, `failure`), `error_type` | SigV4認証リクエストの成功/失敗カウント |
| `iam_auth_duration_seconds` | Histogram | `result` | SigV4署名検証のレイテンシ（秒） |

`error_type` ラベル値（`failure` 時のみ有意）:

- `missing_auth` — 認証ヘッダなし
- `invalid_access_key` — AccessKey不正
- `signature_mismatch` — 署名不一致
- `clock_skew` — 時刻ずれ超過
- `invalid_credential_scope` — Credential Scope 不正
- `expired_token` — STSトークン期限切れ
- `invalid_token` — STSトークン復号失敗
- `insecure_transport` — TLS 非使用
- `internal_error` — 内部エラー

#### 2.1.2 認可メトリクス

| メトリクス名 | 型 | ラベル | 説明 |
|:------------|:---|:-------|:-----|
| `iam_policy_evaluations_total` | Counter | `result` (`allow`, `deny`), `action` | ポリシー評価の結果カウント |
| `iam_policy_evaluation_duration_seconds` | Histogram | — | ポリシー評価のレイテンシ（秒） |

#### 2.1.3 STS メトリクス

| メトリクス名 | 型 | ラベル | 説明 |
|:------------|:---|:-------|:-----|
| `iam_sts_requests_total` | Counter | `result` (`success`, `failure`), `error_type` | STS `AssumeRoleWithWebIdentity` リクエスト数 |
| `iam_sts_token_duration_seconds` | Histogram | — | STSトークン発行処理のレイテンシ |
| `iam_sts_active_sessions_gauge` | Gauge | — | 発行済みで未期限切れの推定セッション数（近似値） |

`error_type` ラベル値 (STS):

- `invalid_action` — 無効なAction
- `missing_token` — WebIdentityToken未指定
- `missing_role` — RoleArn未指定
- `invalid_identity_token` — OIDC検証失敗
- `access_denied` — Trust Policy不適合
- `oidc_not_enabled` / `sts_not_enabled` / `iam_not_enabled` — サービス未有効化
- `internal_error` — 内部エラー

#### 2.1.4 OIDC メトリクス

| メトリクス名 | 型 | ラベル | 説明 |
|:------------|:---|:-------|:-----|
| `iam_oidc_validations_total` | Counter | `result` (`success`, `failure`) | OIDC JWT検証の成否 |
| `iam_oidc_jwks_fetches_total` | Counter | `result` (`success`, `failure`) | JWKS取得の成否 |
| `iam_oidc_jwks_fetch_duration_seconds` | Histogram | — | JWKS取得のレイテンシ |
| `iam_oidc_jwks_last_fetch_timestamp` | Gauge | — | 最後にJWKS取得が成功したUNIXタイムスタンプ |

### 2.2 Histogram バケット設計

```rust
// 認証・認可は通常サブミリ秒で完了
const AUTH_DURATION_BUCKETS: &[f64] = &[
    0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0,
];

// OIDC/JWKS はネットワーク I/O を含むためより寛容な範囲
const OIDC_DURATION_BUCKETS: &[f64] = &[
    0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];
```

---

## 3. 実装計画

### 3.1 ファイル変更一覧

| ファイル | 変更内容 |
|:---------|:---------|
| `dfs/s3_server/src/iam_metrics.rs` | **新規作成** — メトリクス定義モジュール |
| `dfs/s3_server/src/auth_middleware.rs` | 認証・認可メトリクスの計装 |
| `dfs/s3_server/src/sts_handler.rs` | STSメトリクスの計装 |
| `dfs/common/src/auth/oidc.rs` | OIDCメトリクスの計装 |
| `dfs/s3_server/src/main.rs` | メトリクスのPrometheusレジストリ登録 |
| `deploy/helm/rust-hadoop/templates/grafana-dashboard.yaml` | IAMパネルの追加 |

### 3.2 Phase 1: メトリクス定義モジュール `iam_metrics.rs`

新しいモジュールを作成し、すべてのIAMメトリクスを一元管理する。

```rust
// dfs/s3_server/src/iam_metrics.rs

use prometheus::{
    HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Registry,
};
use std::sync::LazyLock;

// --- 認証 (Authentication) ---
pub static IAM_AUTH_REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        prometheus::opts!("iam_auth_requests_total", "Total IAM authentication requests"),
        &["result", "error_type"],
    )
    .unwrap()
});

pub static IAM_AUTH_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    HistogramVec::new(
        HistogramOpts::new(
            "iam_auth_duration_seconds",
            "Duration of SigV4 authentication verification",
        )
        .buckets(vec![0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0]),
        &["result"],
    )
    .unwrap()
});

// --- 認可 (Authorization) ---
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

pub static IAM_POLICY_EVAL_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    HistogramVec::new(
        HistogramOpts::new(
            "iam_policy_evaluation_duration_seconds",
            "Duration of IAM policy evaluation",
        )
        .buckets(vec![0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0]),
        &[],
    )
    .unwrap()
});

// --- STS ---
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

pub static IAM_STS_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    HistogramVec::new(
        HistogramOpts::new(
            "iam_sts_token_duration_seconds",
            "Duration of STS token issuance",
        )
        .buckets(vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5]),
        &[],
    )
    .unwrap()
});

pub static IAM_STS_ACTIVE_SESSIONS: LazyLock<IntGauge> = LazyLock::new(|| {
    IntGauge::new(
        "iam_sts_active_sessions_gauge",
        "Estimated number of active (non-expired) STS sessions",
    )
    .unwrap()
});

// --- OIDC ---
pub static IAM_OIDC_VALIDATIONS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        prometheus::opts!(
            "iam_oidc_validations_total",
            "Total OIDC token validations"
        ),
        &["result"],
    )
    .unwrap()
});

pub static IAM_OIDC_JWKS_FETCHES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        prometheus::opts!(
            "iam_oidc_jwks_fetches_total",
            "Total JWKS fetch attempts"
        ),
        &["result"],
    )
    .unwrap()
});

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

pub static IAM_OIDC_JWKS_LAST_FETCH: LazyLock<IntGauge> = LazyLock::new(|| {
    IntGauge::new(
        "iam_oidc_jwks_last_fetch_timestamp",
        "Unix timestamp of last successful JWKS fetch",
    )
    .unwrap()
});

/// Register all IAM metrics to a Prometheus registry.
pub fn register_iam_metrics(registry: &Registry) -> Result<(), prometheus::Error> {
    registry.register(Box::new(IAM_AUTH_REQUESTS.clone()))?;
    registry.register(Box::new(IAM_AUTH_DURATION.clone()))?;
    registry.register(Box::new(IAM_POLICY_EVALUATIONS.clone()))?;
    registry.register(Box::new(IAM_POLICY_EVAL_DURATION.clone()))?;
    registry.register(Box::new(IAM_STS_REQUESTS.clone()))?;
    registry.register(Box::new(IAM_STS_DURATION.clone()))?;
    registry.register(Box::new(IAM_STS_ACTIVE_SESSIONS.clone()))?;
    registry.register(Box::new(IAM_OIDC_VALIDATIONS.clone()))?;
    registry.register(Box::new(IAM_OIDC_JWKS_FETCHES.clone()))?;
    registry.register(Box::new(IAM_OIDC_JWKS_FETCH_DURATION.clone()))?;
    registry.register(Box::new(IAM_OIDC_JWKS_LAST_FETCH.clone()))?;
    Ok(())
}
```

### 3.3 Phase 2: `auth_middleware.rs` への計装

認証・認可パイプラインの各ポイントにメトリクスを挿入する。

```rust
// auth_middleware.rs の変更点（擬似 diff）

use crate::iam_metrics::*;
use std::time::Instant;

pub async fn auth_middleware(...) -> Response {
    // 認証計測開始
    let auth_start = Instant::now();

    // ... 既存の認証ロジック ...

    // 認証失敗時（各 early return 箇所）
    // 例: parse_credentials 失敗
    Err(e) => {
        let error_type = classify_auth_error(&e);
        IAM_AUTH_REQUESTS
            .with_label_values(&["failure", &error_type])
            .inc();
        IAM_AUTH_DURATION
            .with_label_values(&["failure"])
            .observe(auth_start.elapsed().as_secs_f64());
        // ... 既存のエラーレスポンス ...
    }

    // 署名検証成功時
    Ok(_) => {
        IAM_AUTH_REQUESTS
            .with_label_values(&["success", "none"])
            .inc();
        IAM_AUTH_DURATION
            .with_label_values(&["success"])
            .observe(auth_start.elapsed().as_secs_f64());

        // ポリシー評価 (認可)
        if let Some(role_arn) = &role_arn_from_token {
            if let (Some(pe), Some(ctx)) = (&state.policy_evaluator, &evaluation_context) {
                let policy_start = Instant::now();
                let (s3_action, s3_resource) = resolve_s3_action_and_resource(&req);

                let allowed = pe.evaluate(&s3_action, &s3_resource, role_arn, ctx);

                IAM_POLICY_EVAL_DURATION
                    .with_label_values(&[])
                    .observe(policy_start.elapsed().as_secs_f64());

                if allowed {
                    IAM_POLICY_EVALUATIONS
                        .with_label_values(&["allow", &s3_action])
                        .inc();
                } else {
                    IAM_POLICY_EVALUATIONS
                        .with_label_values(&["deny", &s3_action])
                        .inc();
                    // ... AccessDenied response ...
                }
            }
        }
    }
}

/// Map AuthError to a metric-friendly error_type label.
fn classify_auth_error(err: &AuthError) -> String {
    match err {
        AuthError::MissingAuth => "missing_auth",
        AuthError::InvalidAccessKey { .. } => "invalid_access_key",
        AuthError::SignatureDoesNotMatch { .. } => "signature_mismatch",
        AuthError::RequestTimeTooSkewed { .. } => "clock_skew",
        AuthError::InvalidCredentialScope { .. } => "invalid_credential_scope",
        AuthError::ExpiredToken => "expired_token",
        AuthError::InvalidToken(_) => "invalid_token",
        AuthError::InsecureTransport => "insecure_transport",
        AuthError::InternalError(_) => "internal_error",
    }
    .to_string()
}
```

### 3.4 Phase 3: `sts_handler.rs` への計装

```rust
// sts_handler.rs の変更点

use crate::iam_metrics::*;
use std::time::Instant;

pub async fn handle_sts(...) -> Response {
    let sts_start = Instant::now();

    // ... 既存のロジック ...

    // 各失敗ポイントで:
    IAM_STS_REQUESTS
        .with_label_values(&["failure", "invalid_identity_token"])
        .inc();

    // 成功時:
    IAM_STS_REQUESTS
        .with_label_values(&["success", "none"])
        .inc();
    IAM_STS_DURATION
        .with_label_values(&[])
        .observe(sts_start.elapsed().as_secs_f64());
    IAM_STS_ACTIVE_SESSIONS.inc(); // 近似: 発行件数のインクリメント

    // OIDC検証成功/失敗のトラッキング
    match oidc.validate_token(web_identity_token).await {
        Ok(c) => {
            IAM_OIDC_VALIDATIONS.with_label_values(&["success"]).inc();
            c
        }
        Err(e) => {
            IAM_OIDC_VALIDATIONS.with_label_values(&["failure"]).inc();
            // ...
        }
    }
}
```

### 3.5 Phase 4: `oidc.rs` (dfs-common) への計装

OIDC Validator は `dfs-common` にあるため、メトリクスは **コールバック/トレイト** またはメトリクスクレートの直接参照で計装する。
`dfs-common` の `Cargo.toml` に `prometheus` を追加する方針を取る（既にS3サーバー側で使用しており、統一性がある）。

```rust
// dfs/common/src/auth/oidc.rs の変更点

pub async fn fetch_jwks(&self) -> Result<(), AuthError> {
    let start = std::time::Instant::now();

    // ... 既存のフェッチロジック ...

    // 成功時:
    #[cfg(feature = "metrics")]
    {
        IAM_OIDC_JWKS_FETCHES.with_label_values(&["success"]).inc();
        IAM_OIDC_JWKS_FETCH_DURATION
            .with_label_values(&[])
            .observe(start.elapsed().as_secs_f64());
        IAM_OIDC_JWKS_LAST_FETCH.set(chrono::Utc::now().timestamp());
    }

    // 失敗時:
    #[cfg(feature = "metrics")]
    {
        IAM_OIDC_JWKS_FETCHES.with_label_values(&["failure"]).inc();
    }
}
```

> **設計判断**: OIDC メトリクスを `dfs-common` に直接統合するか、`s3_server` 側のラッパーで計測するか。
> **推奨**: `s3_server/sts_handler.rs` 内のラッパーで OIDC 呼び出しを計測する方が、`dfs-common` への依存追加を最小化できる。
> `dfs-common` 側には feature flag `metrics` を設け、オプトインとする。

### 3.6 Phase 5: `/metrics` エンドポイント更新

```rust
// main.rs の handle_metrics 更新

async fn handle_metrics() -> Result<String, InternalError> {
    let registry = Registry::new();

    // 既存の S3 メトリクス
    registry.register(Box::new(S3_REQUESTS.clone())).map_err(/*...*/)?;

    // 既存の Audit メトリクス
    registry.register(Box::new(crate::audit::AUDIT_LOG_TOTAL.clone())).map_err(/*...*/)?;
    registry.register(Box::new(crate::audit::AUDIT_LOG_DROPPED.clone())).map_err(/*...*/)?;
    registry.register(Box::new(crate::audit::AUDIT_LOG_FLUSH_ERRORS.clone())).map_err(/*...*/)?;

    // ★ IAM メトリクスの一括登録
    crate::iam_metrics::register_iam_metrics(&registry).map_err(|e| {
        tracing::error!("Failed to register IAM metrics: {}", e);
        InternalError
    })?;

    // ... encode & return ...
}
```

---

## 4. Grafana ダッシュボード設計

### 4.1 IAM パネル構成

既存の Helm ConfigMap (`grafana-dashboard.yaml`) に以下の IAM Row を追加する。

```
┌─────────────────────────────────────────────────────────────────────┐
│                        IAM Overview Row (y=24)                      │
├──────────────────┬─────────────────┬─────────────────┬──────────────┤
│ Auth Success/Fail│ Auth Error Types│ Auth Latency p99│ Policy Eval  │
│ (Stat Panel)     │ (Pie Chart)     │ (Gauge)         │ Allow/Deny   │
│ w=6, h=6         │ w=6, h=6        │ w=6, h=6        │ w=6, h=6     │
├──────────────────┴─────────────────┴─────────────────┴──────────────┤
│                     IAM Details Row (y=30)                          │
├──────────────────────────────┬──────────────────────────────────────┤
│ Auth Rate Over Time          │ Policy Evaluation Latency            │
│ (Timeseries, w=12, h=8)     │ (Timeseries w/ Heatmap, w=12, h=8)  │
├──────────────────────────────┴──────────────────────────────────────┤
│                     STS & OIDC Row (y=38)                           │
├──────────────────┬─────────────────┬────────────────────────────────┤
│ STS Request Rate │ STS Error Types │ OIDC JWKS Status               │
│ (Timeseries)     │ (Bar Chart)     │ (Stat: last fetch age)         │
│ w=8, h=8         │ w=8, h=8        │ w=8, h=8                       │
└──────────────────┴─────────────────┴────────────────────────────────┘
```

### 4.2 主要パネルの PromQL クエリ

#### Panel 1: Auth Success Rate (Stat)
```promql
sum(rate(iam_auth_requests_total{result="success"}[5m]))
/
sum(rate(iam_auth_requests_total[5m]))
```
- 表示形式: パーセンテージ
- しきい値: 🟢 > 99%, 🟡 > 95%, 🔴 ≤ 95%

#### Panel 2: Auth Error Breakdown (Pie Chart)
```promql
sum by (error_type) (increase(iam_auth_requests_total{result="failure"}[1h]))
```

#### Panel 3: Auth Latency p99 (Gauge)
```promql
histogram_quantile(0.99, sum(rate(iam_auth_duration_seconds_bucket[5m])) by (le))
```
- 単位: 秒 (`s`)
- しきい値: 🟢 < 10ms, 🟡 < 50ms, 🔴 ≥ 50ms

#### Panel 4: Policy Evaluation Results (Stat)
```promql
# Allow rate
sum(rate(iam_policy_evaluations_total{result="allow"}[5m]))
# Deny rate
sum(rate(iam_policy_evaluations_total{result="deny"}[5m]))
```

#### Panel 5: Auth Rate Over Time (Timeseries)
```promql
sum(rate(iam_auth_requests_total{result="success"}[5m]))   # label: "Success"
sum(rate(iam_auth_requests_total{result="failure"}[5m]))   # label: "Failure"
```

#### Panel 6: Policy Eval Latency (Timeseries)
```promql
histogram_quantile(0.50, sum(rate(iam_policy_evaluation_duration_seconds_bucket[5m])) by (le))
histogram_quantile(0.95, sum(rate(iam_policy_evaluation_duration_seconds_bucket[5m])) by (le))
histogram_quantile(0.99, sum(rate(iam_policy_evaluation_duration_seconds_bucket[5m])) by (le))
```

#### Panel 7: STS Request Rate (Timeseries)
```promql
sum(rate(iam_sts_requests_total{result="success"}[5m]))    # label: "Success"
sum(rate(iam_sts_requests_total{result="failure"}[5m]))    # label: "Failure"
```

#### Panel 8: STS Error Breakdown (Bar Chart)
```promql
sum by (error_type) (increase(iam_sts_requests_total{result="failure"}[1h]))
```

#### Panel 9: OIDC JWKS Freshness (Stat)
```promql
time() - iam_oidc_jwks_last_fetch_timestamp
```
- 表示形式: 秒 → 可読（`2h ago`)
- しきい値: 🟢 < 1h, 🟡 < 6h, 🔴 ≥ 6h

---

## 5. アラートルール設計

### 5.1 PrometheusRule 追加

`deploy/helm/rust-hadoop/templates/prometheus-rules-iam.yaml` を新規作成する。

```yaml
{{- if .Values.monitoring.enabled }}
apiVersion: monitoring.coreos.com/v1
kind: PrometheusRule
metadata:
  name: {{ include "rust-hadoop.fullname" . }}-iam-alerts
  labels:
    {{- include "rust-hadoop.labels" . | nindent 4 }}
    release: {{ .Values.monitoring.prometheusRelease }}
spec:
  groups:
    - name: iam.rules
      rules:
        # 認証成功率が95%を下回った場合
        - alert: IAMAuthSuccessRateLow
          expr: |
            (
              sum(rate(iam_auth_requests_total{result="success"}[5m]))
              /
              sum(rate(iam_auth_requests_total[5m]))
            ) < 0.95
          for: 5m
          labels:
            severity: warning
          annotations:
            summary: "IAM authentication success rate is below 95%"
            description: >
              The IAM authentication success rate is {{ $value | humanizePercentage }}
              over the last 5 minutes.

        # 認証成功率が90%を下回った場合（Critical）
        - alert: IAMAuthSuccessRateCritical
          expr: |
            (
              sum(rate(iam_auth_requests_total{result="success"}[5m]))
              /
              sum(rate(iam_auth_requests_total[5m]))
            ) < 0.90
          for: 2m
          labels:
            severity: critical
          annotations:
            summary: "IAM authentication success rate is critically low (< 90%)"
            description: >
              The IAM auth success rate has dropped to {{ $value | humanizePercentage }}.
              This may indicate a credential misconfiguration or an attack.

        # 認証レイテンシ (p99) が 100ms を超えた場合
        - alert: IAMAuthLatencyHigh
          expr: |
            histogram_quantile(0.99,
              sum(rate(iam_auth_duration_seconds_bucket[5m])) by (le)
            ) > 0.1
          for: 5m
          labels:
            severity: warning
          annotations:
            summary: "IAM authentication p99 latency exceeds 100ms"
            description: >
              Authentication verification p99 latency is {{ $value }}s.

        # ポリシー拒否率が50%を超えた場合
        - alert: IAMPolicyDenyRateHigh
          expr: |
            (
              sum(rate(iam_policy_evaluations_total{result="deny"}[5m]))
              /
              sum(rate(iam_policy_evaluations_total[5m]))
            ) > 0.50
          for: 10m
          labels:
            severity: warning
          annotations:
            summary: "IAM policy deny rate exceeds 50%"
            description: >
              Over 50% of policy evaluations are being denied.
              This may indicate misconfigured policies or unauthorized access attempts.

        # JWKS が 6時間以上更新されていない
        - alert: IAMOidcJwksStale
          expr: |
            (time() - iam_oidc_jwks_last_fetch_timestamp) > 21600
          for: 5m
          labels:
            severity: warning
          annotations:
            summary: "OIDC JWKS has not been refreshed for over 6 hours"
            description: >
              The JWKS cache may be stale. New tokens with rotated keys
              will fail validation.

        # STS 失敗レートが異常に高い
        - alert: IAMStsFailureRateHigh
          expr: |
            (
              sum(rate(iam_sts_requests_total{result="failure"}[5m]))
              /
              sum(rate(iam_sts_requests_total[5m]))
            ) > 0.20
          for: 5m
          labels:
            severity: warning
          annotations:
            summary: "STS token issuance failure rate exceeds 20%"
            description: >
              {{ $value | humanizePercentage }} of STS requests are failing.
{{- end }}
```

---

## 6. 計装箇所マップ

以下の図は、リクエスト処理パイプライン上のメトリクス計装ポイントを示す。

```
Client Request
    │
    ▼
┌─────────────────────────────────────────────────────────┐
│                  auth_middleware                          │
│                                                          │
│  ① TLS Check ──(fail)──▶ 📊 iam_auth_requests{failure}  │
│  │                                                       │
│  ② Parse Credentials ──(fail)──▶ 📊 iam_auth_requests   │
│  │                                                       │
│  ③ Clock Skew ──(fail)──▶ 📊 iam_auth_requests          │
│  │                                                       │
│  ④ Credential Scope ──(fail)──▶ 📊 iam_auth_requests    │
│  │                                                       │
│  ⑤ STS Token Decrypt ──(fail)──▶ 📊 iam_auth_requests   │
│  │                                                       │
│  ⑥ Lookup Secret Key ──(fail)──▶ 📊 iam_auth_requests   │
│  │                                                       │
│  ⑦ Verify Signature ──────────▶ 📊 iam_auth_requests    │
│     │                              📊 iam_auth_duration  │
│     │                                                    │
│     ▼ (success)                                          │
│  ⑧ Policy Evaluation ──────────▶ 📊 iam_policy_*        │
│     │                                                    │
│     ▼                                                    │
│   Next Handler                                           │
└─────────────────────────────────────────────────────────┘

STS Request (sts_handler)
    │
    ▼
┌─────────────────────────────────────────────────────────┐
│                  sts_handler                              │
│                                                          │
│  ⑨ Validate OIDC Token ──────▶ 📊 iam_oidc_validations  │
│  │                                                       │
│  ⑩ Check Trust Policy ────────▶ 📊 iam_policy_*         │
│  │                                                       │
│  ⑪ Generate Credentials ──────▶ 📊 iam_sts_requests     │
│     │                              📊 iam_sts_duration   │
│     │                              📊 iam_sts_sessions   │
│     ▼                                                    │
│   STS Response                                           │
└─────────────────────────────────────────────────────────┘

OIDC Subsystem (oidc.rs)
    │
    ▼
┌─────────────────────────────────────────────────────────┐
│                  OidcValidator                            │
│                                                          │
│  ⑫ fetch_jwks() ──────────────▶ 📊 iam_oidc_jwks_*     │
│                                                          │
└─────────────────────────────────────────────────────────┘
```

---

## 7. 実装工数とスケジュール

| フェーズ | 作業内容 | 推定工数 |
|:---------|:---------|:---------|
| Phase 1 | `iam_metrics.rs` 新規作成 + `main.rs` 登録 | 0.5 日 |
| Phase 2 | `auth_middleware.rs` 計装 | 1 日 |
| Phase 3 | `sts_handler.rs` 計装 | 0.5 日 |
| Phase 4 | `oidc.rs` 計装（オプション feature flag）| 0.5 日 |
| Phase 5 | Grafana ダッシュボード IAM パネル追加 | 1 日 |
| Phase 6 | PrometheusRule アラート追加 | 0.5 日 |
| Phase 7 | テスト・検証 | 1 日 |
| **合計** | | **5 日** |

---

## 8. テスト計画

### 8.1 単体テスト

- メトリクスの増分確認: 認証成功/失敗後に `IAM_AUTH_REQUESTS` のカウンター増加を検証。
- ラベルの正確性: エラータイプが正しく分類されることを検証。

### 8.2 結合テスト

- `/metrics` エンドポイントでIAMメトリクスがPrometheus形式で出力されることを確認。
- 認証失敗リクエスト送信後、`iam_auth_requests_total{result="failure"}` が増加していることを確認。

### 8.3 E2E テスト (手動)

- Docker Compose 環境で Prometheus + Grafana を起動。
- OIDC → STS → S3 API フローを実行し、各パネルにデータが表示されることを確認。

---

## 9. 設計上の決定事項

### 9.1 ラベルのカーディナリティ制御

> [!IMPORTANT]
> Prometheus のベストプラクティスとして、ラベルの組み合わせ（カーディナリティ）を抑制する。

- `iam_auth_requests_total` の `error_type` ラベルは **9種類** に限定（列挙型から機械的にマッピング）。
- `iam_policy_evaluations_total` の `action` ラベルも S3 API アクションに限定（約20種、固定セット）。
- **パスやユーザーIDはラベルにしない**（無限カーディナリティ回避）。

### 9.2 OIDC メトリクスの配置

`dfs-common` はライブラリクレートであり、Prometheus への直接依存は避けたい場合がある。
2つのアプローチを検討した：

| アプローチ | メリット | デメリット |
|:----------|:--------|:----------|
| A: `dfs-common` に `prometheus` を追加 (feature flag) | 正確な計測ポイント | ライブラリの依存増 |
| B: `s3_server` 側ラッパーで計測 | `dfs-common` は clean | JWKS 内部のリトライ等は計測不可 |

**推奨: アプローチ B（ラッパー方式）から開始し、必要に応じて A に昇格。**

### 9.3 `iam_sts_active_sessions_gauge` の精度

ステートレス STS 設計のため、厳密なアクティブセッション数は追跡不可。
代替として「発行件数 − 推定期限切れ件数」を近似する Gauge を提供する。
実装は「発行時 inc()、`default_duration` 後に dec() する delayed タスク」とする。

---

## 10. 今後の拡張候補

- **Structured Logging 連携**: `tracing` の `Span` にメトリクスを紐付け、分散トレーシング (Jaeger/Tempo) と統合。
- **Rate Limiting メトリクス**: 認証失敗が閾値を超えた IP をログに記録（将来的なレート制限の前段階）。
- **Dashboard as Code**: Grafana ダッシュボードを Grafonnet (Jsonnet) で管理し、CI/CD パイプラインでバージョン管理。
