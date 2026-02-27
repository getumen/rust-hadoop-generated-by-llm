# OIDC & STS Integration (IAM 代替) — 詳細設計書

> **対象タスク**: TODO.md §1 Security & Identity — OIDC & STS Integration (IAM 代替)
> **前提**: S3 Signature V4 Authentication (Core Engine) は実装済み
> **最終更新**: 2026-02-27

---

## 目次

- [OIDC \& STS Integration (IAM 代替) — 詳細設計書](#oidc--sts-integration-iam-代替--詳細設計書)
  - [目次](#目次)
  - [1. 概要と目標](#1-概要と目標)
    - [1.1 背景](#11-背景)
    - [1.2 ゴール](#12-ゴール)
  - [2. アーキテクチャシフト（自作IAMからの脱却）](#2-アーキテクチャシフト自作iamからの脱却)
  - [3. アーキテクチャ全体像](#3-アーキテクチャ全体像)
  - [4. Phase 1: OIDC (OpenID Connect) 連携基盤](#4-phase-1-oidc-openid-connect-連携基盤)
    - [4.1 JWKS (JSON Web Key Set) の取得とキャッシュ](#41-jwks-json-web-key-set-の取得とキャッシュ)
    - [4.2 JWT 検証ロジック](#42-jwt-検証ロジック)
  - [5. Phase 2: STS (Security Token Service) 実装](#5-phase-2-sts-security-token-service-実装)
    - [5.1 `AssumeRoleWithWebIdentity` エンドポイント](#51-assumerolewithwebidentity-エンドポイント)
    - [5.2 ステートレスな STS セッショントークン (AWS S3 互換の完全性)](#52-ステートレスな-sts-セッショントークン-aws-s3-互換の完全性)
  - [6. Phase 3: IAM ポリシー評価エンジン (静的コンフィグ駆動)](#6-phase-3-iam-ポリシー評価エンジン-静的コンフィグ駆動)
    - [6.1 真の S3 互換に向けた静的ポリシーマッピング](#61-真の-s3-互換に向けた静的ポリシーマッピング)
    - [6.2 `iam_config.json` の構造](#62-iam_configjson-の構造)
    - [6.3 評価ロジックの実装](#63-評価ロジックの実装)
  - [7. S3 Server ミドルウェア統合](#7-s3-server-ミドルウェア統合)
  - [8. セキュリティ考慮事項](#8-セキュリティ考慮事項)
  - [9. 実装フェーズ](#9-実装フェーズ)
    - [Phase 1: OIDC 連携基盤（推定2-3日）](#phase-1-oidc-連携基盤推定2-3日)
    - [Phase 2: STS 実装（推定2-3日）](#phase-2-sts-実装推定2-3日)
    - [Phase 3: IAM ポリシー評価エンジン（推定3-4日）](#phase-3-iam-ポリシー評価エンジン推定3-4日)
    - [Phase 4: 仕上げ（推定1-2日）](#phase-4-仕上げ推定1-2日)
  - [10. 設定パラメータ一覧](#10-設定パラメータ一覧)

---
## 1. 概要と目標

### 1.1 背景

現在の S3 サーバーは `EnvCredentialProvider` により**単一の AccessKey/SecretKey ペア**のみをサポートしている（シングルテナント状態）。
マルチテナント（複数組織や複数ユーザの同居）を実現するためには IAM 的な仕組みが必要だが、ユーザ管理・グループ管理・ポリシー管理のデータベースや CRUD API をフルスクラッチで実装するのはスコープが大きすぎる。
そこで、**Keycloak 等の外部 Identity Provider (IdP) にユーザ管理を委譲し、S3 サーバー側は OIDC と STS (Security Token Service) に特化する** モダンなアプローチを採用する。

### 1.2 ゴール

1. **OIDC 連携**: 外部 IdP が発行した JWT（IDトークン）の署名を検証する。
2. **STS 実装**: JWT を一時的な AWS クレデンシャル（AccessKey, SecretKey, SessionToken）に交換する `AssumeRoleWithWebIdentity` エンドポイントを提供する。
3. **動的認可制御**: JWT に含まれる `groups` や `tenant` 等のクレーム（属性情報）を元に、アクセス可能なバケット（リソース）を論理的に分離・評価する。

---
## 2. アーキテクチャシフト（自作IAMからの脱却）

| 特徴                   | 旧設計 (フルスクラッチIAM)     | 新設計 (OIDC + STS)                                    |
| :--------------------- | :----------------------------- | :----------------------------------------------------- |
| **ユーザ情報の保存先** | S3サーバー内蔵の RocksDB       | 外部の IdP (Keycloak, Auth0 等)                        |
| **管理 API**           | 自作REST API (`/iam/users` 等) | IdP の管理コンソールを利用 (開発不要)                  |
| **認証方式**           | AWS Signature V4 のみ          | IdP での OIDC ログイン → STS で SigV4 に変換           |
| **状態管理 (State)**   | RocksDB による永続化が必須     | 一時クレデンシャルのインメモリキャッシュのみで完結可能 |

> [!NOTE]
> `S3_ACCESS_KEY` と `S3_SECRET_KEY` による静的クレデンシャルは、**管理者（Root）用アカウント** として引き続き利用可能とする（既存実装の後方互換）。

---
## 3. アーキテクチャ全体像

```
┌──────────────┐         (1) OIDC Login (ブラウザ/CLI)        ┌──────────────┐
│  End User /  │─────────────────────────────────────────────▶│  OIDC IdP    │
│  Application │◀─────────────────────────────────────────────│  (Keycloak)  │
└──────┬───────┘         (2) ID Token (JWT) + Claims          └──────┬───────┘
       │                                                             │
       │ (3) POST /?Action=AssumeRoleWithWebIdentity                 │ (4) Fetch JWKS
       │     &WebIdentityToken=<JWT>                                 │ (公開鍵)
       ▼                                                             ▼
┌──────────────────────────────────────────────────────────────────────────┐
│                             S3 Server                                    │
│  ┌─────────────────┐     ┌────────────────────────────────────────────┐  │
│  │  OIDC Validator │◀────┤ JWT の署名・有効期限・Audience を検証          │  │
│  └─────────────────┘     └────────────────────────────────────────────┘  │
│          │                                                               │
│          ▼ (5) OK なら一時クレデンシャルを発行                                │
│  ┌─────────────────┐     ┌────────────────────────────────────────────┐  │
│  │  STS Manager    ├────▶│ メモリ内キャッシュ (LRU / TTL) に             │  │
│  └───────┬─────────┘     │ SessionRecord を保存                       │  │
│          │               └────────────────────────────────────────────┘  │
└──────────┼───────────────────────────────────────────────────────────────┘
       (6) │ 200 OK
           │ <AccessKeyId>, <SecretAccessKey>, <SessionToken>, <Expiration>
           ▼
┌──────────────┐
│  Client      │ (7) 一時クレデンシャルを使って通常の SigV4 リクエストを実施
│  (AWS SDK)   │─────────────────────────────────────────────────────────▶ (S3 Operations)
└──────────────┘
```

---
## 4. Phase 1: OIDC (OpenID Connect) 連携基盤

### 4.1 JWKS (JSON Web Key Set) の取得とキャッシュ
OIDC Provider の検証には公開鍵が必要。
1. `OIDC_ISSUER_URL` の `/.well-known/openid-configuration` にアクセスし `jwks_uri` を取得。
2. 公開鍵（JWKS）をフェッチし、メモリ上にキャッシュする。定期的なバックグラウンド更新（キーローテーション対応）を行う。

### 4.2 JWT 検証ロジック
Rust の `jsonwebtoken` クレート等を利用して以下を検証する。
- **Signature**: キャッシュされた JWKS を使った署名検証。
- **Expiration (`exp`)**: トークンが期限切れでないこと。
- **Audience (`aud`)**: `OIDC_CLIENT_ID` と一致すること。
- **Issuer (`iss`)**: `OIDC_ISSUER_URL` と一致すること。

---
## 5. Phase 2: STS (Security Token Service) 実装

### 5.1 `AssumeRoleWithWebIdentity` エンドポイント
S3 サーバー直下の AWS STS 互換エンドポイント。

**リクエスト例:**
```http
POST /?Action=AssumeRoleWithWebIdentity&DurationSeconds=3600&RoleArn=arn:dfs:iam:::role/tenant-a-role&RoleSessionName=app1&WebIdentityToken=<JWT> HTTP/1.1
Host: s3.example.com
```

**レスポンス例:**
```xml
<AssumeRoleWithWebIdentityResponse>
  <AssumeRoleWithWebIdentityResult>
    <Credentials>
      <AccessKeyId>ASIA...</AccessKeyId>
      <SecretAccessKey>...</SecretAccessKey>
      <SessionToken>...</SessionToken>
      <Expiration>2026-02-27T12:00:00Z</Expiration>
    </Credentials>
    <SubjectFromWebIdentityToken>user-id-from-jwt</SubjectFromWebIdentityToken>
    <AssumedRoleUser>
      <AssumedRoleId>AROA...:app1</AssumedRoleId>
      <Arn>arn:dfs:sts:::assumed-role/tenant-a-role/app1</Arn>
    </AssumedRoleUser>
  </AssumeRoleWithWebIdentityResult>
</AssumeRoleWithWebIdentityResponse>
```

### 5.2 ステートレスな STS セッショントークン (AWS S3 互換の完全性)
AWS S3 と同様に「S3 サーバー群がスケールアウトしても、どのノードでも STS トークンを検証可能」にするため、一時クレデンシャルの状態（セッション）をメモリや外部DBに保存しない**ステートレスアーキテクチャ**を採用する。

`SessionToken` 自体の中に、元の JWT のクレーム情報を含めて暗号化（または内部署名付きの独自 JWT、例えば Fernet トークンのように）してクライアントに返却する。

* **メリット**: S3 サーバーの再起動でセッションが揮発しない。クラスタ内の全 S3 ノードで中央DB（RocksDB 等）なしに一貫したトークン検証が可能。
* **仕組み**:
  1. `AssumeRoleWithWebIdentity` 実行時、S3 サーバー群で共有する内部共通鍵（`STS_SIGNING_KEY` 等）を使い、クレームと有効期限をパッキングした「内部 SessionToken」を生成する。
  2. クライアントからのS3リクエストに `x-amz-security-token` ヘッダーが付与されていた場合、その内部共通鍵で構成要素を復号・検証し、ポリシー評価に必要な属性（`groups` 等）をインメモリに復元してリクエストを処理する。

---
## 6. Phase 3: IAM ポリシー評価エンジン (静的コンフィグ駆動)

### 6.1 真の S3 互換に向けた静的ポリシーマッピング
単なる「バケット名のプレフィックス一致 (`start_with`)」では、「読み込み専用 (ReadOnly)」や「バケット一覧の許可 (`s3:ListAllMyBuckets`)」などのきめ細かい制御ができない。そこで、AWS S3 互換の**JSON ボリシー評価エンジン**を再導入しつつ、DBによるCRUD管理ではなく**起動時に読み込む静的な設定ファイル (`iam_config.json`)** で一元管理する。

### 6.2 `iam_config.json` の構造
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
              "StringEquals": {
                "OIDC_ISSUER:groups": "tenant-a"
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

### 6.3 評価ロジックの実装
1. **AssumeRole フェーズ (Phase 2)**: クライアントが STS で指定した `RoleArn` が `iam_config.json` に存在し、かつ JWT の `groups` 等が `AssumeRolePolicyDocument` の条件を満たすか検査する。満たせば、Roleの `Policies` を SessionToken にエンコードして返却。
2. **S3 リクエスト認可フェーズ**: リクエストの S3 アクション（例: `s3:PutObject`）とリソース ARN が、復号したセッショントークン内の JSON ポリシーで明示的に `Allow` されているか、標準的な IAM 評価アルゴリズム（Explicit Deny → Allow → Implicit Deny）で判定する。

```rust
pub struct PolicyEvaluator {
    // S3アクションとポリシーのマッチングエンジン
}

impl PolicyEvaluator {
    pub fn evaluate(&self, action: &str, resource: &str, session_policies: &[PolicyDocument]) -> bool {
        // AWS標準の Action / Resource (ワイルドカード対応) マッチング
        // ... (実装はPhase5相当のエンジンを流用)
    }
}
```

---
## 7. S3 Server ミドルウェア統合

`s3_server/src/auth_middleware.rs` を拡張する。

```
受信リクエスト
  │
  ├─ x-amz-security-token ヘッダがあるか？
  │    ├─ Yes (STSの一時クレデンシャル)
  │    │     1. SessionTokenManager でトークンの有効性を検証
  │    │     2. ヒットした SessionRecord の temp_secret_key で SigV4署名検証
  │    │     3. PolicyEvaluator に SessionRecord.claims を渡し、認可(Allow/Deny)を判定
  │    │
  │    └─ No (永続クレデンシャル = 管理者)
  │          1. EnvCredentialProvider で SigV4署名検証
  │          2. 認可はスキップ (フルアクセス許可)
  │
  └─ リクエスト処理の続行 (または 403 Access Denied)
```

---
## 8. セキュリティ考慮事項

| 脅威                     | 対策                                                                         |
| ------------------------ | ---------------------------------------------------------------------------- |
| 重複 JWT のリプレイ攻撃  | STS発行時に JWT の `jti` (Token ID) をチェックし一度きりにする（オプション） |
| Session Token の総当たり | 256文字長以上の cryptographically secure なランダム文字列                    |
| 期限切れトークンの再利用 | `expiration` チェック + `tokio::time` による掃除タスク。                     |
| JWTの漏洩                | HTTPSプロトコルを必須とする。`OIDC_ISSUER_URL` への通信もTLS必須。           |

---
## 9. 実装フェーズ

### Phase 1: OIDC 連携基盤（推定2-3日）
- `dfs-common/Cargo.toml` へ `jsonwebtoken`, `reqwest` の追加。
- OIDC Discovery と JWKS のパース処理。
- JWT検証ロジックの実装。

### Phase 2: STS 実装（推定2-3日）
- `s3_server/src/sts_handler.rs` に `AssumeRoleWithWebIdentity` 追加。
- インメモリの `SessionTokenManager` 実装 (STS発行と保持)。

### Phase 3: IAM ポリシー評価エンジン（推定3-4日）
- `iam_config.json` 等の静的ファイル読み込み・オンメモリ保持構造の実装。
- `dfs-common/src/auth/policy_eval.rs` の作成（AWS標準JSONポリシーの評価エンジン）。
- `s3_server/src/auth_middleware.rs` への組み込み。
- アクション(HTTP)とS3リソース名のマッピングヘルパー関数の作成。

### Phase 4: 仕上げ（推定1-2日）
- `S3_COMPATIBILITY.md` の更新。
- 単体テスト・統合テストスクリプト（Keycloakモック等を使用）の作成。

---
## 10. 設定パラメータ一覧

| 環境変数               | 型     | デフォルト | 説明                                                     |
| ---------------------- | ------ | ---------- | -------------------------------------------------------- |
| `OIDC_ISSUER_URL`      | string | なし       | OIDC プロバイダの Issuer URL (例: `https://keycloak...`) |
| `OIDC_CLIENT_ID`       | string | なし       | 本 S3 サーバーの Client ID (audience 検証用)             |
| `STS_DEFAULT_DURATION` | u64    | `3600`     | デフォルトの一時セッション有効期限（秒）                 |
| `STS_MAX_DURATION`     | u64    | `43200`    | 最大の一時セッション有効期限（秒）                       |
