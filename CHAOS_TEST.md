# カオスモンキーテスト ガイド

このドキュメントでは、Rust Hadoop DFSのレプリケーション機能をカオスモンキーテストで検証する方法を説明します。

## 概要

カオスモンキーテストは、システムの耐障害性を検証するために、ランダムにサーバーを停止・再起動するテスト手法です。このテストにより、以下を確認できます:

- ✅ レプリケーションが正しく機能しているか
- ✅ サーバー障害時にデータが失われないか
- ✅ システムが障害から自動的に回復できるか
- ✅ 部分的な障害時でも読み書きが可能か

## 環境構成

Docker Composeで以下のサービスを起動します:

- **Master Server** × 1: メタデータ管理
- **Chunk Servers** × 5: データストレージ（レプリケーション係数3に対して十分な冗長性）

```
┌─────────────┐
│   Master    │
│  :50051     │
└──────┬──────┘
       │
   ┌───┴───┬───────┬───────┬───────┐
   │       │       │       │       │
┌──▼──┐ ┌──▼──┐ ┌──▼──┐ ┌──▼──┐ ┌──▼──┐
│ CS1 │ │ CS2 │ │ CS3 │ │ CS4 │ │ CS5 │
│:5005│ │:5005│ │:5005│ │:5005│ │:5005│
│  2  │ │  3  │ │  4  │ │  5  │ │  6  │
└─────┘ └─────┘ └─────┘ └─────┘ └─────┘
```

## クイックスタート

### 1. クラスタの起動

```bash
./start_cluster.sh
```

このスクリプトは以下を実行します:
- Dockerイメージのビルド
- 全サービスの起動
- サービスの準備完了を待機

### 2. カオスモンキーテストの実行

```bash
./chaos_test.sh
```

## テストシナリオ

### Phase 1: 初期セットアップ
1. 10MBのランダムデータファイルを生成
2. DFSにアップロード（3つのChunkServerにレプリケート）
3. MD5チェックサムを保存

### Phase 2: カオステスト

#### Test 1: 単一サーバー障害
```bash
# 1つのChunkServerをランダムに停止
# データのダウンロードと整合性確認
# サーバーを再起動
```

**期待結果**: 残り2つのレプリカからデータを取得可能

#### Test 2: 2サーバー同時障害
```bash
# 2つのChunkServerを同時に停止
# データのダウンロードと整合性確認
# サーバーを再起動
```

**期待結果**: 残り1つのレプリカからデータを取得可能

#### Test 3: ローリング障害
```bash
# 5回繰り返し:
#   - ランダムにサーバーを停止
#   - データをダウンロード
#   - 整合性確認
#   - サーバーを再起動
```

**期待結果**: 各ラウンドでデータの整合性が保たれる

#### Test 4: 障害中のアップロード
```bash
# 1つのChunkServerを停止
# 新しいファイルをアップロード
# サーバーを再起動
```

**期待結果**: 利用可能なサーバーが十分あればアップロード成功

### Phase 3: 最終検証
1. 元のファイルを再度ダウンロード
2. MD5チェックサムで整合性を確認
3. 全ファイルのリスト表示

## 手動テスト

### ファイルのアップロード

```bash
# テストファイルを作成
echo "Hello, DFS!" > test.txt

# アップロード
docker run --rm --network rust-hadoop_dfs-network \
  -v $(pwd)/test.txt:/tmp/test.txt \
  rust-hadoop-master \
  /app/dfs_cli --master http://dfs-master:50051 put /tmp/test.txt /test.txt
```

### ファイルのリスト表示

```bash
docker run --rm --network rust-hadoop_dfs-network \
  rust-hadoop-master \
  /app/dfs_cli --master http://dfs-master:50051 ls
```

### ファイルのダウンロード

```bash
docker run --rm --network rust-hadoop_dfs-network \
  -v $(pwd):/output \
  rust-hadoop-master \
  /app/dfs_cli --master http://dfs-master:50051 get /test.txt /output/downloaded.txt
```

### ChunkServerの手動停止・再起動

```bash
# 停止
docker-compose stop dfs-chunkserver1

# 再起動
docker-compose start dfs-chunkserver1

# ステータス確認
docker-compose ps
```

## ログの確認

### 全サービスのログ

```bash
docker-compose logs -f
```

### 特定のサービスのログ

```bash
# Masterのログ
docker-compose logs -f master

# ChunkServer1のログ
docker-compose logs -f chunkserver1
```

## トラブルシューティング

### サービスが起動しない

```bash
# ログを確認
docker-compose logs

# コンテナを再作成
docker-compose down
docker-compose up -d --force-recreate
```

### ビルドエラー

```bash
# キャッシュをクリアして再ビルド
docker-compose build --no-cache
```

### ネットワークエラー

```bash
# ネットワークを再作成
docker-compose down
docker network prune
docker-compose up -d
```

### ストレージのクリーンアップ

```bash
# 全データを削除
docker-compose down -v

# 再起動
docker-compose up -d
```

## パフォーマンステスト

### 大きなファイルのテスト

```bash
# 100MBのファイルを作成
dd if=/dev/urandom of=large_file.bin bs=1M count=100

# アップロード時間を測定
time docker run --rm --network rust-hadoop_dfs-network \
  -v $(pwd)/large_file.bin:/tmp/large_file.bin \
  rust-hadoop-master \
  /app/dfs_cli --master http://dfs-master:50051 put /tmp/large_file.bin /large.bin

# ダウンロード時間を測定
time docker run --rm --network rust-hadoop_dfs-network \
  -v $(pwd):/output \
  rust-hadoop-master \
  /app/dfs_cli --master http://dfs-master:50051 get /large.bin /output/large_downloaded.bin
```

### 並列アップロード

```bash
# 複数のファイルを並列でアップロード
for i in {1..10}; do
  echo "File $i" > file_$i.txt
  docker run --rm --network rust-hadoop_dfs-network \
    -v $(pwd)/file_$i.txt:/tmp/file_$i.txt \
    rust-hadoop-master \
    /app/dfs_cli --master http://dfs-master:50051 put /tmp/file_$i.txt /file_$i.txt &
done
wait
```

## 期待される結果

### 成功ケース
- ✅ レプリケーション係数3で全ファイルが複製される
- ✅ 2つまでのサーバー障害でもデータアクセス可能
- ✅ ファイルの整合性が常に保たれる
- ✅ サーバー再起動後、自動的にMasterに再登録される

### 失敗ケース（想定内）
- ⚠️ 3つ以上のサーバーが同時に停止するとデータアクセス不可
- ⚠️ 利用可能なサーバーが3つ未満の場合、レプリケーション係数を満たせない

## クリーンアップ

```bash
# クラスタの停止
docker-compose down

# データボリュームも削除
docker-compose down -v

# Dockerイメージも削除
docker rmi rust-hadoop-master
```

## まとめ

このカオスモンキーテストにより、Rust Hadoop DFSのレプリケーション機能が以下を満たすことを確認できます:

1. **耐障害性**: 複数のサーバー障害に耐えられる
2. **データ整合性**: 障害時でもデータが破損しない
3. **自動回復**: サーバー再起動時に自動的に復旧
4. **可用性**: 部分的な障害時でも読み書き可能

これらの特性により、本番環境でも信頼性の高い分散ファイルシステムとして機能することが期待できます。
