# OIDC & STS Integration (IAM 代替) — 設計概要

> **対象タスク**: TODO.md §1 Security & Identity — OIDC & STS Integration (IAM 代替)
> **前提**: S3 Signature V4 Authentication (Core Engine) は実装済み
> **最終更新**: 2026-03-05

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

## 4. 詳細設計ドキュメント

各フェーズの詳細設計は以下の個別ドキュメントを参照。

| フェーズ | ドキュメント                                                     | 概要                                                   |
| :------- | :--------------------------------------------------------------- | :----------------------------------------------------- |
| Phase 1  | [OIDC 連携基盤](iam_phase1_oidc.md)                              | JWKS 取得・キャッシュ、JWT 検証ロジック                |
| Phase 2  | [STS 実装](iam_phase2_sts.md)                                    | `AssumeRoleWithWebIdentity`、ステートレス SessionToken |
| Phase 3  | [ポリシー評価エンジン](iam_phase3_policy_engine.md)              | `iam_config.json`、IAM ポリシー評価                    |
| 横断     | [ミドルウェア統合 & セキュリティ](iam_middleware_integration.md) | `auth_middleware` 拡張、セキュリティ脅威対策           |

---

## 5. 実装フェーズ

### Phase 1: OIDC 連携基盤（推定2-3日） → [詳細](iam_phase1_oidc.md)
- `dfs-common/Cargo.toml` へ `jsonwebtoken`, `reqwest` の追加。
- OIDC Discovery と JWKS のパース処理。
- JWT検証ロジックの実装。

### Phase 2: STS 実装（推定2-3日） → [詳細](iam_phase2_sts.md)
- `s3_server/src/sts_handler.rs` に `AssumeRoleWithWebIdentity` 追加。内部トークンの署名・暗号化には AES-GCM を使用。
- ステートレスな `SessionToken` 生成・復号ロジックの実装。将来の鍵ローテーションに向け、トークンのヘッダ部（またはメタデータ）に `KID` を付与する構造とする。

### Phase 3: IAM ポリシー評価エンジン（推定3-4日） → [詳細](iam_phase3_policy_engine.md)
- `iam_config.json` 等の静的ファイル読み込み・オンメモリ保持構造の実装。
- `dfs-common/src/auth/policy.rs` の作成（AWS標準JSONポリシーの評価エンジン）。
- `s3_server/src/auth_middleware.rs` への組み込み。
- アクション(HTTP)とS3リソース名のマッピングヘルパー関数の作成。

### Phase 4: 仕上げ（推定1-2日）
- `S3_COMPATIBILITY.md` の更新。
- 単体テスト・統合テストスクリプト（Keycloakモック等を使用）の作成。

---

## 6. 設定パラメータ一覧

| 環境変数               | 型     | デフォルト | 説明                                                     |
| ---------------------- | ------ | ---------- | -------------------------------------------------------- |
| `OIDC_ISSUER_URL`      | string | なし       | OIDC プロバイダの Issuer URL (例: `https://keycloak...`) |
| `OIDC_CLIENT_ID`       | string | なし       | 本 S3 サーバーの Client ID (audience 検証用)             |
| `STS_DEFAULT_DURATION` | u64    | `3600`     | デフォルトの一時セッション有効期限（秒）                 |
| `STS_MAX_DURATION`     | u64    | `43200`    | 最大の一時セッション有効期限（秒）                       |
| `STS_SIGNING_KEY`      | string | なし       | STS セッショントークンの暗号化用共通鍵 (32 bytes)        |
| `IAM_CONFIG_PATH`      | string | なし       | `iam_config.json` へのパス                               |

---

## 7. 運用と制限

### 7.1 設定の更新 (Hot Reload)
`iam_config.json` の変更を反映させるため、プロセス再起動なしで設定を再読み込みする仕組み（例：`SIGHUP` シグナル受信時、または `notify` クレートによるファイル監視）を Phase 4 以降で検討する。

### 7.2 トークンの失効 (Revocation)
ステートレスモデルでは、発行済みのトークンを個別に無効化することは困難である。
- **基本戦略**: 有効期限 (`DurationSeconds`) を短く設定する。
- **緊急対応**: 必要であれば、`STS_SIGNING_KEY` をローテーションすることで全トークンを一括失効させる。または、特定の `jti` (JWT ID) を短期間ブラックリスト（Redis等）に入れる仕組みを将来的に検討する。

### 7.3 セキュリティ
`STS_SIGNING_KEY` は非常に重要な機密情報である。環境変数またはシークレットマネージャー（K8s Secret 等）で厳重に管理し、定期的なローテーションを推奨する。
