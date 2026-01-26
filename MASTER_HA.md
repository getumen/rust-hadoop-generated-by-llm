# Master High Availability (HA) ガイド

このドキュメントでは、Rust Hadoop DFSのMasterサーバーの高可用性（HA）構成について説明します。
現在の実装は**Raftコンセンサスアルゴリズム**に基づいており、**シャーディング**と**Configuration Group**による分散アーキテクチャを採用しています。

## 概要

Master HAは、以下の2層構造で実現されています：

### 1. Configuration Group (Meta-Shard)
- **役割**: ShardMapの管理
- **構成**: 3ノード以上のRaftクラスタ
- **状態**: ShardMap（シャードIDとピアのマッピング）
- **RPC**: FetchShardMap, AddShard, RemoveShard

### 2. Data Shards (Shard-0, Shard-1, ...)
- **役割**: ファイルメタデータの管理（シャードごとに分割）
- **構成**: 各シャードが3ノード以上のRaftクラスタ
- **状態**: MasterState（files, chunk_servers, pending_commands, transaction_records）
- **RPC**: CreateFile, AllocateBlock, GetFileInfo, Rename, PrepareTransaction, CommitTransaction, AbortTransaction, etc.

## アーキテクチャ

```
┌───────────────────────────────────────────────────────────────┐
│              Config Server (Meta-Shard)                       │
│  ┌────────────┐    ┌────────────┐    ┌────────────┐           │
│  │ ConfigNode │◄──►│ ConfigNode │◄──►│ ConfigNode │           │
│  └────────────┘    └────────────┘    └────────────┘           │
│  ShardMap: { shard-1: [addrs...], shard-2: [addrs...] }       │
└───────────────────────────────────────────────────────────────┘
          │                              │
          │ ShardMap Fetch               │
          ▼                              ▼
┌─────────────────────────┐      ┌─────────────────────────┐
│ Shard-1 (Range:""-"/m") │      │ Shard-2 (Range:"/m"-"") │
│  ┌───────────────────┐  │      │  ┌───────────────────┐  │
│  │ Master-1 (Leader) │  │      │  │ Master-1 (Leader) │  │
│  └┬─────────▲───────▲┘  │      │  └┬─────────▲───────▲┘  │
│   │         │       │   │      │   │         │       │   │
│ ┌─▼──────┐┌─▼──────┐│   │      │ ┌─▼──────┐┌─▼──────┐│   │
│ │Master-2││Master-3││   │      │ │Master-2││Master-3││   │
│ └────────┘└────────┘│   │      │ └────────┘└────────┘│   │
│ (高負荷時は自動Split) │      │ (Throughput監視中)  │
└─────────────────────────┘      └─────────────────────────┘
          │                              │
          │ Metadata for paths < "/m"    │ Metadata for paths >= "/m"
          │                              │
     ┌────┴────┬─────────┬─────────┬─────┴────┐
     │         │         │         │          │
┌────▼───┐ ┌───▼────┐ ┌──▼─────┐ ┌─▼──────┐ ┌─▼──────┐
│  CS 1  │ │  CS 2  │ │  CS 3  │ │  CS 4  │ │  CS 5  │
└────────┘ └────────┘ └────────┘ └────────┘ └────────┘
```

## 実装の詳細

### 1. ダイナミックシャーディング (Dynamic Sharding)

本システムは**Range-based Dynamic Sharding**を採用しており、S3やGoogle Colossusと同様のアプローチを取っています。

#### Range-based Partitioning

- **辞書順範囲分割**: ファイルパスの辞書順範囲に基づいてShardを割り当てます
  - 例: Shard-1は`""`（空文字列）から`"/m"`まで、Shard-2は`"/m"`から`""`（無限大）まで
- **プレフィックス局所性**: 同じディレクトリやプレフィックスのファイルは同じShardに配置されるため、`ListFiles`などのディレクトリ操作が効率的
- **決定的ルーティング**: ハッシュと異なり、パスから直接どのShardか判定可能（クライアント側でキャッシュ可能）

#### 負荷監視とスループット計測

各Masterは`ThroughputMonitor`を使用してリアルタイムでスループットを計測します：

```rust
pub struct ThroughputMonitor {
    read_rps: AtomicU64,   // 読み取りリクエスト/秒
    write_rps: AtomicU64,  // 書き込みリクエスト/秒
    read_bps: AtomicU64,   // 読み取りバイト/秒
    write_bps: AtomicU64,  // 書き込みバイト/秒
}
```

- リクエストごとに`record_request(path, size)`を呼び出し、スライディングウィンドウで集計
- 定期的に閾値（`split_threshold_rps`）と比較し、超過時はSplitを検討

#### 自動シャード分割（Split）のフロー

1. **負荷検出**: LeaderがThroughputMonitorで閾値超過を検出
2. **分割点決定**: 現在のファイル一覧の中央値パスを分割点として選択
3. **Raft合意**: `SplitShard`コマンドをRaftログに書き込み、全ノードで合意
   ```rust
   Command::SplitShard {
       new_shard_id: String,
       split_key: String,
       new_shard_leader: String,
   }
   ```
4. **Config Server更新**: `AddShard` RPCで新Shardの情報（ID、Range、Leader）を登録
5. **データ移行（Shuffle）**:
   - 旧Shardから新Shardへ`InitiateShuffle` RPCを送信
   - 新Shardの範囲に該当するファイルメタデータを転送
   - 旧Shardから該当ファイルを削除
6. **完了**: クライアントは次回のShardMap更新で新しい構成を認識

#### Redirect機能

- **誤ったShard判定**: クライアントが古いShardMapで誤ったShardにアクセス
- **REDIRECT応答**: Masterは`OutOfRange`エラーと共に正しいShardのLeaderアドレスを返す
- **ShardMap更新**: クライアントはConfig Serverから最新のShardMapを取得し、リトライ

### 2. クロスシャード操作 (Cross-Shard Operations) - Transaction Record方式

Google Spannerスタイルに近い、**Transaction Record**を使用した2フェーズコミット（2PC）を実装しています。

#### Transaction Recordの構造

各ShardはMasterState内に`transaction_records`を持ち、Raftログを通じて永続化します。

```rust
pub struct TransactionRecord {
    tx_id: String,              // UUID
    tx_type: TransactionType,   // Rename { source, dest }
    state: TxState,             // Pending, Prepared, Committed, Aborted
    timestamp: u64,             // 開始タイムスタンプ
    participants: Vec<String>,  // 参加Shard ID
    operations: Vec<TxOperation>, // 各Shardでの操作
}

pub enum TxState {
    Pending,    // トランザクション開始
    Prepared,   // 準備完了（検証OK）
    Committed,  // コミット完了
    Aborted,    // 中止
}
```

#### クロスシャードRenameのフロー

1. **Transaction開始 (Phase 1)**
   - Source Shardが`transaction_id`を発行し、`Pending`状態でレコードを作成。
   - `CreateTransactionRecord`コマンドをRaftで合意。

2. **Prepare (Phase 1)**
   - Source ShardがDest Shardへ`PrepareTransaction` RPCを送信。
   - Dest Shardは整合性をチェック（例: 移動先ファイルが存在しないか）。
   - OKならDest Shardで`Prepared`レコードを作成し、成功を返す。

3. **Commit (Phase 2)**
   - Source ShardはDest Shardからの`Prepared`を受け取ると、自身の状態を`Committed`に更新（Raft合意）。
   - Source Shard上のソースファイルを削除。
   - Source ShardがDest Shardへ`CommitTransaction` RPCを送信。
   - Dest Shardは状態を`Committed`に更新し、デスティネーションファイルを作成。

4. **完了**
   - Source Shardがクライアントへ成功を返す。

#### 障害復旧 (Fault Recovery)

- **タイムアウト**: 各フェーズで応答がない場合、TransactionはタイムアウトとなりAbortされます。
- **リカバリ**: Shardがクラッシュしても、RaftログからTransaction Recordが復元されるため、再起動後に状態（Pending/Prepared/Committed）を確認し、整合性を保つことができます。
- **GC**: 完了したTransaction Recordはバックグラウンドタスクにより一定時間後に削除されます。

### 3. リーダー選出 & ログレプリケーション

Raftアルゴリズムに基づき、各ShardおよびConfig Serverで独立してリーダー選出とログ複製が行われます。

- **Leader**: クライアントからのリクエストを処理し、ログをFollowerへ複製。
- **Follower**: Leaderからのログを受信し、コミットされたら状態マシンへ適用。
- **Candidate**: Leader不在時に立候補し選挙を行う。

### 4. 永続化 (Persistence)

RocksDBを使用してRaftログとMaster状態を永続化しています。

- `log`: Raftのエントリ（操作ログ）
- `voted_for`, `current_term`: 選挙情報
- `snapshot`: 定期的に作成される状態スナップショット

## Docker Compose構成

`docker-compose.yml`の例：

```yaml
services:
  # Config Server (Meta-Shard)
  config-server:
    build: .
    command: /app/config_server --addr 0.0.0.0:50052 ...

  # Shard 1 (2 Master Nodes for demo)
  master1-shard1:
    build: .
    environment:
      - SHARD_ID=shard-1
    command: /app/master --addr 0.0.0.0:50051 --shard-id shard-1 ...

  master2-shard1:
    # ...

  # Shard 2
  master1-shard2:
    environment:
      - SHARD_ID=shard-2
    command: /app/master --addr 0.0.0.0:50051 --shard-id shard-2 ...

  # ChunkServers (Shared)
  chunkserver1-shard1:
     # ...
```

## テスト

以下のスクリプトでHA機能とシャーディング機能をテストできます。

```bash
# シャーディング & クロスシャードリネーム
./cross_shard_test.sh

# トランザクションアボート
./transaction_abort_test.sh

# 障害復旧
./fault_recovery_test.sh

# ダイナミックシャーディング（自動分割）
./auto_scaling_test.sh
```

## 今後の改善予定

- [x] **Range-basedシャーディング**: 複数のMaster Shardによる水平スケーリング（辞書順範囲分割）
- [x] **Configuration Group**: ShardMapの集中管理
- [x] **クロスシャード操作**: Transaction RecordによるAtomic操作
- [x] **ダイナミックシャード分割・統合**: 負荷に応じた自動Split/Merge、データ移行（Shuffle）
- [x] **クライアント側ShardMapキャッシング**: リダイレクトの削減
- [x] **ReadIndex**: Followerからの読み取り（負荷分散）
- [ ] **動的なメンバーシップ変更**: 稼働中のクラスタへのノード追加・削除
