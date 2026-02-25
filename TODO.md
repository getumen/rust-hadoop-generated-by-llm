# Rust Hadoop DFS - TODO List

## 🔴 Tier 1: 根本的な信頼性とセキュリティ（最優先）
本番環境での稼働において「当たり前」に必要とされる、データ保護とセキュリティの基盤です。

### 1. Security & Identity (セキュリティ・認証)
S3互換サービスとしての信頼確保。
- [x] **TLS Encryption** ✅ (完了)
    - [x] `tonic` / `axum` での自署名・CA証明書による通信暗号化サポート。
    - [x] Raftノード間通信（gRPC）のTLS化。
- [ ] **S3 Signature V4 Authentication**
    - [ ] `Authorization` ヘッダーのパースと署名検証ロジックの実装（HMAC-SHA256）。
    - [ ] `X-Amz-Content-Sha256` によるペイロード整合性チェック。
- [ ] **IAM & Credentials Management**
    - [ ] AccessKey/SecretKeyを紐付けるユーザ管理DB（RocksDB等）のメタデータ層への追加。
    - [ ] バケット/パス単位のAllow/Denyポリシー評価エンジンの実装。

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
- [ ] **Server-Side Encryption (SSE)**: AES-256を用いた保管時暗号化の実装。
- [ ] **Pre-signed URLs**: 短期間有効な署名付きURLの生成と検証。

### 6. Client Interface & Efficiency
- [ ] **FUSE Mount**: `rust-fuse` 等を用いた、POSIX準拠なマウントインターフェース。
- [ ] **Storage Quotas**: ディレクトリ・バケット単位の物理容量/ファイル数制限。
- [ ] **Data Compression**: ファイル単位/バケット単位でのLZ4/Zstd圧縮。

---

## 🔘 Tier 4: 高度な自動化とコンプライアンス
大規模運用や特定の規制要件に対応するための項目。

### 7. Governance & Global Scale
- [ ] **Object Locking (WORM)**: 削除・変更を物理的に禁止する保存期間（Retention）管理。
- [ ] **Lifecycle Policies**: 日数経過に応じた自動削除・階層移動のポリシー実行。
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

**Last Updated**: 2026-02-24
**Maintainer**: Development Team
