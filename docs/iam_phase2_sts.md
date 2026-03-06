# Phase 2: STS (Security Token Service) 実装 — 詳細設計

> **親ドキュメント**: [OIDC & STS Integration 設計書](iam_credentials_design.md)
> **推定工数**: 2-3日
> **前提**: Phase 1 (OIDC 連携基盤) が完了していること

---

## 1. 概要

外部 IdP が発行した JWT を、一時的な AWS 互換クレデンシャル（AccessKey, SecretKey, SessionToken）に交換する `AssumeRoleWithWebIdentity` エンドポイントを提供する。

---

## 2. `AssumeRoleWithWebIdentity` エンドポイント

S3 サーバー直下の AWS STS 互換エンドポイント。

### リクエスト例

```http
POST /?Action=AssumeRoleWithWebIdentity&DurationSeconds=3600&RoleArn=arn:dfs:iam:::role/tenant-a-role&RoleSessionName=app1&WebIdentityToken=<JWT> HTTP/1.1
Host: s3.example.com
```

### レスポンス例

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

### 実装ファイル

- `s3_server/src/sts_handler.rs` — エンドポイントハンドラ

---

## 3. ステートレスな STS セッショントークン

AWS S3 と同様に「S3 サーバー群がスケールアウトしても、どのノードでも STS トークンを検証可能」にするため、一時クレデンシャルの状態（セッション）をメモリや外部DBに保存しない**ステートレスアーキテクチャ**を採用する。

`SessionToken` 自体の中に、元の JWT のクレーム情報を含めて暗号化（または内部署名付きの独自 JWT、例えば Fernet トークンのように）してクライアントに返却する。

### メリット
- S3 サーバーの再起動でセッションが揮発しない。
- クラスタ内の全 S3 ノードで中央DB（RocksDB 等）なしに一貫したトークン検証が可能。

### 仕組み

1. `AssumeRoleWithWebIdentity` 実行時、S3 サーバー群で共有する内部共通鍵（`STS_SIGNING_KEY` 等）を使い、JWT のクレーム（`sub`, `groups`等）、引き受ける `RoleArn`、生成した一時秘密鍵（`temp_secret_key`）、有効期限をパッキング・暗号化した「内部 SessionToken」を生成する。
2. クライアントからのS3リクエストに `x-amz-security-token` ヘッダーが付与されていた場合、その内部共通鍵で構成要素を復号・整合性検証し、抽出された `RoleArn` に紐づくポリシーを**サーバー側の静的設定 (`iam_config.json`)** から特定して認可処理を行う。
3. **注記**: トークン自体に巨大なポリシー JSON を含めないことで、HTTP ヘッダのサイズ制限（一般に 8KB〜16KB）超過を回避しつつ、ステートレス性を維持する。

### 実装ファイル

- `dfs-common/src/auth/sts.rs` — `StsTokenManager` (AES-GCM 暗号化/復号)

---

## 4. 関連する設定パラメータ

| 環境変数               | 型     | デフォルト | 説明                                              |
| ---------------------- | ------ | ---------- | ------------------------------------------------- |
| `STS_DEFAULT_DURATION` | u64    | `3600`     | デフォルトの一時セッション有効期限（秒）          |
| `STS_MAX_DURATION`     | u64    | `43200`    | 最大の一時セッション有効期限（秒）                |
| `STS_SIGNING_KEY`      | string | なし       | STS セッショントークンの暗号化用共通鍵 (32 bytes) |

---

## 5. 依存クレート

- `aes-gcm` — AES-GCM (AEAD) による暗号化・整合性保護
- `rand` — 一時 SecretKey やランダム Nonce の生成
- `base64` — トークンのエンコード/デコード

---

## 6. タスク一覧

- [x] `s3_server/src/sts_handler.rs` に `AssumeRoleWithWebIdentity` 追加。
- [x] ステートレスな `SessionToken` 生成・復号ロジックの実装。
- [x] 将来の鍵ローテーションに向け、トークンのヘッダ部（またはメタデータ）に `KID` を付与する構造とする。
- [x] `AuthError` の STS 関連バリアント拡張（`ExpiredToken` 等）。
- [x] `auth_middleware` における `x-amz-security-token` の復号・検証の統合。
