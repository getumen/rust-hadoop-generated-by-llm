# Phase 3: IAM ポリシー評価エンジン (静的コンフィグ駆動) — 詳細設計

> **親ドキュメント**: [OIDC & STS Integration 設計書](iam_credentials_design.md)
> **推定工数**: 3-4日
> **前提**: Phase 2 (STS 実装) が完了していること

---

## 1. 概要

単なる「バケット名のプレフィックス一致 (`starts_with`)」では、「読み込み専用 (ReadOnly)」や「バケット一覧の許可 (`s3:ListAllMyBuckets`)」などのきめ細かい制御ができない。そこで、AWS S3 互換の**JSON ポリシー評価エンジン**を再導入しつつ、DBによるCRUD管理ではなく**起動時に読み込む静的な設定ファイル (`iam_config.json`)** で一元管理する。

---

## 2. `iam_config.json` の構造

Role と Policy を定義し、OIDC(JWT) のクレームからどの Role を Assume（引き受け）できるかを `AssumeRolePolicyDocument` (Trust Relationship) で制御する。

```json
{
  "Roles": [
    {
      "RoleName": "tenant-a-role",
      "Arn": "arn:dfs:iam:::role/tenant-a-role",
      "AssumeRolePolicyDocument": {
        "Statement": [
          {
            "Effect": "Allow",
            "Principal": { "Federated": "OIDC_ISSUER" },
            "Action": "sts:AssumeRoleWithWebIdentity",
            "Condition": {
              "ForAnyValue:StringEquals": {
                "OIDC_ISSUER:groups": ["tenant-a"]
              }
            }
          }
        ]
      },
      "Policies": [
        {
          "PolicyName": "TenantAReadWrite",
          "PolicyDocument": {
            "Statement": [
              {
                "Effect": "Allow",
                "Action": ["s3:GetObject", "s3:PutObject", "s3:ListBucket"],
                "Resource": ["arn:dfs:s3:::tenant-a-*"]
              }
            ]
          }
        }
      ]
    }
  ]
}
```

---

## 3. 評価ロジックの実装

### 3.1 AssumeRole フェーズ (Phase 2 連携)

クライアントが STS で指定した `RoleArn` が `iam_config.json` に存在し、かつ JWT のクレーム（例: `groups` 配列）が `AssumeRolePolicyDocument` の条件を満たすか検査する。

- **条件評価**: `groups` のような配列属性（Array Claims）に対しては、`ForAnyValue:StringEquals` 相当（配列内のいずれかが一致すれば OK）の評価をネイティブにサポートする。また、単一文字列としてのマッチングも「配列に含まれるか」の判定として扱う。

### 3.2 S3 リクエスト認可フェーズ

リクエストの S3 アクション（例: `s3:GetObject`）とリソース ARN が、復号したセッショントークン内の `RoleArn` に対応する JSON ポリシーで明示的に `Allow` されているか、標準的な IAM 評価アルゴリズム（Explicit Deny → Allow → Implicit Deny）で判定する。

- ポリシー自体はサーバーが起動時にロードした `iam_config.json` から `RoleArn` をキーに参照する。これによりトークンサイズの肥大化を防ぐ。

### 3.3 PolicyEvaluator インターフェース

```rust
pub struct PolicyEvaluator {
    // S3アクションとポリシーのマッチングエンジン
}

impl PolicyEvaluator {
    pub fn evaluate(&self, action: &str, resource: &str, role_arn: &str, context: &EvaluationContext) -> bool {
        // 1. role_arn に紐づくポリシーを静的設定から特定
        // 2. AWS標準の Action / Resource (ワイルドカード対応) マッチング
        // ... (実装はPhase3で構築する評価エンジンを使用)
        todo!("Policy evaluation engine will be implemented in Phase 3")
    }
}
```

### 実装ファイル

- `dfs-common/src/auth/policy.rs` — `PolicyEvaluator`, `IamConfig` 等

---

## 4. 関連する設定パラメータ

| 環境変数          | 型     | デフォルト | 説明                       |
| ----------------- | ------ | ---------- | -------------------------- |
| `IAM_CONFIG_PATH` | string | なし       | `iam_config.json` へのパス |

---

## 5. タスク一覧

- [x] `iam_config.json` 等の静的ファイル読み込み・オンメモリ保持構造（`Role` と `Policy` の定義）の実装。
- [x] AWS 標準の IAM JSON ポリシー（Effect, Action, Resource 等）を処理する `PolicyEvaluator` の実装。
- [x] `resolve_s3_action_and_resource()` ヘルパー（HTTP → S3 アクション変換）。
- [x] `auth_middleware` への認可（ポリシー評価）の統合と `AccessDenied` エラー処理。
