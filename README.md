# Rust Hadoop DFS

Rustで実装されたHadoop互換の分散ファイルシステム（DFS）です。

## 特徴

- ✅ **分散ストレージ**: 複数のChunkServerにデータを分散保存
- ✅ **レプリケーション**: デフォルトで3つのレプリカを作成（冗長性確保）
- ✅ **高可用性 (HA)**: RaftコンセンサスアルゴリズムによるMasterの冗長化
- ✅ **データ整合性**: Raftログレプリケーションによるメタデータの一貫性保証
- ✅ **永続化**: RocksDBを使用したRaftログと状態の永続化
- ✅ **スナップショット**: ログの肥大化を防ぐスナップショット機能
- ✅ **パイプラインレプリケーション**: 効率的なデータ複製
- ✅ **gRPC通信**: 高性能な通信プロトコル
- ✅ **Docker対応**: Docker Composeで簡単にクラスタ構築

## アーキテクチャ

```
┌─────────────────────────────────────────────────┐
│              Raft Consensus Group               │
│                                                 │
│  ┌──────────┐    ┌──────────┐    ┌──────────┐   │
│  │ Master 1 │◄──►│ Master 2 │◄──►│ Master 3 │   │
│  │ (Leader) │    │(Follower)│    │(Follower)│   │
│  └────┬─────┘    └──────────┘    └──────────┘   │
│       │                                         │
└───────┼─────────────────────────────────────────┘
        │ Metadata Management
        │
   ┌────┴────┬────────┬────────┐
   │         │        │        │
┌──▼───┐ ┌──▼───┐ ┌──▼───┐ ┌──▼───┐
│ CS 1 │ │ CS 2 │ │ CS 3 │ │ CS 4 │  ← Data Storage
│(Data)│ │(Data)│ │(Data)│ │(Data)│
└──────┘ └──────┘ └──────┘ └──────┘
```

## クイックスタート

### ローカル開発

```bash
# ビルド
cargo build --release

# Masterサーバー起動 (3ノード構成)
# Terminal 1
cargo run --bin master -- --id 1 --http-port 8081 --addr 127.0.0.1:50051 --peers http://127.0.0.1:8082,http://127.0.0.1:8083 --storage-dir /tmp/raft1

# Terminal 2
cargo run --bin master -- --id 2 --http-port 8082 --addr 127.0.0.1:50052 --peers http://127.0.0.1:8081,http://127.0.0.1:8083 --storage-dir /tmp/raft2

# Terminal 3
cargo run --bin master -- --id 3 --http-port 8083 --addr 127.0.0.1:50053 --peers http://127.0.0.1:8081,http://127.0.0.1:8082 --storage-dir /tmp/raft3

# ChunkServer起動（別ターミナルで）
cargo run --bin chunkserver -- --addr 127.0.0.1:50061 --master-addr 127.0.0.1:50051,127.0.0.1:50052,127.0.0.1:50053 --storage-dir /tmp/cs1
cargo run --bin chunkserver -- --addr 127.0.0.1:50062 --master-addr 127.0.0.1:50051,127.0.0.1:50052,127.0.0.1:50053 --storage-dir /tmp/cs2
cargo run --bin chunkserver -- --addr 127.0.0.1:50063 --master-addr 127.0.0.1:50051,127.0.0.1:50052,127.0.0.1:50053 --storage-dir /tmp/cs3

# ファイル操作
cargo run --bin dfs_cli -- --master http://127.0.0.1:50051,http://127.0.0.1:50052,http://127.0.0.1:50053 put test.txt /test.txt
cargo run --bin dfs_cli -- --master http://127.0.0.1:50051,http://127.0.0.1:50052,http://127.0.0.1:50053 ls
```

### Docker Compose

```bash
# クラスタ起動（Master × 3 + ChunkServer × 5）
./start_cluster.sh

# または手動で
docker-compose up -d

# ファイルアップロード
docker run --rm --network rust-hadoop_dfs-network \
  -v $(pwd)/file.txt:/tmp/file.txt \
  rust-hadoop-master1 \
  /app/dfs_cli --master http://dfs-master1:50051,http://dfs-master2:50051,http://dfs-master3:50051 \
  put /tmp/file.txt /file.txt

# ファイル一覧
docker run --rm --network rust-hadoop_dfs-network \
  rust-hadoop-master1 \
  /app/dfs_cli --master http://dfs-master1:50051,http://dfs-master2:50051,http://dfs-master3:50051 \
  ls
```

## カオスモンキーテスト

レプリケーション機能とMasterの耐障害性をテストします:

```bash
# クラスタ起動
./start_cluster.sh

# 完全なカオステスト
./chaos_test.sh
```

詳細は [CHAOS_TEST.md](CHAOS_TEST.md) を参照してください。

## プロジェクト構成

```
rust-hadoop/
├── proto/
│   └── dfs.proto              # gRPC定義
├── src/
│   ├── bin/
│   │   ├── master.rs          # Masterサーバー (Raft統合)
│   │   ├── chunkserver.rs     # ChunkServerサーバー
│   │   └── dfs_cli.rs         # CLIクライアント
│   ├── master.rs              # Master実装
│   ├── chunkserver.rs         # ChunkServer実装
│   ├── simple_raft.rs         # Raftコンセンサス実装
│   └── lib.rs
├── Dockerfile                 # Dockerイメージ定義
├── docker-compose.yml         # クラスタ構成
├── start_cluster.sh           # クラスタ起動スクリプト
└── chaos_test.sh              # カオステスト
```

## レプリケーション機能

### 仕組み

1. **ブロック割り当て**: Masterが3つのChunkServerを選択
2. **パイプライン転送**: クライアント → CS1 → CS2 → CS3
3. **冗長性**: 2つまでのサーバー障害に耐える

詳細は [REPLICATION.md](REPLICATION.md) を参照してください。

### レプリケーションフロー

```
Client
  │
  │ WriteBlock(data, next=[CS2, CS3])
  ▼
ChunkServer1
  │ 1. Save locally
  │ 2. ReplicateBlock(data, next=[CS3])
  ▼
ChunkServer2
  │ 3. Save locally
  │ 4. ReplicateBlock(data, next=[])
  ▼
ChunkServer3
  │ 5. Save locally
  ▼
Done (3 replicas)
```

## API

### Master Service

- `CreateFile`: ファイル作成
- `GetFileInfo`: ファイル情報取得
- `AllocateBlock`: ブロック割り当て
- `CompleteFile`: ファイル書き込み完了
- `ListFiles`: ファイル一覧
- `RegisterChunkServer`: ChunkServer登録

### ChunkServer Service

- `WriteBlock`: ブロック書き込み（レプリケーション付き）
- `ReadBlock`: ブロック読み込み
- `ReplicateBlock`: レプリケーション受信

## 技術スタック

- **言語**: Rust
- **通信**: gRPC (tonic)
- **シリアライゼーション**: Protocol Buffers
- **非同期**: Tokio
- **CLI**: Clap
- **コンテナ**: Docker

## 開発

### ビルド

```bash
cargo build
```

### テスト

```bash
cargo test
```

### Protoファイル更新時

```bash
# build.rsが自動的に再生成
cargo build
```

## トラブルシューティング

### ChunkServerが登録されない

```bash
# Masterのログ確認
docker-compose logs master

# ChunkServerのログ確認
docker-compose logs chunkserver1
```

### ファイルアップロード失敗

- ChunkServerが3つ以上起動しているか確認
- ネットワーク接続を確認
- ストレージ容量を確認

### Docker関連

```bash
# コンテナ再作成
docker-compose down
docker-compose up -d --force-recreate

# ボリューム削除（全データ削除）
docker-compose down -v
```

## ライセンス

MIT

## 参考資料

- [REPLICATION.md](REPLICATION.md) - レプリケーション機能の詳細
- [CHAOS_TEST.md](CHAOS_TEST.md) - カオステストガイド
- [Hadoop HDFS Architecture](https://hadoop.apache.org/docs/current/hadoop-project-dist/hadoop-hdfs/HdfsDesign.html)
