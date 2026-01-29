# Rust Hadoop DFS

Rustで実装されたHadoop互換の分散ファイルシステム（DFS）です。
Google File System (GFS) やHadoop HDFSのアーキテクチャを参考に、Raftによる強整合性とシャーディングによるスケーラビリティを実現しています。

## 特徴

- ✅ **分散ストレージ**: 複数のChunkServerにデータを分散保存
- ✅ **レプリケーション**: デフォルトで3つのレプリカを作成（冗長性確保）
- ✅ **読み取り最適化**: 部分読み取り、並行フェッチ、LRUキャッシュ、S3レンジリクエスト最適化（HTTP 206 Partial Content）
- ✅ **高可用性 (HA)**: RaftコンセンサスアルゴリズムによるMasterの冗長化
- ✅ **Dynamic Membership Changes**: Raftクラスタの稼働中ノード追加・削除（Joint Consensus、Leader Transfer、Catch-up Protocol）
- ✅ **ダイナミックシャーディング**: Range-basedシャーディングによる複数のMasterグループでのメタデータ水平分割、負荷に応じた自動シャード分割（Split/Merge）、S3/Colossusスタイルのプレフィックス局所性
- ✅ **トランザクション**: クロスシャード操作（Rename等）をAtomicに実行するTransaction Recordの実装
- ✅ **データ整合性**: Raftログレプリケーションによるメタデータの一貫性保証
- ✅ **パイプラインレプリケーション**: 効率的なデータ複製
- ✅ **gRPC通信**: 高性能な通信プロトコル
- ✅ **S3互換API**: AWS CLI や Apache Spark からのアクセス、マルチパートアップロード対応
- ✅ **Docker対応**: Docker Composeで簡単にクラスタ構築（シャーディング・S3構成対応）

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
│ Shard-1 (""-"/m")     │            │ Shard-2 ("/m"-"")     │
│ ┌───────────────────┐ │            │ ┌───────────────────┐ │
│ │ Master1 (Leader)  │ │            │ │ Master1 (Leader)  │ │
│ │ Master2 (Follower)│ │            │ │ Master2 (Follower)│ │
│ │ Master3 (Follower)│ │            │ │ Master3 (Follower)│ │
│ └───────────────────┘ │            │ └───────────────────┘ │
│ (Range-based, 負荷に │            │ (高負荷時は自動分割) │
│  応じて自動Split)      │            │                       │
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
docker compose -f docker-compose.yml up -d --build

# ファイルアップロードの例 (Shard 1への書き込み)
docker exec dfs-master1-shard1 /app/dfs_cli --master http://localhost:50051 put /file.txt /uploaded.txt

# ファイル操作 (Renameなど)
docker exec dfs-master1-shard1 /app/dfs_cli --master http://localhost:50051 rename /uploaded.txt /renamed.txt

# AWS CLIからの操作 (S3互換API経由)
aws --endpoint-url http://localhost:9000 s3 ls s3://spark-test-bucket/
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
# 全ての単体テストを実行（結合テストはスキップ）
cargo test

# 結合テスト（Client Integration Test）のみを実行
# ※事前にDockerクラスタが起動している必要があります
cargo test -- --ignored

# 全てのテスト（単体 + 結合）を実行
cargo test -- --include-ignored
```

### 2. インテグレーション & カオステスト

| テストスクリプト            | 説明                                                       |
| --------------------------- | ---------------------------------------------------------- |
| `rename_test.sh`            | 基本的なファイルリネーム機能（同一シャード内）のテスト     |
| `same_shard_rename_test.sh` | シャード環境下での同一シャード内リネームのテスト           |
| `cross_shard_test.sh`       | クロスシャードリネーム（Transaction Record）の正常系テスト |
| `transaction_abort_test.sh` | クロスシャード操作失敗時のロールバック（Abort）テスト      |
| `fault_recovery_test.sh`    | トランザクション中のシャード障害からの復旧テスト           |
| `auto_scaling_test.sh`      | 負荷に応じたシャード自動分割（Dynamic Sharding）のテスト   |
| `chaos_test.sh`             | ChunkServerの障害などをシミュレートしたカオステスト        |
| `run_spark_test.sh`         | S3互換API経由でのSpark (CSV/Parquet) 統合テスト            |


## プロジェクト構成

このプロジェクトはCargo Workspace機能を使用して複数のクレートを管理しています。

```
rust-hadoop/
├── dfs/                       # DFS主要コンポーネント
│   ├── metaserver/            # メタデータ管理サーバー
│   │   ├── src/bin/
│   │   │   ├── master.rs      # Masterサーバー (Raft + Sharding統合)
│   │   │   └── config_server.rs # Configサーバー (Meta-Shard)
│   │   └── src/               # Raft, Shardingロジックの実装
│   ├── chunkserver/           # データ格納サーバー
│   │   ├── src/bin/
│   │   │   └── chunkserver.rs # ChunkServerバイナリ
│   │   └── src/               # データ保存・レプリケーションロジック
│   └── client/                # クライアントライブラリ & CLI
│       ├── src/bin/
│       │   └── dfs_cli.rs     # CLIツール
│       └── src/               # Clientライブラリ実装 (接続管理, ShardMapキャッシュ)
├── s3_server/                 # S3互換APIゲートウェイ (Axumベース)
├── proto/
│   └── dfs.proto              # gRPC定義
├── docker-compose.yml         # シャーディング構成用Docker Compose
└── *.sh                       # 各種テスト・起動スクリプト
```

## API

### Master Core RPC (gRPC)

- `CreateFile`: ファイル作成
- `GetFileInfo`: ファイル情報取得
- `AllocateBlock`: ブロック割り当て
- `CompleteFile`: ファイル書き込み完了
- `ListFiles`: ファイル一覧
- `Rename`: ファイル移動・名前変更（クロスシャード対応）
- `InitiateShuffle`: シャード分割時のデータ移行開始

### Master Mgmt & Monitoring (HTTP)

- **GET** `/health`: Liveness Probe
- **GET** `/metrics`: Prometheus メトリクス (Raft状態など)
- **GET** `/raft/state`: Raftの内部状態（JSON）

### Master Internal RPC (Transaction & Raft)

- `PrepareTransaction`: トランザクション準備（2PC）
- `CommitTransaction`: トランザクションコミット
- `AbortTransaction`: トランザクションロールバック
- `Heartbeat`: ChunkServer生存確認

### ChunkServer RPC

- `WriteBlock`: ブロック書き込み（パイプラインレプリケーション対応）
- `ReadBlock`: ブロック読み取り（部分読み取り対応: offset/length パラメータ）
- `ReplicateBlock`: レプリケーション受信
- `UpdateBlockSize`: ブロックサイズ更新（ファイル完了時）

### S3 API (REST)

- `PutObject`, `GetObject`, `DeleteObject`, `HeadObject`, `CopyObject`
- `CreateMultipartUpload`, `UploadPart`, `CompleteMultipartUpload`
- `CreateBucket`, `DeleteBucket`, `ListBuckets`
- `DeleteObjects` (Multi-Object Delete)

## 技術スタック

- **言語**: Rust
- **通信**: gRPC (tonic)
- **非同期**: Tokio
- **合意形成**: 自作Raft実装 + Transaction Record (2PC)
- **シャーディング**: Range-based Dynamic Sharding（負荷監視による自動Split/Merge）
- **オブザーバビリティ**: 構造化ログ（tracing）、分散トレーシング（Request ID伝播）
- **コンテナ**: Docker

## ライセンス

MIT

## 参考資料

### プロジェクトドキュメント

- [MASTER_HA.md](MASTER_HA.md) - Master HAとシャーディングの詳細
- [DYNAMIC_SHARDING.md](DYNAMIC_SHARDING.md) - ダイナミックシャーディングの実装詳細
- [REPLICATION.md](REPLICATION.md) - レプリケーション機能の詳細
- [S3_COMPATIBILITY.md](S3_COMPATIBILITY.md) - S3互換APIとSpark統合の詳細
- [CHAOS_TEST.md](CHAOS_TEST.md) - カオステストガイド
- [test_scripts/DYNAMIC_MEMBERSHIP_TESTS.md](test_scripts/DYNAMIC_MEMBERSHIP_TESTS.md) - Dynamic Membership Changesのテストガイド

### 外部リソース

- [Google File System](https://research.google.com/archive/gfs.html)
- [Spanner: Google's Globally-Distributed Database](https://research.google.com/archive/spanner.html)
- [Google Colossus](https://cloud.google.com/blog/products/storage-data-transfer/a-peek-behind-colossus-googles-file-system)
