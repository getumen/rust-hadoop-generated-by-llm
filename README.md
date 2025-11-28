# Rust Hadoop DFS

Rustで実装されたHadoop互換の分散ファイルシステム（DFS）です。

## 特徴

- ✅ **分散ストレージ**: 複数のChunkServerにデータを分散保存
- ✅ **レプリケーション**: デフォルトで3つのレプリカを作成（冗長性確保）
- ✅ **耐障害性**: サーバー障害時でもデータアクセス可能
- ✅ **パイプラインレプリケーション**: 効率的なデータ複製
- ✅ **gRPC通信**: 高性能な通信プロトコル
- ✅ **Docker対応**: Docker Composeで簡単にクラスタ構築

## アーキテクチャ

```
┌─────────────────┐
│  Master Server  │  ← メタデータ管理
│   (Metadata)    │
└────────┬────────┘
         │
    ┌────┴────┬────────┬────────┐
    │         │        │        │
┌───▼───┐ ┌──▼───┐ ┌──▼───┐ ┌──▼───┐
│ CS 1  │ │ CS 2 │ │ CS 3 │ │ CS 4 │  ← データストレージ
│(Data) │ │(Data)│ │(Data)│ │(Data)│
└───────┘ └──────┘ └──────┘ └──────┘
```

## クイックスタート

### ローカル開発

```bash
# ビルド
cargo build --release

# Masterサーバー起動
cargo run --bin master

# ChunkServer起動（別ターミナルで）
cargo run --bin chunkserver -- --addr 127.0.0.1:50052
cargo run --bin chunkserver -- --addr 127.0.0.1:50053 --storage-dir /tmp/cs2
cargo run --bin chunkserver -- --addr 127.0.0.1:50054 --storage-dir /tmp/cs3

# ファイル操作
cargo run --bin dfs_cli -- put test.txt /test.txt
cargo run --bin dfs_cli -- ls
cargo run --bin dfs_cli -- get /test.txt downloaded.txt
```

### Docker Compose

```bash
# クラスタ起動（Master + ChunkServer × 5）
./start_cluster.sh

# または手動で
docker-compose up -d

# ファイルアップロード
docker run --rm --network rust-hadoop_dfs-network \
  -v $(pwd)/file.txt:/tmp/file.txt \
  rust-hadoop-master \
  /app/dfs_cli --master http://dfs-master:50051 put /tmp/file.txt /file.txt

# ファイル一覧
docker run --rm --network rust-hadoop_dfs-network \
  rust-hadoop-master \
  /app/dfs_cli --master http://dfs-master:50051 ls

# ファイルダウンロード
docker run --rm --network rust-hadoop_dfs-network \
  -v $(pwd):/output \
  rust-hadoop-master \
  /app/dfs_cli --master http://dfs-master:50051 get /file.txt /output/downloaded.txt
```

## カオスモンキーテスト

レプリケーション機能の耐障害性をテストします:

```bash
# クラスタ起動
./start_cluster.sh

# 簡易テスト（1サーバー停止）
./simple_chaos_test.sh

# 完全なカオステスト（複数シナリオ）
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
│   │   ├── master.rs          # Masterサーバー
│   │   ├── chunkserver.rs     # ChunkServerサーバー
│   │   └── dfs_cli.rs         # CLIクライアント
│   ├── master.rs              # Master実装
│   ├── chunkserver.rs         # ChunkServer実装
│   └── lib.rs
├── Dockerfile                 # Dockerイメージ定義
├── docker-compose.yml         # クラスタ構成
├── start_cluster.sh           # クラスタ起動スクリプト
├── simple_chaos_test.sh       # 簡易カオステスト
└── chaos_test.sh              # 完全カオステスト
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
