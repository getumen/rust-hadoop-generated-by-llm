# Rust Hadoop DFS - TODO List

## 🔴 Tier 1: 根本的な信頼性とセキュリティ（最優先）
本番環境での稼働において「当たり前」に必要とされる、データ保護とセキュリティの基盤です。

### 1. Security & Identity (セキュリティ・認証)
S3互換サービスとしての信頼確保。
- [x] **TLS Encryption** ✅ (完了)
    - [x] `tonic` / `axum` での自署名・CA証明書による通信暗号化サポート。
    - [x] Raftノード間通信（gRPC）のTLS化。
- [x] **S3 Signature V4 Authentication (Core Engine)** ✅
    - [x] `CanonicalRequest` / `StringToSign` 計算ロジックの実装（HMAC-SHA256）。
    - [x] チャンク署名 (`STREAMING-AWS4-HMAC-SHA256-PAYLOAD`) への対応（ロジック実装）。
    - [x] `Authorization` ヘッダーおよびクエリパラメータからの認証情報抽出。
    - [x] タイミング攻撃対策（定数時間比較）の導入。
- [ ] **OIDC & STS Integration (IAM 代替)** — [詳細設計書](docs/iam_credentials_design.md) (※要改訂)
    - [ ] **Phase 1: OIDC (OpenID Connect) 連携基盤**（推定2-3日）
        - [ ] OIDC Provider (Keycloak等) の Discovery URLからのJWKS取得・キャッシュ処理。
        - [ ] JWT（IDトークン）の署名検証、有効期限・Audienceチェックの実装。
        - [ ] テスト用OIDC Providerモック（またはローカルKeycloakコンテナ）の設定。
    - [ ] **Phase 2: STS (Security Token Service) 実装**（推定2-3日）
        - [ ] `AssumeRoleWithWebIdentity` エンドポイントの実装（JWTから一時クレデンシャル交換）。
        - [ ] ステートレスな `SessionToken` の生成（内部共通鍵によるクレームの暗号化/署名パッキング）。
        - [ ] `AuthError` の STS 関連バリアント拡張（`ExpiredToken` 等）。
        - [ ] `auth_middleware` における `x-amz-security-token` の復号・検証の統合。
    - [ ] **Phase 3: IAM ポリシー評価エンジン (静的コンフィグ駆動)**（推定3-4日）
        - [ ] `iam_config.json` 等の静的ファイル読み込み・オンメモリ保持構造(`Role` と `Policy` の定義)の実装。
        - [ ] AWS標準のIAM JSONポリシー（Effect, Action, Resource等）を処理する `PolicyEvaluator`の実装。
        - [ ] `resolve_s3_action_and_resource()` ヘルパー（HTTP→S3アクション変換）。
        - [ ] `auth_middleware` への認可（ポリシー評価）の統合と `AccessDenied` エラー処理。
    - [ ] **Phase 4: 仕上げ**（推定1-2日）
        - [ ] ドキュメント更新（`S3_COMPATIBILITY.md`, `docs/iam_credentials_design.md` のOIDC向け改訂）。
        - [ ] HelmChartの環境変数追加（`OIDC_ISSUER_URL`, `OIDC_CLIENT_ID` 等）。
        - [ ] E2Eテスト（OIDC Login -> STS -> S3 API）のスクリプト作成 (`test_scripts/oidc_sts_test.sh`)。
- [ ] **Audit Logging (Security Event Trail)**
    - [ ] 認証・認可・IAM管理操作の監査ログをRocksDBに記録。
    - [ ] 保持期間（TTL）ベースの自動ローテーション。
- [ ] **IAM Observability**
    - [ ] IAM メトリクス（認証成功/失敗率、ポリシー評価レイテンシ等）の Prometheus エクスポート。
    - [ ] 既存 Grafana ダッシュボードへのIAMパネル追加。

### 2. Data Reliability (データ保護)
データの欠損や静かな破損を許さないための仕組み。
- [ ] **End-to-End Checksums**
    - [ ] Clientでの書き込み時チェックサム計算、Metaserverへの保存。
    - [ ] ChunkServerでの読み取り時・バックグラウンドでの整合性検証。
- [ ] **Background Healer (Auto-Repair)**
    - [ ] レプリカ数が不足している、またはチェックサムが不一致なブロックを抽出するスキャナー。
    - [ ] Metaserverによる不足レプリカの自動再配置命令の送出。
- [ ] **Backup & Recovery**
    - [ ] Raftログを外部ストレージに退避するアーカイブスレッド。
    - [ ] State MachineのスナップショットをS3形式で外部へ定期アップロード。

---

## 🟡 Tier 2: 実用的な性能とコストの最適化
性能向上と並行して、リソース使用効率（Bytes per Dollar）を最大化する項目です。

### 3. Core Protocol & Availability (近代化)
- [ ] **Raft Optimizations (Batching & Pipelining)**
    - [ ] `simple_raft.rs` の `AppendEntries` をバッチ化し、1回のDisk I/Oで複数ログを処理。
    - [ ] コミット応答を待たずに次のログを先行送信するパイプライニングの実装。
- [ ] **Rack Awareness**
    - [ ] ChunkServer登録時にラックID（例: `/rack-1/host-1`）をメタデータに追加。
    - [ ] レプリカ配置時に「少なくとも1つは別ラック」とするプレイスメント・ポリシーの実装。
- [ ] **Hedged Reads (Tail Latency Mitigation)**
    - [ ] 1次リクエストの応答が一定時間（例: p95レイテンシ (ms)）来ない場合、別レプリカに投げる並行リクエスト管理。
    - [ ] 最速のレスポンスをクライアントに返し、遅い方のリクエストをキャンセルするロジック。

### 4. Throughput & Storage Excellence
- [ ] **io_uring / Zero-Copy Data Path**
    - [ ] `tokio-uring` 等を用いたChunkServerの非同期ファイルI/Oの高速化。
    - [ ] `sendfile` や registered buffer を活用し、カーネル/ユーザ空間のメモリコピーを排除。
- [ ] **Intelligent Storage Tiering**
    - [ ] アクセス統計をベースに「冷えたデータ」をHDD層へ自動移行するバックグラウンドプロセス。
    - [ ] メタデータの構造最適化（SeaweedFS方式など）によるメモリ占有率の削減。

---

## 🟢 Tier 3: S3/DFS 拡張機能とユーザビリティ
実用的なストレージサービスとしての利便性を高める機能群です。

### 5. Advanced S3 Compatibility
- [ ] **Object Versioning**: `filename?versionId=...` 形式の履歴管理と削除マーカーの実装。
- [ ] **Server-Side Encryption (SSE)**: AES-256を用いた保管時暗号化の実装。 *(前提: IAM ポリシー評価の `Condition` キーで暗号化制御を行うため IAM が完了していること)*
- [ ] **Pre-signed URLs**: 短期間有効な署名付きURLの生成と検証。 *(前提: IAM & STS が完了していること)*
- [ ] **Bucket Policy**: バケット単位のリソースベースポリシー（`GET/PUT/DELETE /{bucket}?policy`）。 *(前提: IAM ポリシー評価エンジンが完了していること)*
- [ ] **Virtual-Host Style Routing**: ホスト名ベースのバケット特定とリクエスト正規化。
- [ ] **Strict Path Normalization**: S3独自のSigV4向けURIエンコード・正規化ルール（RFC 3986をベースにした「S3フレーバー」）への準拠。

### 6. Client Interface & Efficiency
- [ ] **FUSE Mount**: `rust-fuse` 等を用いた、POSIX準拠なマウントインターフェース。
- [ ] **Storage Quotas**: ディレクトリ・バケット単位の物理容量/ファイル数制限。
- [ ] **Data Compression**: ファイル単位/バケット単位でのLZ4/Zstd圧縮。

---

## 🔘 Tier 4: 高度な自動化とコンプライアンス
大規模運用や特定の規制要件に対応するための項目。

### 7. Governance & Global Scale
- [ ] **Object Locking (WORM)**: 削除・変更を物理的に禁止する保存期間（Retention）管理。 *(推奨: IAM ポリシー評価エンジンが先に完了していること)*
- [ ] **Lifecycle Policies**: 日数経過に応じた自動削除・階層移動のポリシー実行。 *(前提: IAM 権限による設定操作の制御が必要)*
- [ ] **Cross-Cluster Replication**: クラスタ間を跨いだ非同期レプリケーション。
- [ ] **Erasure Coding (RS(6,3))**: 3xレプリカから高効率なECへ、バックグラウンドでの変換。

---

## 🚀 Moonshots (Visionary)
(優先度：低)
- **Shared Log Architecture**: ログ管理の完全外部化。
- **Zero-Copy GPU Path**: GPU VRAMダイレクト転送。

---

## ✅ Completed & Archived

### Core Distributed Logic
- [x] **Namespace Sharding**: Range-based partitioning with dynamic split & merge logic.
- [x] **Multi-Raft Cluster**: Master nodes organized into shards using Raft for high availability.
- [x] **Safe Mode**: Initialization state for warm-up and cluster safety checks.
- [x] **Dynamic Membership**: Support for adding/removing Master nodes via Joint Consensus.
- [x] **Self-Healing Base**: Heartbeat-based liveness and basic block reporting.

### Testing & Quality Assurance
- [x] **Unit Testing**: Over 20 tests covering Raft state machine and core logic.
- [x] **Integration Testing**: Chaos tests (Network partitions) and consistency checkers.
- [x] **Toxiproxy Integration**: Simulating network instability in local K8s.

### Observability & Infrastructure
- [x] **Metrics & Dashboards**: Prometheus exporters for Metaserver/S3Server and Grafana dashboards.
- [x] **Alerting**: Pre-configured rules for node failure and latency spikes.
- [x] **Deployment**: Production-ready Helm Chart with PDBs, resource limits, and service monitors.

### Code Quality & Technical Debt
- [x] **RaftNode Initialization**: Refactored `RaftNode::new` to use `RaftNodeConfig` struct.
- [x] **Clippy Compliance**: Fixed all clippy warnings across meta-server, chunk-server, and test suites.
- [x] **Large Result Types**: Resolved large `Result` variant warnings by boxing large error types or adding allows.

**Last Updated**: 2026-02-26
**Maintainer**: Development Team
