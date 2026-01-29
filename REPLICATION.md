# レプリケーション機能

このドキュメントでは、Rust Hadoop DFSのレプリケーション機能について説明します。

## 概要

レプリケーション機能により、ファイルブロックが複数のChunkServerに自動的に複製されます。これにより、データの冗長性と可用性が向上します。

## 主な機能

### 1. レプリケーション係数
- デフォルトのレプリケーション係数: **3**
- Masterサーバーが各ブロックに対して最大3つのChunkServerを選択します
- ChunkServerは全シャードで共有されているため、シャードの設定に関わらずグローバルなプールから選択されます
- 利用可能なChunkServerが3つ未満の場合は、利用可能な数だけ選択されます

### 2. レプリケーションパイプライン
- クライアントは最初のChunkServerにのみデータを送信します
- 最初のChunkServerがデータをローカルに保存した後、次のChunkServerに転送します
- このプロセスがパイプライン方式（Chain Replication）で続き、すべてのレプリカが作成されます

### 3. 自動再レプリケーションとバランサー
- **Balancer**: ディスク容量の使用率に基づいて、使用量の多いChunkServerから少ないChunkServerへバックグラウンドでブロックを移動します（実装済み）。
- ChunkServerの障害検知: ハートビートの途絶を検知し、デッドサーバーとしてマークします。

## 実装の詳細

### Master側
- `allocate_block`: 容量の多いChunkServerを優先的に選択してリストを返します。
- `register_chunk_server`: 共通のストレージプールとしてChunkServerを管理します。

### ChunkServer側
- `write_block`: クライアントからのデータを受け取り、自身のディスクに書き込んだ後、次のChunkServerへ転送します。
- `replicate_block`: 前のChunkServerからの転送データを受け取ります。

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

## テスト方法

### シャーディング環境でのテスト

`docker-compose.yml` を使用すると、複数のShard（Master）と共有されたChunkServerプールが起動します。

```bash
# クラスタ起動
docker compose -f docker-compose.yml up -d --build

# ファイルアップロード（レプリケーション確認）
docker exec master-0-0 /app/dfs_cli --master http://localhost:50051 put /test.txt /test.txt
# Output: Replicating to 3 servers: ...
```

### データの確認

ChunkServerのデータディレクトリを確認することで、実際に複製されているかチェックできます。

```bash
docker compose -f docker-compose.yml exec chunkserver1 ls -l /data/
docker compose -f docker-compose.yml exec chunkserver2 ls -l /data/
```

## 既存の改善状況

- ✅ **負荷分散 (Balancer)**: 実装済み。容量に基づいてブロック移動を行います。
- ✅ **シャーディング対応**: 複数のMasterが共通のChunkServerプールを利用できるようになりました。
- ✅ **読み取り最適化**: 部分読み取り、並行フェッチ、キャッシング機能を実装済み。

## 読み取り最適化 (Read Optimization)

DFSの読み取り性能を大幅に向上させるため、複数の最適化機能が実装されています。

### 実装済みの機能

#### 1. 部分ブロック読み取り (Partial Block Reads)

従来は1ブロック全体を読み取る必要がありましたが、必要な範囲のみを効率的に読み取れるようになりました。

**プロトコル拡張**:
```protobuf
message ReadBlockRequest {
  string block_id = 1;
  optional uint64 offset = 2;   // 読み取り開始位置（バイト）
  optional uint64 length = 3;   // 読み取りサイズ（バイト）
}
```

**ChunkServer実装**:
- Seek-based I/O を使用し、ファイルの必要な部分のみをディスクから読み取ります
- 大容量ブロックでも低レイテンシで応答可能

#### 2. LRUブロックキャッシュ

頻繁にアクセスされるブロックをメモリ上にキャッシュし、ディスクI/Oを削減します。

**設定方法**:
```bash
# 環境変数でキャッシュサイズを指定（デフォルト: 100ブロック）
export BLOCK_CACHE_SIZE=200
./chunkserver
```

**動作**:
- LRU (Least Recently Used) アルゴリズムで古いブロックを自動削除
- 読み取りリクエストごとにキャッシュヒット率をログ出力
- ホットデータへの高速アクセスを実現

#### 3. 並行ブロックフェッチ (Concurrent Block Fetching)

ファイルの複数ブロックを並行してダウンロードし、スループットを向上させます。

**Client API**:
```rust
// 従来: 順次ダウンロード
client.get_file(path, dest).await?;

// 最適化: 並行ダウンロード
client.get_file_concurrent(path, dest).await?;
```

**動作**:
- 複数のChunkServerから同時にブロックを取得
- ネットワーク帯域を最大限活用
- 大容量ファイルのダウンロード時間を大幅に短縮

#### 4. 範囲読み取りAPI (Range Read API)

ファイルの指定バイト範囲のみを効率的に読み取るクライアントAPIです。

**Client API**:
```rust
// バイト範囲 [offset, offset+length) を読み取り
let data = client.read_file_range(path, offset, length).await?;
```

**用途**:
- 大容量ファイルの一部のみが必要な場合
- S3 Range Requestの実装基盤
- データベースやインデックスファイルへの効率的なアクセス

#### 5. S3レンジリクエスト最適化

S3互換APIでHTTP Range Requestを受信した際、全ファイルをダウンロードせずに必要な範囲のみを返します。

**動作**:
```bash
# クライアントからのリクエスト
curl -H "Range: bytes=1000-2000" http://localhost:9000/bucket/file.dat

# S3サーバーの動作
# 1. read_file_range()で必要な範囲のみを取得
# 2. HTTP 206 Partial Content を返却
# 3. Content-Range ヘッダーで範囲情報を通知
```

**利点**:
- AWS S3と同様の動作で既存ツールと互換性が高い
- Sparkなどの分析ツールでのParquet/ORC列指向フォーマットの効率的な読み取り
- 大容量ファイルの部分アクセスでの帯域幅削減

#### 6. ブロックサイズ調整

ファイル書き込み完了時、最終ブロックのサイズを実際のデータサイズに合わせて調整します。

**動作**:
- `CompleteFile` RPC受信時、ChunkServerに最終ブロックサイズを通知
- 読み取り時に正確なファイルサイズを保証
- 部分読み取りとの連携で境界処理を正確に実行

### ReadIndex プロトコル

Raft Leader上での読み取り整合性を保証しつつ、低レイテンシを実現します。

**動作原理**:
1. Leaderが現在のコミットインデックスを記録
2. Heartbeatで過半数の確認応答を待機
3. 応答確認後、読み取りを実行（ログ追記不要）

**利点**:
- 読み取り専用操作でもRaftログに追記不要
- Leaderからの読み取りで線形化可能性を保証
- 読み取りスループットの向上

### パフォーマンスへの影響

これらの最適化により、以下のような性能向上が期待できます:

- **部分読み取り**: 大容量ブロックでのレイテンシを90%以上削減
- **LRUキャッシュ**: ホットデータへのアクセスで80%以上のレイテンシ削減
- **並行フェッチ**: 大容量ファイルのダウンロード時間を最大70%短縮
- **S3レンジリクエスト**: Sparkでの列指向フォーマット読み取りで帯域幅を50%以上削減

## 今後の改善点

### レプリケーション関連
1. **動的レプリケーション係数**: ファイルごとにレプリケーション係数を設定可能にする
2. **ラック認識**: 異なるラックにレプリカを配置してフォールトトレランスを向上
3. **自動修復 (Self-Healing)**: レプリカ数が不足した場合に、即座に新しいChunkServerへコピーを作成する（現在はScaler/Balancerによる移動がメイン）

### 読み取り最適化 (Phase 2)
1. **Follower読み取り**: Followerノードからの読み取りを許可し、読み取り負荷を分散
2. **整合性レベル設定**: アプリケーションごとに強整合性/結果整合性を選択可能に
3. **gRPCストリーミング**: 大容量ブロックの転送でストリーミングレスポンスを実装
4. **先読み戦略**: シーケンシャルアクセスパターンを検出し、自動的に次のブロックをプリフェッチ
