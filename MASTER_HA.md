# Master High Availability (HA) ガイド

このドキュメントでは、Rust Hadoop DFSのMasterサーバーの高可用性（HA）構成について説明します。
現在の実装は**Raftコンセンサスアルゴリズム**に基づいており、**シャーディング**と**Configuration Group**による分散アーキテクチャを採用しています。

## 概要

Master HAは、以下の2層構造で実現されています：

### 1. Configuration Group (Meta-Shard)
- **役割**: ShardMapの管理、クロスシャード操作の調整
- **構成**: 3ノード以上のRaftクラスタ
- **状態**: ShardMap（シャードIDとピアのマッピング）
- **RPC**: FetchShardMap, AddShard, RemoveShard, CrossShardRename

### 2. Data Shards (Shard-0, Shard-1, ...)
- **役割**: ファイルメタデータの管理（シャードごとに分割）
- **構成**: 各シャードが3ノード以上のRaftクラスタ
- **状態**: MasterState（files, chunk_servers, pending_commands）
- **RPC**: CreateFile, AllocateBlock, GetFileInfo, Rename, etc.

## アーキテクチャ

```
┌─────────────────────────────────────────────────┐
│         Configuration Group (Meta-Shard)        │
│  ┌──────────┐    ┌──────────┐    ┌──────────┐   │
│  │ Config 0 │◄──►│ Config 1 │◄──►│ Config 2 │   │
│  │ (Leader) │    │(Follower)│    │(Follower)│   │
│  └──────────┘    └──────────┘    └──────────┘   │
│  ShardMap: { shard-0: [...], shard-1: [...] }  │
└─────────────────────────────────────────────────┘
         │                              │
         │ Manages ShardMap             │ Coordinates
         │                              │ Cross-Shard Ops
         ▼                              ▼
┌─────────────────────┐      ┌─────────────────────┐
│   Shard-0 (a-m)     │      │   Shard-1 (n-z)     │
│  ┌────────────────┐ │      │  ┌────────────────┐ │
│  │ Master-0-0     │ │      │  │ Master-1-0     │ │
│  │ (Leader)       │ │      │  │ (Leader)       │ │
│  ├────────────────┤ │      │  ├────────────────┤ │
│  │ Master-0-1     │ │      │  │ Master-1-1     │ │
│  │ (Follower)     │ │      │  │ (Follower)     │ │
│  ├────────────────┤ │      │  ├────────────────┤ │
│  │ Master-0-2     │ │      │  │ Master-1-2     │ │
│  │ (Follower)     │ │      │  │ (Follower)     │ │
│  └────────────────┘ │      │  └────────────────┘ │
└─────────────────────┘      └─────────────────────┘
         │                              │
         │ Metadata for files a-m       │ Metadata for files n-z
         │                              │
    ┌────┴────┬────────┬────────┬───────┴────┐
    │         │        │        │            │
┌───▼──┐ ┌───▼──┐ ┌───▼──┐ ┌───▼──┐ ┌───▼──┐
│ CS 1 │ │ CS 2 │ │ CS 3 │ │ CS 4 │ │ CS 5 │
│(Data)│ │(Data)│ │(Data)│ │(Data)│ │(Data)│
└──────┘ └──────┘ └──────┘ └──────┘ └──────┘
```

## 実装の詳細

### 1. リーダー選出 (Leader Election)

各Raftグループ（Config Group、各Data Shard）で独立してリーダー選出が行われます：

- 各ノードは起動時、**Follower**として開始します。
- Leaderからのハートビートが一定時間（選挙タイムアウト：150-300msのランダム）途絶えると、**Candidate**になり選挙を開始します。
- 過半数の票を集めたCandidateが新しい**Leader**になります。
- Leaderは定期的にハートビート（AppendEntries RPC）を送信し、権威を維持します。

### 2. ログレプリケーション (Log Replication)

#### Data Shardの場合（ファイル操作）

1. クライアントはShardのLeaderにリクエスト（例: `CreateFile`）を送信します。
2. Leaderはリクエストを自身のログに追加し、Followerに並列で送信します。
3. 過半数のFollowerがログを保存したことを確認すると、Leaderはログを**コミット**し、状態マシン（MasterState）に適用します。
4. Leaderはクライアントに応答を返します。
5. FollowerはLeaderからの次のハートビートでコミットインデックスの更新を知り、自身の状態マシンにログを適用します。

#### Config Groupの場合（ShardMap管理）

1. クライアント（または管理者）がConfig GroupのLeaderに`AddShard`/`RemoveShard`リクエストを送信します。
2. Config GroupのRaftログに`ConfigCommand`が記録され、過半数で合意されます。
3. ShardMapが更新され、全ノードで同期されます。

### 3. シャーディング (Sharding)

- **Consistent Hashing**: ファイルパスのハッシュ値に基づいて、どのShardが担当するかを決定します。
- **Virtual Nodes**: 各Shardに複数の仮想ノードを割り当て、負荷を均等に分散します。
- **ShardMap**: Config Groupが権威を持つShardMapを管理し、クライアントはこれを参照してリクエスト先を決定します。

### 4. クロスシャード操作 (Cross-Shard Operations) - Transaction Record方式

**設計思想**: Google Spannerスタイルの**Transaction Record**を使用し、各Shardが独立してトランザクション状態を管理します。Meta-Shardは調整役ではなく、Transaction IDの発行とShardMap管理のみを担当します。

#### Transaction Recordの構造

各Shardは、クロスシャード操作のためのTransaction Recordを自身のRaftログに記録します：

```rust
pub struct TransactionRecord {
    tx_id: String,              // UUID（Meta-Shardが発行）
    tx_type: TransactionType,   // Rename, Move, etc.
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

#### クロスシャードRenameのフロー（例: /file-a.txt → /file-z.txt）

**Phase 1: Transaction開始**

1. クライアント → Source Shard (shard-0): `Rename(/file-a.txt, /file-z.txt)`
2. Source Shard:
   - クロスシャードを検出（ハッシュ計算）
   - Transaction ID生成（UUID）
   - 自身のRaftログに`TransactionRecord`を記録（state=Pending）
   ```
   TxRecord {
       tx_id: "tx-12345",
       state: Pending,
       participants: ["shard-0", "shard-1"],
       operations: [
           { shard_id: "shard-0", op: Delete("/file-a.txt") },
           { shard_id: "shard-1", op: Create("/file-z.txt", metadata) }
       ]
   }
   ```

**Phase 2: Prepare（検証フェーズ）**

3. Source Shard → Dest Shard (shard-1): `PrepareTransaction(tx_id, operation)`
4. Dest Shard:
   - `/file-z.txt`が既に存在しないか確認
   - 自身のRaftログに`TransactionRecord`を記録（state=Prepared）
   - Source Shardに`Prepared`を返す

5. Source Shard:
   - `/file-a.txt`が存在するか確認
   - 自身のRaftログを更新（state=Prepared）

**Phase 3: Commit（実行フェーズ）**

6. Source Shard:
   - 両方のShardが`Prepared`なら、Commitを決定
   - 自身のRaftログを更新（state=Committed）
   - `/file-a.txt`を削除

7. Source Shard → Dest Shard: `CommitTransaction(tx_id)`
8. Dest Shard:
   - 自身のRaftログを更新（state=Committed）
   - `/file-z.txt`を作成

9. Source Shard → クライアント: Success

#### 障害時の処理

**Dest Shardがダウン（Prepare前）:**
- Source Shardはタイムアウト（10秒）でAbort
- Transaction Recordを`Aborted`に更新
- クライアントにエラーを返す

**Source Shardがダウン（Commit前）:**
- Dest Shardは`Prepared`状態で待機
- Source Shardが復旧後、Transaction Recordを読み取り、Commitを再試行
- タイムアウト（10秒）後、Dest Shardは自動的にAbort

**ネットワーク分断:**
- タイムスタンプベースのタイムアウト
- タイムアウト後、両Shardが独立してAbort判断

#### Transaction Recordの読み取り

ファイル読み取り時、Transaction Recordをチェックして最新状態を反映：

```rust
fn get_file(path: &str) -> Option<FileMetadata> {
    // 1. 通常のファイルメタデータを取得
    if let Some(metadata) = files.get(path) {
        // 2. このファイルを削除するCommittedトランザクションがあるか？
        for tx in transaction_records.values() {
            if tx.state == Committed && tx.deletes_file(path) {
                return None; // 削除済み
            }
        }
        return Some(metadata);
    }
    
    // 3. このファイルを作成するCommittedトランザクションがあるか？
    for tx in transaction_records.values() {
        if tx.state == Committed && tx.creates_file(path) {
            return Some(tx.get_metadata(path));
        }
    }
    
    None
}
```

#### ガベージコレクション

古いTransaction Recordを定期的に削除：

- Committed/Abortedから1時間経過したレコードを削除
- Pending/Preparedは保持（復旧のため）
- バックグラウンドタスクで定期実行

#### 利点

- **Meta-Shardがボトルネックにならない**: 各Shardが独立して処理
- **強い整合性**: Raftログによる保証
- **障害復旧**: Transaction Recordから状態を復元可能
- **スケーラビリティ**: Shard数に応じてスケール

### 5. 永続化 (Persistence)

RocksDBを使用して以下のデータを永続化しています：

**各Raftグループ（Config Group、各Data Shard）ごとに**:
- `current_term`: 現在のTerm番号
- `voted_for`: 現在のTermで投票した候補者ID
- `log`: ログエントリ（Term, Command）
- `snapshot`: アプリケーション状態のスナップショット

これにより、ノードがクラッシュして再起動しても、以前の状態から再開できます。

### 6. スナップショット (Snapshot)

ログが無限に増えるのを防ぐため、定期的にスナップショットを作成します。

- ログサイズが一定（例: 100エントリ）を超えると、現在のアプリケーション状態（MasterStateまたはShardMap）をシリアライズして保存します。
- スナップショットに含まれる古いログエントリは削除（Truncate）されます。
- 遅れているFollowerには、ログの代わりにスナップショットを転送（InstallSnapshot RPC）して同期させます。

### 7. クライアントの動作

#### シャード解決
1. クライアントはConfig Groupから`ShardMap`を取得（キャッシュ可能）
2. ファイルパスのハッシュ値から担当Shardを決定
3. 該当ShardのMasterに接続

#### リーダー発見
- 接続したMasterがLeaderでない場合、「Not Leader」エラーと共に**Leader Hint**を返します。
- クライアントはHintを元にLeaderに再接続します。

#### リダイレクト処理
- 間違ったShardに接続した場合、「REDIRECT」エラーと共に正しいShardのアドレスを返します。
- クライアントは正しいShardに再接続します。

## Docker Compose構成

`docker-compose-sharded.yml`の例：

```yaml
services:
  # Config Server
  config-server:
    build: .
    container_name: config-server
    command: /app/config_server --addr 0.0.0.0:50052 --id 0 --peers "" --http-port 8081 --storage-dir /data/config-raft
    volumes:
      - config-server-data:/data/config-raft
    ports:
      - "50050:50052"
      - "8080:8081"

  # Shard 0
  master-0-0:
    build: .
    container_name: master-0-0
    command: /app/master --addr 0.0.0.0:50051 --id 0 --shard-id shard-0 --peers http://master-0-1:8080,http://master-0-2:8080 --http-port 8080 --storage-dir /data/raft --shard-config /app/shard_config.json
    volumes:
      - master-0-0-data:/data/raft
      - ./shard_config.json:/app/shard_config.json
    ports:
      - "50051:50051"

  # ... (master-0-1, master-0-2, master-1-0, master-1-1, master-1-2)

  # ChunkServers (shared across shards)
  chunkserver1:
    build: .
    container_name: chunkserver1
    command: /app/chunkserver --addr 0.0.0.0:50052 --master-addr master-0-0:50051,master-0-1:50051,master-0-2:50051,master-1-0:50051,master-1-1:50051,master-1-2:50051
    volumes:
      - cs1-data:/data/cs1
```

## 運用・トラブルシューティング

### クラスタの状態確認

各Raftグループのログを確認：

```bash
# Config Group
docker compose -f docker-compose-sharded.yml logs -f config-server

# Shard 0
docker compose -f docker-compose-sharded.yml logs -f master-0-0

# Shard 1
docker compose -f docker-compose-sharded.yml logs -f master-1-0
```

`Node X becoming LEADER` や `Node X starting election` などのログで状態を確認できます。

### ノードの復旧

ノードがダウンしても、データは永続化ボリュームに残っています。
コンテナを再起動すれば、自動的にクラスタに復帰し、遅れているログやスナップショットを取得して同期します。

```bash
docker compose -f docker-compose-sharded.yml start master-0-0
```

### 全ノード停止からの復旧

各Raftグループで過半数のノードが起動すれば、サービスは再開します：
- Config Group: 3ノード構成なら2ノード必要
- 各Data Shard: 3ノード構成なら2ノード必要

### データの整合性

- **メタデータ**: 各Raftグループにより強整合性が保証されます。
- **ShardMap**: Config GroupのRaftにより強整合性が保証されます。
- **ファイルデータ**: ChunkServerへの書き込みはパイプラインレプリケーションで行われます。

### カオステスト

シャーディング環境でのフォールトトレランスをテスト：

```bash
./chaos_test_sharded.sh
```

このテストは以下を検証します：
- ChunkServer障害時のデータアクセス
- レプリケーションの正常性
- データ整合性（MD5チェックサム）

## 今後の改善予定

- [x] **シャーディング**: 複数のMaster Shardによる水平スケーリング
- [x] **Configuration Group**: ShardMapの集中管理
- [/] **クロスシャード操作**: Raft-based coordination（プロトコル定義済み、実装中）
- [ ] **動的なメンバーシップ変更**: 稼働中のクラスタへのノード追加・削除
- [ ] **ReadIndex**: Followerからの読み取り（負荷分散）
- [ ] **Pre-Vote**: 無駄な選挙の抑制
- [ ] **クライアント側ShardMapキャッシング**: リダイレクトの削減
