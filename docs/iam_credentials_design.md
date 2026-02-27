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
  - [11. 運用と制限](#11-運用と制限)
    - [11.1 設定の更新 (Hot Reload)](#111-設定の更新-hot-reload)
    - [11.2 トークンの失効 (Revocation)](#112-トークンの失効-revocation)
    - [11.3 セキュリティ](#113-セキュリティ)

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

| 特徴                   | 旧設計 (フルスクラッチIAM)     | 新設計 (OIDC + STS)                          |
| :--------------------- | :----------------------------- | :------------------------------------------- |
| **ユーザ情報の保存先** | S3サーバー内蔵の RocksDB       | 外部の IdP (Keycloak, Auth0 等)              |
| **管理 API**           | 自作REST API (`/iam/users` 等) | IdP の管理コンソールを利用 (開発不要)        |
| **認証方式**           | AWS Signature V4 のみ          | IdP での OIDC ログイン → STS で SigV4 に変換 |
| **状態管理 (State)**   | RocksDB による永続化が必須     | サーバー側状態は不要 (トークンに内包)        |

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
│  │  STS Manager    ├────▶│ クレームと一時 SecretKey をパッキング・暗号化   │  │
│  └───────┬─────────┘     │ して SessionToken を生成 (ステートレス)       │  │
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
  1. `AssumeRoleWithWebIdentity` 実行時、S3 サーバー群で共有する内部共通鍵（`STS_SIGNING_KEY` 等）を使い、JWT のクレーム（`sub`, `groups`等）、引き受ける `RoleArn`、生成した一時秘密鍵（`temp_secret_key`）、有効期限をパッキング・暗号化した「内部 SessionToken」を生成する。
  2. クライアントからのS3リクエストに `x-amz-security-token` ヘッダーが付与されていた場合、その内部共通鍵で構成要素を復号・整合性検証し、抽出された `RoleArn` に紐づくポリシーを**サーバー側の静的設定 (`iam_config.json`)** から特定して認可処理を行う。
  3. **注記**: トークン自体に巨大なポリシー JSON を含めないことで、HTTP ヘッダのサイズ制限（一般に 8KB〜16KB）超過を回避しつつ、ステートレス性を維持する。

---
## 6. Phase 3: IAM ポリシー評価エンジン (静的コンフィグ駆動)

### 6.1 真の S3 互換に向けた静的ポリシーマッピング
単なる「バケット名のプレフィックス一致 (`starts_with`)」では、「読み込み専用 (ReadOnly)」や「バケット一覧の許可 (`s3:ListAllMyBuckets`)」などのきめ細かい制御ができない。そこで、AWS S3 互換の**JSON ポリシー評価エンジン**を再導入しつつ、DBによるCRUD管理ではなく**起動時に読み込む静的な設定ファイル (`iam_config.json`)** で一元管理する。

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

### 6.3 評価ロジックの実装
1. **AssumeRole フェーズ (Phase 2)**: クライアントが STS で指定した `RoleArn` が `iam_config.json` に存在し、かつ JWT のクレーム（例: `groups` 配列）が `AssumeRolePolicyDocument` の条件を満たすか検査する。
   - **条件評価**: `groups` のような配列属性（Array Claims）に対しては、`ForAnyValue:StringEquals` 相当（配列内のいずれかが一致すれば OK）の評価をネイティブにサポートする。また、単一文字列としてのマッチングも「配列に含まれるか」の判定として扱う。
2. **S3 リクエスト認可フェーズ**: リクエストの S3 アクション（例: `s3:GetObject`）とリソース ARN が、復号したセッショントークン内の `RoleArn` に対応する JSON ポリシーで明示的に `Allow` されているか、標準的な IAM 評価アルゴリズム（Explicit Deny → Allow → Implicit Deny）で判定する。
   - ポリシー自体はサーバーが起動時にロードした `iam_config.json` から `RoleArn` をキーに参照する。これによりトークンサイズの肥大化を防ぐ。

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

---
## 7. S3 Server ミドルウェア統合

`s3_server/src/auth_middleware.rs` を拡張する。

```
受信リクエスト
  │
  ├─ x-amz-security-token ヘッダがあるか？
  │    ├─ Yes (STSの一時クレデンシャル)
  │    │     1. SessionToken を内部共通鍵で復号し、クレーム、RoleArn、SecretKey を復元
  │    │     2. 復元した temp_secret_key で SigV4 署名検証
  │    │     3. 復元した RoleArn と属性情報を使い、PolicyEvaluator で認可判定
  │    │
  │    └─ No (永続クレデンシャル = 管理者)
  │          1. EnvCredentialProvider で SigV4署名検証
  │          2. 認可はスキップ (フルアクセス許可)
  │
  └─ リクエスト処理の続行 (または 403 Access Denied)
```

---
## 8. セキュリティ考慮事項

| 脅威                     | 対策                                                                                                                                             |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| 重複 JWT のリプレイ攻撃  | STS発行時に JWT の `jti` (Token ID) をチェックし重複を排除する（ステートレス性を優先する場合は TTL 付きの外部ストア (Redis等) を将来的に導入）。 |
| Session Token の総当たり | AES-GCM (AEAD) による暗号化と整合性保護。鍵強度(256bit)と Nonce/Tag による改ざん・推測防止。                                                     |
| 期限切れトークンの再利用 | `expiration` クレームの復号・検証による拒否。                                                                                                    |
| JWTの漏洩                | HTTPSプロトコルを必須とする。`OIDC_ISSUER_URL` への通信もTLS必須。                                                                               |

---
## 9. 実装フェーズ

### Phase 1: OIDC 連携基盤（推定2-3日）
- `dfs-common/Cargo.toml` へ `jsonwebtoken`, `reqwest` の追加。
- OIDC Discovery と JWKS のパース処理。
- JWT検証ロジックの実装。

### Phase 2: STS 実装（推定2-3日）
- `s3_server/src/sts_handler.rs` に `AssumeRoleWithWebIdentity` 追加。内部トークンの署名・暗号化には AES-GCM を使用。
- ステートレスな `SessionToken` 生成・復号ロジックの実装。将来の鍵ローテーションに向け、トークンのヘッダ部（またはメタデータ）に `KID` を付与する構造とする。

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
| `STS_SIGNING_KEY`      | string | なし       | STS セッショントークンの暗号化用共通鍵 (32 bytes)        |
| `IAM_CONFIG_PATH`      | string | なし       | `iam_config.json` へのパス                               |

---
## 11. 運用と制限

### 11.1 設定の更新 (Hot Reload)
`iam_config.json` の変更を反映させるため、プロセス再起動なしで設定を再読み込みする仕組み（例：`SIGHUP` シグナル受信時、または `notify` クレートによるファイル監視）を Phase 4 以降で検討する。

### 11.2 トークンの失効 (Revocation)
ステートレスモデルでは、発行済みのトークンを個別に無効化することは困難である。
- **基本戦略**: 有効期限 (`DurationSeconds`) を短く設定する。
- **緊急対応**: 必要であれば、`STS_SIGNING_KEY` をローテーションすることで全トークンを一括失効させる。または、特定の `jti` (JWT ID) を短期間ブラックリスト（Redis等）に入れる仕組みを将来的に検討する。

### 11.3 セキュリティ
`STS_SIGNING_KEY` は非常に重要な機密情報である。環境変数またはシークレットマネージャー（K8s Secret 等）で厳重に管理し、定期的なローテーションを推奨する。
