# Rust Hadoop DFS

Rustで実装されたHadoop互換の分散ファイルシステム（DFS）です。
Google File System (GFS) やHadoop HDFSのアーキテクチャを参考に、Raftによる強整合性とシャーディングによるスケーラビリティを実現しています。

## 特徴

- ✅ **分散ストレージ**: 複数のChunkServerにデータを分散保存
- ✅ **レプリケーション**: デフォルトで3つのレプリカを作成（冗長性確保）
- ✅ **高可用性 (HA)**: RaftコンセンサスアルゴリズムによるMasterの冗長化
- ✅ **スケーラビリティ (シャーディング)**: 複数のMasterグループによるメタデータの水平分割、Consistent Hashingによる負荷分散
- ✅ **トランザクション**: クロスシャード操作（Rename等）をAtomicに実行するTransaction Recordの実装
- ✅ **データ整合性**: Raftログレプリケーションによるメタデータの一貫性保証
- ✅ **パイプラインレプリケーション**: 効率的なデータ複製
- ✅ **gRPC通信**: 高性能な通信プロトコル
- ✅ **Docker対応**: Docker Composeで簡単にクラスタ構築（シャーディング構成対応）

## アーキテクチャ

### 全体構成 (Sharding & Raft)

```
┌─────────────────────────────────────────────────────────────┐
│             Config Server (Meta-Shard)                      │
│      [ Config1 ] ◄──► [ Config2 ] ◄──► [Config3 ]           │
│      (ShardMapの管理・設定情報の保持)                       │
└─────────────────────────────────────────────────────────────┘
          │ (ShardMap Fetch)                   │
          ▼                                    ▼
┌───────────────────────┐            ┌───────────────────────┐
│     Shard-1 (A-M)     │            │     Shard-2 (N-Z)     │
│ ┌───────────────────┐ │            │ ┌───────────────────┐ │
│ │ Master1 (Leader)  │ │            │ │ Master1 (Leader)  │ │
│ │ Master2 (Follower)│ │            │ │ Master2 (Follower)│ │
│ │ Master3 (Follower)│ │            │ │ Master3 (Follower)│ │
│ └───────────────────┘ │            │ └───────────────────┘ │
└───────────────────────┘            └───────────────────────┘
          │                                    │
          └──────────────────┬─────────────────┘
                             ▼
              ┌─────────────────────────────┐
              │        ChunkServers         │
              │ [CS1] [CS2] [CS3] [CS4] ... │
              └─────────────────────────────┘
```

## クイックスタート

### Docker Compose (推奨: シャーディング構成)

シャーディング対応のクラスタを起動します。

```bash
# シャーディング環境（2シャード構成 + 複数のChunkServer）の起動
docker compose -f docker-compose-sharded.yml up -d --build

# ファイルアップロードの例 (Shard 1への書き込み)
docker exec dfs-master1-shard1 /app/dfs_cli --master http://localhost:50051 put /file.txt /uploaded.txt

# ファイル操作 (Renameなど)
docker exec dfs-master1-shard1 /app/dfs_cli --master http://localhost:50051 rename /uploaded.txt /renamed.txt
```

### ローカル開発 (シングルマスター構成)

従来のシングルマスター/HA構成も利用可能です。

```bash
# ビルド
cargo build --release

# クラスタ起動スクリプト (Master × 3 + ChunkServer × 5)
./start_cluster.sh
```

## テスト

様々なシナリオに対応したテストスクリプトが用意されています。

### 1. ユニットテスト

```bash
cargo test
```

### 2. インテグレーション & カオステスト

| テストスクリプト            | 説明                                                       |
| --------------------------- | ---------------------------------------------------------- |
| `rename_test.sh`            | 基本的なファイルリネーム機能（同一シャード内）のテスト     |
| `same_shard_rename_test.sh` | シャード環境下での同一シャード内リネームのテスト           |
| `cross_shard_test.sh`       | クロスシャードリネーム（Transaction Record）の正常系テスト |
| `transaction_abort_test.sh` | クロスシャード操作失敗時のロールバック（Abort）テスト      |
| `fault_recovery_test.sh`    | トランザクション中のシャード障害からの復旧テスト           |
| `chaos_test.sh`             | ChunkServerの障害などをシミュレートしたカオステスト        |

## プロジェクト構成

```
rust-hadoop/
├── proto/
│   └── dfs.proto              # gRPC定義
├── src/
│   ├── bin/
│   │   ├── master.rs          # Masterサーバー (Raft + Sharding統合)
│   │   ├── config_server.rs   # Configサーバー (Meta-Shard)
│   │   ├── chunkserver.rs     # ChunkServerサーバー
│   │   └── dfs_cli.rs         # CLIクライアント
│   ├── master.rs              # Master実装 (Transactionロジック含む)
│   ├── sharding.rs            # Shardingロジック (Consistent Hashing)
│   ├── simple_raft.rs         # Raftコンセンサス実装
│   └── chunkserver.rs         # ChunkServer実装
├── docker-compose-sharded.yml # シャーディング構成用Docker Compose
├── docker-compose.yml         # 従来構成用Docker Compose
└── *.sh                       # 各種テスト・起動スクリプト
```

## API

### Master Core RPC

- `CreateFile`: ファイル作成
- `GetFileInfo`: ファイル情報取得
- `AllocateBlock`: ブロック割り当て
- `CompleteFile`: ファイル書き込み完了
- `ListFiles`: ファイル一覧
- `Rename`: ファイル移動・名前変更（クロスシャード対応）

### Master Internal RPC (Transaction & Raft)

- `PrepareTransaction`: トランザクション準備（2PC）
- `CommitTransaction`: トランザクションコミット
- `AbortTransaction`: トランザクションロールバック
- `Heartbeat`: ChunkServer生存確認

### ChunkServer RPC

- `WriteBlock`: ブロック書き込み（パイプラインレプリケーション）
- `ReadBlock`: ブロック読み込み
- `ReplicateBlock`: レプリケーション受信

## 技術スタック

- **言語**: Rust
- **通信**: gRPC (tonic)
- **非同期**: Tokio
- **合意形成**: 自作Raft実装 + Transaction Record (2PC)
- **シャーディング**: Consistent Hashing (Virtual Nodes)
- **コンテナ**: Docker

## ライセンス

MIT

## 参考資料

- [MASTER_HA.md](MASTER_HA.md) - Master HAとシャーディングの詳細
- [REPLICATION.md](REPLICATION.md) - レプリケーション機能の詳細
- [CHAOS_TEST.md](CHAOS_TEST.md) - カオステストガイド
- [Google File System](https://research.google.com/archive/gfs.html)
- [Spanner: Google's Globally-Distributed Database](https://research.google.com/archive/spanner.html)
