# レプリケーション機能

このドキュメントでは、Rust Hadoop DFSに追加されたレプリケーション機能について説明します。

## 概要

レプリケーション機能により、ファイルブロックが複数のChunkServerに自動的に複製されます。これにより、データの冗長性と可用性が向上します。

## 主な機能

### 1. レプリケーション係数
- デフォルトのレプリケーション係数: **3**
- Masterサーバーが各ブロックに対して最大3つのChunkServerを選択します
- 利用可能なChunkServerが3つ未満の場合は、利用可能な数だけ選択されます

### 2. レプリケーションパイプライン
- クライアントは最初のChunkServerにのみデータを送信します
- 最初のChunkServerがデータをローカルに保存した後、次のChunkServerに転送します
- このプロセスがパイプライン方式で続き、すべてのレプリカが作成されます

### 3. 実装の詳細

#### protoファイルの変更
- `WriteBlockRequest`に`next_servers`フィールドを追加
- `ReplicateBlock` RPCを`ChunkServerService`に追加
- `ReplicateBlockRequest`と`ReplicateBlockResponse`メッセージを追加

#### Masterサーバーの変更
- `allocate_block`メソッドで複数のChunkServerを選択
- レプリケーション係数に基づいてサーバーを選択

#### ChunkServerの変更
- `write_block`メソッドでレプリケーションパイプラインをサポート
- `replicate_block`メソッドを実装して、他のChunkServerからのレプリケーションリクエストを処理

#### CLIの変更
- `put`コマンドで`next_servers`を最初のChunkServerに渡す
- レプリケーション状況をユーザーに表示

## テスト方法

### 1. Masterサーバーを起動
```bash
cargo run --bin master
```

### 2. 複数のChunkServerを起動（異なるターミナルで）
```bash
# ChunkServer 1
cargo run --bin chunkserver -- --addr 127.0.0.1:50052 --storage-dir /tmp/cs1

# ChunkServer 2
cargo run --bin chunkserver -- --addr 127.0.0.1:50053 --storage-dir /tmp/cs2 --master-addr 127.0.0.1:50051

# ChunkServer 3
cargo run --bin chunkserver -- --addr 127.0.0.1:50054 --storage-dir /tmp/cs3 --master-addr 127.0.0.1:50051
```

### 3. ファイルをアップロード
```bash
cargo run --bin dfs_cli -- put test.txt /test.txt
```

出力例:
```
Replicating to 3 servers: ["127.0.0.1:50052", "127.0.0.1:50053", "127.0.0.1:50054"]
File uploaded successfully with replication
```

### 4. レプリケーションを確認
各ChunkServerのストレージディレクトリを確認して、ブロックが複製されていることを確認します:

```bash
ls -la /tmp/cs1/
ls -la /tmp/cs2/
ls -la /tmp/cs3/
```

すべてのディレクトリに同じblock_idのファイルが存在するはずです。

### 5. ファイルをダウンロード
```bash
cargo run --bin dfs_cli -- get /test.txt downloaded.txt
```

いずれかのChunkServerがダウンしていても、他のレプリカからデータを取得できます。

## アーキテクチャ

```
Client
  |
  | 1. WriteBlock(data, next_servers=[CS2, CS3])
  v
ChunkServer1
  |
  | 2. Save locally
  | 3. ReplicateBlock(data, next_servers=[CS3])
  v
ChunkServer2
  |
  | 4. Save locally
  | 5. ReplicateBlock(data, next_servers=[])
  v
ChunkServer3
  |
  | 6. Save locally
  v
Done
```

## 今後の改善点

1. **動的レプリケーション係数**: ファイルごとにレプリケーション係数を設定可能にする
2. **ラック認識**: 異なるラックにレプリカを配置してフォールトトレランスを向上
3. **レプリケーション検証**: Masterがレプリケーションの完了を検証
4. **自動再レプリケーション**: ChunkServerがダウンした場合に自動的に再レプリケーション
5. **負荷分散**: ChunkServerの負荷を考慮してレプリカの配置を最適化
