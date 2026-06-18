# Refactor Instructions

## Objective

既存の動作仕様を一切壊さずに技術的負債を段階的に解消し、今後の機能追加・変更が安全かつ容易にできる状態にすること。

**目標ではないもの**: 見た目の綺麗さ、全面書き換え、アーキテクチャの刷新。

---

## Project Understanding

### What This Project Does

GFS/HDFSにインスパイアされた分散ファイルシステム(DFS)のRust実装。Range-based dynamic sharding、Raft consensus (HA)、3x pipeline replication、S3互換APIを持つ。

### System Topology

```
ConfigServer (3-node Raft) — ShardMap管理
  ├── Shard 1 Masters (3-node Raft) — paths "" to "/m"
  └── Shard 2 Masters (3-node Raft) — paths "/m" to ""
           │
      ChunkServers (3x replication pipeline)
```

Client → ConfigServer (ShardMap取得) → Master (metadata RPC) → ChunkServer (data read/write)

### Workspace Crates (5 crates)

| Crate | Path | Role | LOC (approx) |
|-------|------|------|-------------|
| metaserver | `dfs/metaserver/` | Master + ConfigServer + Raft consensus | ~7,000 |
| chunkserver | `dfs/chunkserver/` | Block storage, replication, checksum, LRU cache | ~1,200 |
| client | `dfs/client/` | Client library + CLI tool | ~1,750 |
| common | `dfs/common/` | Sharding, TLS, auth (SigV4, OIDC, STS, IAM), audit, telemetry | ~2,300 |
| s3_server | `dfs/s3_server/` | Axum-based S3 REST API gateway | ~2,700 |

### Entry Points (Binaries)

| Binary | Source | Default Port |
|--------|--------|-------------|
| master | `dfs/metaserver/src/bin/master.rs` | gRPC 50051, HTTP 8080 |
| config_server | `dfs/metaserver/src/bin/config_server.rs` | gRPC 50052, HTTP 8081 |
| chunkserver | `dfs/chunkserver/src/bin/chunkserver.rs` | gRPC 50052, HTTP 8082 |
| dfs_cli | `dfs/client/src/bin/dfs_cli.rs` | N/A |
| s3-server | `dfs/s3_server/src/main.rs` | 9000 |

### Data Flow Summary

1. **Write**: Client → Master (CreateFile + AllocateBlock) → ChunkServer pipeline (CS1→CS2→CS3) → Master (CompleteFile)
2. **Read**: Client → Master (GetFileInfo + GetBlockLocations) → ChunkServer (ReadBlock with replica fallback)
3. **Cross-shard Rename**: Source Master (coordinator) → 2PC: Prepare → Commit/Abort to destination Master
4. **Dynamic Shard Split**: ThroughputMonitor detects hot prefix → Raft commit → ConfigServer registers new shard → IngestMetadata to new peers

### External Dependencies (key)

- `tonic 0.12` / `prost 0.13`: gRPC
- `tokio 1.0`: async runtime
- `axum 0.8`: HTTP server (S3, health, metrics)
- `rocksdb 0.24`: persistent state (Raft log, audit)
- `rustls 0.23`: TLS
- `prometheus 0.14`: metrics
- `jsonwebtoken 10.3`: OIDC JWT validation
- `aes-gcm 0.10.3`: STS token encryption

---

## Behaviors To Preserve

以下は絶対に壊してはならない既存挙動である。各フェーズで必ずこれらの検証を通すこと。

1. **Raft consensus**: Leader election, log replication, snapshot, joint consensus membership change
2. **Cross-shard rename**: 2PC (Prepare → Commit/Abort) transaction record lifecycle
3. **Pipeline replication**: Client → CS1 → CS2 → CS3 の連鎖書き込み
4. **Safe mode**: 起動時の書き込み拒否 → 99%ブロック報告で解除
5. **Dynamic shard split/merge**: ThroughputMonitor → ConfigServer → IngestMetadata
6. **S3 API互換性**: PutObject, GetObject, ListObjects (V1/V2), multipart upload, CopyObject, DeleteObjects
7. **S3 Signature V4認証**: Authorization header + query parameter + chunked encoding
8. **OIDC/STS/IAM**: token validation → AssumeRoleWithWebIdentity → policy evaluation
9. **Audit logging**: HMAC hash chaining, tamper-evidence, secondary indexing
10. **End-to-end checksums**: Client-side CRC32 → ChunkServer verification → background scrubber
11. **TLS**: gRPC + HTTP全通信のTLS対応
12. **gRPC API contract**: `proto/dfs.proto` のメッセージ・サービス定義

---

## Non-Negotiables

- `proto/dfs.proto` のメッセージ定義を変更しない（下位互換を壊す）
- RocksDBのストレージフォーマットを変更しない（既存データが読めなくなる）
- 公開gRPCサービスのシグネチャを変更しない
- S3 APIのレスポンスXML構造を変更しない
- STS/SigV4の暗号処理ロジックを変更しない
- Raft log entry formatのシリアライズ形式を変更しない（既存snapshot/logとの互換性）

---

## Stop And Ask Conditions

以下に該当する場合、実装を止めて人間に質問すること。

1. `proto/dfs.proto` の変更が必要になった場合
2. `Command` enum や `MasterCommand` enum のバリアント変更が必要になった場合（Raft log互換性に影響）
3. RocksDB のキーフォーマットやColumn Family構成の変更が必要になった場合
4. S3 APIレスポンスのXML構造変更が必要になった場合
5. 認証・署名・暗号化ロジックの変更が必要になった場合
6. テストが失敗し、原因がリファクタリングによるものか既存バグか判断できない場合
7. 複数の設計案があり、どちらが正しいかコードから判断できない場合
8. 削除候補のコードが本当に不要か確証が持てない場合

---

## Baseline Commands

作業開始前と各フェーズ完了後に必ず実行し、結果を記録すること。

```bash
# 1. Working tree is clean
git status

# 2. Build succeeds
cargo build --release 2>&1 | tail -5

# 3. Clippy passes (CI gate)
cargo clippy -- -D warnings 2>&1 | tail -10

# 4. Format check passes (CI gate)
cargo fmt --all -- --check

# 5. Unit tests pass
cargo test --lib 2>&1 | tail -20

# 6. (Optional, Docker required) Integration tests
# docker compose up -d --build && sleep 15
# ./test_scripts/cross_shard_test.sh
# ./test_scripts/rename_test.sh
# docker compose down -v
```

---

## Debt Map

### D1: `connect_endpoint` の重複 (LOW RISK, HIGH CONFIDENCE)

**根拠**:
- `dfs/client/src/mod.rs:87-116` — `Client::connect_endpoint()`
- `dfs/chunkserver/src/chunkserver.rs:79-113` — `MyChunkServer::connect_endpoint()`
- `dfs/common/src/security.rs:63` — `connect_endpoint()` (既に共通関数が存在)

**なぜ負債か**: 同一ロジック(URL scheme変換 + TLS設定)が3箇所に存在。common crateに既に `security::connect_endpoint()` があるのに、client/chunkserverが独自実装を持っている。

**影響範囲**: client, chunkserver crate
**変更リスク**: 低。既存の `security::connect_endpoint()` に委譲するだけ。
**検証**: `cargo test --lib`, 統合テスト (rename, cross_shard)
**判定**: 実装してよい

---

### D2: `eprintln!` デバッグ出力の残留 (LOW RISK, HIGH CONFIDENCE)

**根拠**:
- `dfs/common/src/sharding.rs:70` — `eprintln!("DEBUG: add_shard called for {}", shard_id)`
- `dfs/common/src/sharding.rs:78` — `eprintln!("DEBUG: add_shard peers: ...")`
- `dfs/common/src/sharding.rs:277` — `eprintln!("DEBUG: get_all_shards returning: ...")`

**なぜ負債か**: デバッグ用出力がstderrに残っている。本番環境のノイズになり、tracing crateを使うべき。
**影響範囲**: common crate (sharding module)
**変更リスク**: 極低。出力先の変更のみ。
**検証**: `cargo test --lib -p dfs-common`
**判定**: 実装してよい

---

### D3: `#[allow(dead_code)]` の検証 (LOW RISK, MEDIUM CONFIDENCE)

**根拠**:
- `dfs/s3_server/src/audit.rs:44, 283, 299`
- `dfs/s3_server/src/s3_types.rs:101, 104, 109, 178, 187`

**なぜ負債か**: 本当に使われていないコードか、将来使う予定のコードかが不明。suppressしているだけでは根本解決にならない。
**影響範囲**: s3_server crate
**変更リスク**: 低。ただし削除前に本当に未使用か確認が必要。
**検証**: `cargo build --release`(dead_code warningが出ないこと), `cargo test --lib -p s3-server`
**判定**: 調査のうえ実装してよい。使用箇所が見つかった場合はallowを外すだけでよい。明確に不要と判断できたもののみ削除。

---

### D4: `handlers.rs` の巨大関数 (MEDIUM RISK, HIGH CONFIDENCE)

**根拠**:
- `dfs/s3_server/src/handlers.rs` — `handle_request()` が約988行の単一関数

**なぜ負債か**: 一つの関数がS3の全オペレーション (PutObject, GetObject, ListObjects, MPU, Copy, Delete等) を巨大なif/elseチェーンで処理している。可読性が低く、個別オペレーションの変更が困難。
**影響範囲**: s3_server crate
**変更リスク**: 中。関数分割は安全だが、分割ミスでルーティングが壊れる可能性あり。
**改善案**: `handle_request()` はルーティングのみに徹し、各S3オペレーションを独立した関数に抽出する(既にいくつかは抽出済み)。
**検証**: `cargo test --lib -p s3-server`, S3統合テスト (`test_scripts/run_s3_test.sh`)
**判定**: 実装してよい（関数抽出のみ。ロジック変更は行わない）

---

### D5: `simple_raft.rs` の巨大ファイル (MEDIUM RISK, MEDIUM CONFIDENCE)

**根拠**:
- `dfs/metaserver/src/simple_raft.rs` — 3,268行の単一ファイル

**なぜ負債か**: Raft consensus、RPC処理、Snapshot、Membership change、State Machine apply が全て一つのファイルに詰め込まれている。
**影響範囲**: metaserver crate (Raftの根幹)
**変更リスク**: 中〜高。Raft実装は正確性が極めて重要で、分割ミスが致命的なバグにつながりうる。
**改善案**: ファイルをモジュールに分割（例: `raft/node.rs`, `raft/log.rs`, `raft/snapshot.rs`, `raft/membership.rs`, `raft/rpc.rs`）。ロジック変更は一切行わない。
**検証**: `cargo test --lib -p dfs-metaserver`, `cargo test -p dfs-metaserver` (property-based tests等含む)
**判定**: 提案に留める。分割方法を示し、人間の承認を得てから実装すること。

---

### D6: `auth_middleware.rs` の複雑さ (MEDIUM RISK, HIGH CONFIDENCE)

**根拠**:
- `dfs/s3_server/src/auth_middleware.rs` — `auth_middleware()` が約407行の単一関数

**なぜ負債か**: 認証フロー全体 (SigV4解析 → 鍵導出 → 署名検証 → STS検証 → IAMポリシー評価 → Audit記録) が1関数に凝縮されている。
**影響範囲**: s3_server crate (認証)
**変更リスク**: 中。認証ロジックの分割ミスはセキュリティ影響あり。
**改善案**: 論理ステップごとにヘルパー関数を抽出（例: `verify_signature()`, `resolve_sts_credentials()`, `evaluate_policy()`, `record_audit()`）。分岐ロジックは変更しない。
**検証**: `cargo test --lib -p s3-server`, OIDC/STS統合テスト (`test_scripts/oidc_sts_test.sh`)
**判定**: 実装してよい（関数抽出のみ。ロジック変更・分岐変更は行わない）

---

### D7: 文字列ベースのエラーコード (MEDIUM RISK, LOW CONFIDENCE)

**根拠**:
- `dfs/client/src/mod.rs:997-1022` — `"REDIRECT:<addr>"` をStatus messageから文字列解析
- `dfs/client/src/mod.rs:908-923` — `"Not Leader|<addr>"` を文字列解析
- `dfs/metaserver/src/master.rs` — Status messageに `REDIRECT:` や `Not Leader|` を埋め込み

**なぜ負債か**: gRPC Status messageの文字列パースに依存しており、フォーマット変更で静かに壊れる。
**影響範囲**: client + metaserver (相互依存)
**変更リスク**: 中〜高。両側を同時に変更する必要があり、ローリングアップデート時の互換性問題が生じうる。
**改善案**: gRPC metadata (trailer) を使うか、proto定義にredirect fieldを追加する。ただしproto変更は Non-Negotiables に抵触。
**判定**: 提案に留める。proto変更を伴うため人間の判断が必要。

---

### D8: Fire-and-forget replication (MEDIUM RISK, HIGH CONFIDENCE)

**根拠**:
- `dfs/chunkserver/src/chunkserver.rs:507-517` — WriteBlock内のpipeline replicationエラーをlog後に無視
- `dfs/chunkserver/src/chunkserver.rs:747-757` — ReplicateBlock内の同様の処理

**なぜ負債か**: Pipeline replicationが下流で失敗してもクライアントにはsuccessが返る。レプリカ不足を検出できない。
**影響範囲**: chunkserver crate (データ整合性)
**変更リスク**: 中。エラー伝播方式の変更はクライアントの挙動に影響する。
**改善案**: 少なくともmetricsにreplication失敗を記録し、Masterのheartbeatでレプリカ数を確認できるようにする。
**判定**: 提案に留める。既存の挙動（成功応答を返す）を変更するかは設計判断が必要。

---

### D9: Partial read checksum skip (LOW RISK, HIGH CONFIDENCE)

**根拠**:
- `dfs/chunkserver/src/chunkserver.rs:597` — `// TODO: Implement chunked checksum verification for partial reads`

**なぜ負債か**: 部分読み取り時にchecksum検証がスキップされており、破損データを検知できない。
**影響範囲**: chunkserver crate (データ整合性)
**変更リスク**: 低。512byteチャンク単位のchecksum構造を活用すれば、対象チャンクのみ検証可能。
**検証**: `cargo test --lib -p dfs-chunkserver`, checksum統合テスト (`test_scripts/run_checksum_test.sh`)
**判定**: 実装してよい

---

### D10: Magic number の散在 (LOW RISK, HIGH CONFIDENCE)

**根拠**:
- `dfs/client/src/mod.rs:305, 422, 560, 696` — `100 * 1024 * 1024` (100MB max message size) が4箇所にハードコード
- `dfs/chunkserver/src/chunkserver.rs:158` — `512` (checksum chunk size) がハードコード
- `dfs/metaserver/src/simple_raft.rs` — election timeout 1500-3000ms がハードコード

**なぜ負債か**: 同じ値が複数箇所に散在しており、変更時に漏れが生じやすい。
**影響範囲**: 全crate
**変更リスク**: 極低。定数化するだけ。
**検証**: `cargo build --release`, `cargo test --lib`
**判定**: 実装してよい

---

### D11: Background taskの管理不在 (HIGH RISK, LOW CONFIDENCE)

**根拠**:
- `dfs/metaserver/src/master.rs` — MyMaster::new()で7つのtokio::spawnタスクを起動。cancellation tokenなし。
- `dfs/chunkserver/src/bin/chunkserver.rs` — 3つのbackground task。graceful shutdownなし。

**なぜ負債か**: Server shutdownでbackground taskが即座に中断され、中途半端な状態が残りうる。
**影響範囲**: metaserver, chunkserver (運用安定性)
**変更リスク**: 高。tokio_util::CancellationToken等の導入が必要で、全taskのライフサイクル管理に影響。
**判定**: 提案に留める。設計方針の合意が必要。

---

### D12: `master.rs` のbackground task混在 (MEDIUM RISK, HIGH CONFIDENCE)

**根拠**:
- `dfs/metaserver/src/master.rs` — MyMaster::new()内に7つのbackground task(liveness checker, balancer, transaction cleanup, data shuffler, metrics decay, ShardMap refresh, split detection)が直接記述されている。

**なぜ負債か**: コンストラクタにビジネスロジック(バックグラウンド処理)が混在しており、テストが困難。各taskのインターバルや動作条件を個別に変更・テストできない。
**影響範囲**: metaserver crate
**変更リスク**: 中。関数抽出は安全だが、タイミングやMutex取得順序を変えると挙動が変わりうる。
**改善案**: 各background taskを独立した関数(`fn run_liveness_checker(...)`, `fn run_balancer(...)` 等)に抽出し、`MyMaster::new()`はそれらをspawnするだけにする。ロジック変更は行わない。
**検証**: `cargo test --lib -p dfs-metaserver`
**判定**: 実装してよい（関数抽出のみ。ループ内容・タイミングの変更は行わない）

---

## Implementation Phases

### Phase 0: Baseline Recording

1. `git status` でcleanな状態を確認
2. Baseline Commandsを全て実行し、結果を記録
3. 以降の各フェーズ完了後にも同じコマンドを実行し、差分がないことを確認

### Phase 1: 明らかに安全な整理 (D2, D10)

**目的**: ロジックに一切触れず、コードの衛生状態を改善する。

**D2: eprintln!をtracingに置換**
- `dfs/common/src/sharding.rs:70, 78, 277` の `eprintln!("DEBUG: ...")` を `tracing::debug!(...)` に置換
- `dfs/common/src/sharding.rs:335` の `eprintln!("Failed to load...")` を `tracing::warn!(...)` に置換

**D10: Magic number の定数化**
- `dfs/client/src/mod.rs` および `dfs/chunkserver/src/chunkserver.rs` で繰り返し使われる `100 * 1024 * 1024` を定数 (`const MAX_GRPC_MESSAGE_SIZE: usize = 100 * 1024 * 1024;`) として各crateに定義
- `dfs/chunkserver/src/chunkserver.rs:158` の `512` を `const CHECKSUM_CHUNK_SIZE: usize = 512;` に

**検証**: Baseline Commands全て、特に `cargo clippy -- -D warnings`

---

### Phase 2: 重複の解消 (D1)

**目的**: 同一ロジックを共通関数に集約し、保守コストを下げる。

**D1: connect_endpoint の統合**
- `dfs/client/src/mod.rs:87-116` の `Client::connect_endpoint()` を `dfs_common::security::connect_endpoint()` の呼び出しに置換
- `dfs/chunkserver/src/chunkserver.rs:79-113` の `MyChunkServer::connect_endpoint()` も同様に置換
- **注意**: common版の `connect_endpoint()` のシグネチャが十分かを先に確認すること。不足がある場合（例: scheme変換ロジックの差異）は、common版を拡張してから置換する。

**検証**: `cargo test --lib`, `cargo test --lib -p dfs-chunkserver`, `cargo test --lib -p dfs-client`

---

### Phase 3: dead code の検証と整理 (D3)

**目的**: `#[allow(dead_code)]` を検証し、根拠をもって対処する。

- `dfs/s3_server/src/s3_types.rs` の各 `#[allow(dead_code)]`: struct fieldが本当にどこからも参照されていないか `cargo build --release` のwarning出力で確認
- `dfs/s3_server/src/audit.rs` の `#[allow(dead_code)]`: 同上
- 使われていないことが確実なフィールド/関数: `#[allow(dead_code)]` を削除し、フィールド/関数自体も削除
- 将来使う可能性がある（例: S3互換性のために必要）場合: `#[allow(dead_code)]` にコメントで理由を追記（例: `// S3 ListBucketResult V2 response field, reserved for future use`）

**検証**: `cargo build --release`, `cargo clippy -- -D warnings`

---

### Phase 4: S3 handler関数の分割 (D4)

**目的**: 巨大な `handle_request()` を個別のハンドラ関数に分解する。

**手順**:
1. `dfs/s3_server/src/handlers.rs` を読み、現在の分岐構造を把握
2. 既に独立関数になっているもの（`create_bucket`, `list_objects` 等）はそのまま残す
3. `handle_request()` 内にインラインで書かれている各S3オペレーションの処理ブロックを個別関数に抽出
4. `handle_request()` はルーティング (method + path + query params → handler function) のみを行う dispatcher にする
5. **ロジックは一切変更しない**。抽出した関数のシグネチャは引数をそのまま受け取る形でよい

**検証**: `cargo test --lib -p s3-server`, S3統合テスト

---

### Phase 5: auth_middleware の関数分割 (D6)

**目的**: 認証フローの各段階をヘルパー関数に分離し、可読性を上げる。

**手順**:
1. `dfs/s3_server/src/auth_middleware.rs` の `auth_middleware()` を読む
2. 以下の論理ブロックを個別関数に抽出:
   - SigV4 credential 解析 + validation
   - Secret key lookup (通常 vs STS token)
   - 署名検証
   - IAM policy evaluation
   - Audit record 作成
3. `auth_middleware()` はこれらを順次呼び出すオーケストレータにする
4. **認証の分岐ロジックは一切変更しない**

**検証**: `cargo test --lib -p s3-server`, OIDC/STS統合テスト

---

### Phase 6: Master background task の関数抽出 (D12)

**目的**: `MyMaster::new()` コンストラクタからbackground taskのロジックを抽出する。

**手順**:
1. `dfs/metaserver/src/master.rs` の `MyMaster::new()` 内の各 `tokio::spawn` ブロックを確認
2. 各ブロックを以下のような独立関数に抽出:
   - `async fn run_liveness_checker(state: Arc<Mutex<MasterState>>, ...)`
   - `async fn run_block_balancer(state: Arc<Mutex<MasterState>>, ...)`
   - `async fn run_transaction_cleanup(state: Arc<Mutex<MasterState>>, ...)`
   - `async fn run_data_shuffler(state: Arc<Mutex<MasterState>>, ...)`
   - `async fn run_metrics_decay(state: Arc<Mutex<MasterState>>, ...)`
   - `async fn run_shard_map_refresh(state: Arc<Mutex<MasterState>>, ...)`
   - `async fn run_split_detector(state: Arc<Mutex<MasterState>>, ...)`
3. `MyMaster::new()` は各関数を `tokio::spawn` するだけにする
4. **各taskのloop内容、interval、Mutex取得順序は一切変更しない**

**検証**: `cargo test --lib -p dfs-metaserver`, 統合テスト (cross_shard, rename)

---

### Phase 7: Partial read checksum (D9)

**目的**: 部分読み取り時のchecksum検証を実装する。

**手順**:
1. `dfs/chunkserver/src/chunkserver.rs:597` 付近の `read_block()` を確認
2. `.meta` ファイルに格納された512byteチャンク単位のCRC32を活用
3. offset/length から対象チャンクの範囲を計算（`start_chunk = offset / 512`, `end_chunk = (offset + length - 1) / 512`）
4. 対象チャンクのみ `.meta` から読み取り、データとの整合性を検証
5. 不整合時は既存の `recover_block()` フローに乗せる

**検証**: `cargo test --lib -p dfs-chunkserver`, checksum統合テスト

---

### Phase 8 (提案のみ): 以下は人間の承認を得てから実装

| ID | 内容 | 理由 |
|----|------|------|
| D5 | `simple_raft.rs` のモジュール分割 | Raft正確性への影響が大きい。分割案を提示し、承認後に実装 |
| D7 | 文字列ベースのエラーコードの構造化 | proto変更が必要になる可能性。設計提案を先に提出 |
| D8 | Replication failure のエラー伝播改善 | 既存挙動（success応答）の変更。設計判断が必要 |
| D11 | Background task lifecycle管理 | CancellationToken導入等の設計合意が必要 |

---

## Verification Requirements

### 各フェーズ共通

1. **ビルド確認**: `cargo build --release` が成功すること
2. **Clippy**: `cargo clippy -- -D warnings` が成功すること（CIゲート）
3. **Format**: `cargo fmt --all -- --check` が成功すること（CIゲート）
4. **Unit tests**: `cargo test --lib` が全て成功すること
5. **差分確認**: `git diff` で意図しない変更がないことを確認

### Phase 4, 5, 6 追加検証

- 関数分割後に既存の呼び出し元が正しくルーティングされることを手動確認
- 分割前後で `handle_request` / `auth_middleware` / `MyMaster::new` の外部挙動が変わらないこと

### Phase 7 追加検証

- 新規テストの追加: partial read でchecksum不整合を検知できることを確認するユニットテスト

---

## Reporting Format

各フェーズ完了後に以下を報告すること:

```
## Phase N Complete: [タイトル]

### Changes Made
- [変更したファイルと内容の要約]

### Verification Results
- `cargo build --release`: [PASS/FAIL]
- `cargo clippy -- -D warnings`: [PASS/FAIL]
- `cargo fmt --all -- --check`: [PASS/FAIL]
- `cargo test --lib`: [PASS/FAIL] (N tests passed)

### Issues Encountered
- [あれば記載。なければ "None"]

### Next Phase
- [次に着手するフェーズ]
```

---

## Out-of-Scope Items

以下はこのリファクタリングの対象外とする:

1. **新機能の追加**: TODO.mdに記載されている未実装機能 (Object Versioning, SSE, Pre-signed URLs等)
2. **io_uring関連コード**: パフォーマンス改善が見込めないことが判明済み。既にブランチが放棄されている。
3. **テストカバレッジの大幅拡充**: Phase 7の部分読み取りテスト以外の新規テスト追加は対象外
4. **依存ライブラリのバージョンアップ**: 現在のバージョンで動作している限り、アップデートは行わない
5. **Docker Compose / Helm Chart の変更**: インフラ構成は変更しない
6. **ドキュメントの更新**: コード変更に伴うREADME等の更新は不要（CLAUDE.mdの更新も不要）
7. **パフォーマンス最適化**: Mutex → RwLock変更、clone削減等は今回対象外（別途計画）
8. **エラー型の統一**: anyhow → custom error enum 等の大規模変更は対象外
9. **CI設定の変更**: `.github/workflows/ci.yml` は変更しない
