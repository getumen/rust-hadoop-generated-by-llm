# Phase 1: OIDC (OpenID Connect) 連携基盤 — 詳細設計

> **親ドキュメント**: [OIDC & STS Integration 設計書](iam_credentials_design.md)
> **推定工数**: 2-3日
> **前提**: S3 Signature V4 Authentication (Core Engine) は実装済み

---

## 1. 概要

外部 IdP (Keycloak 等) が発行した JWT（IDトークン）の署名を検証するための基盤を構築する。
これにより、S3 サーバーが外部のユーザ管理システムと連携し、トークンベースの認証が可能になる。

---

## 2. JWKS (JSON Web Key Set) の取得とキャッシュ

OIDC Provider の検証には公開鍵が必要。

1. `OIDC_ISSUER_URL` の `/.well-known/openid-configuration` にアクセスし `jwks_uri` を取得。
2. 公開鍵（JWKS）をフェッチし、メモリ上にキャッシュする。定期的なバックグラウンド更新（キーローテーション対応）を行う。

### 実装ファイル

- `dfs-common/src/auth/oidc.rs` — `OidcValidator` 構造体

---

## 3. JWT 検証ロジック

Rust の `jsonwebtoken` クレート等を利用して以下を検証する。

- **Signature**: キャッシュされた JWKS を使った署名検証。
- **Expiration (`exp`)**: トークンが期限切れでないこと。
- **Audience (`aud`)**: `OIDC_CLIENT_ID` と一致すること。
- **Issuer (`iss`)**: `OIDC_ISSUER_URL` と一致すること。

---

## 4. 関連する設定パラメータ

| 環境変数          | 型     | デフォルト | 説明                                                     |
| ----------------- | ------ | ---------- | -------------------------------------------------------- |
| `OIDC_ISSUER_URL` | string | なし       | OIDC プロバイダの Issuer URL (例: `https://keycloak...`) |
| `OIDC_CLIENT_ID`  | string | なし       | 本 S3 サーバーの Client ID (audience 検証用)             |

---

## 5. 依存クレート

- `jsonwebtoken` — JWT のパース・署名検証
- `reqwest` — JWKS エンドポイントへの HTTP アクセス

---

## 6. タスク一覧

- [x] `dfs-common/Cargo.toml` へ `jsonwebtoken`, `reqwest` の追加。
- [x] OIDC Discovery と JWKS のパース処理。
- [x] JWT 検証ロジックの実装。
- [x] テスト用 OIDC Provider モック（またはローカル Keycloak コンテナ）の設定。
