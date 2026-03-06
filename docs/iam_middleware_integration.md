# S3 Server ミドルウェア統合 & セキュリティ考慮事項

> **親ドキュメント**: [OIDC & STS Integration 設計書](iam_credentials_design.md)
> **対象フェーズ**: Phase 2〜3 で段階的に統合

---

## 1. ミドルウェア統合フロー

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

### 実装ファイル

- `s3_server/src/auth_middleware.rs`

---

## 2. セキュリティ考慮事項

| 脅威                     | 対策                                                                                                                                             |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| 重複 JWT のリプレイ攻撃  | STS発行時に JWT の `jti` (Token ID) をチェックし重複を排除する（ステートレス性を優先する場合は TTL 付きの外部ストア (Redis等) を将来的に導入）。 |
| Session Token の総当たり | AES-GCM (AEAD) による暗号化と整合性保護。鍵強度(256bit)と Nonce/Tag による改ざん・推測防止。                                                     |
| 期限切れトークンの再利用 | `expiration` クレームの復号・検証による拒否。                                                                                                    |
| JWTの漏洩                | HTTPSプロトコルを必須とする。`OIDC_ISSUER_URL` への通信もTLS必須。                                                                               |
