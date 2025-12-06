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

`docker-compose-sharded.yml` を使用すると、複数のShard（Master）と共有されたChunkServerプールが起動します。

```bash
# クラスタ起動
docker compose -f docker-compose-sharded.yml up -d --build

# ファイルアップロード（レプリケーション確認）
docker exec master-0-0 /app/dfs_cli --master http://localhost:50051 put /test.txt /test.txt
# Output: Replicating to 3 servers: ...
```

### データの確認

ChunkServerのデータディレクトリを確認することで、実際に複製されているかチェックできます。

```bash
docker compose -f docker-compose-sharded.yml exec chunkserver1 ls -l /data/
docker compose -f docker-compose-sharded.yml exec chunkserver2 ls -l /data/
```

## 既存の改善状況

- ✅ **負荷分散 (Balancer)**: 実装済み。容量に基づいてブロック移動を行います。
- ✅ **シャーディング対応**: 複数のMasterが共通のChunkServerプールを利用できるようになりました。

## 今後の改善点

1. **動的レプリケーション係数**: ファイルごとにレプリケーション係数を設定可能にする
2. **ラック認識**: 異なるラックにレプリカを配置してフォールトトレランスを向上
3. **自動修復 (Self-Healing)**: レプリカ数が不足した場合に、即座に新しいChunkServerへコピーを作成する（現在はScaler/Balancerによる移動がメイン）
