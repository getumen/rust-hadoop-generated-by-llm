# カオスモンキーテスト ガイド

このドキュメントでは、Rust Hadoop DFSのシャーディング環境下での耐障害性を検証するカオスモンキーテストの手法について説明します。

## 概要

現在、システムは**Raftコンセンサス**と**シャーディング**（Transaction Recordによるクロスシャード操作を含む）を実装しています。カオステストでは、これらの機能が障害発生時にも正しく動作することを確認します。

主な検証項目:
- ✅ **Master HA**: Masterノード（Leader/Follower）の障害
- ✅ **Cross-Shard Atomic**: クロスシャードトランザクション中の障害とロールバック/コミット
- ✅ **Replication**: ChunkServer障害時のデータ可用性

## 環境構成

**シャーディング構成 (`docker-compose.yml`)** を使用します。

- **Config Server** (Meta-Shard) × 1 (Raft Group)
- **Shard 1** (Master) × 3 (Raft Group)
- **Shard 2** (Master) × 3 (Raft Group)
- **Chunk Servers** × 5 (Shared)

```
┌────────────────────────────────────────┐
│             Config Server              │
└───────────────────┬────────────────────┘
          ┌─────────┴─────────┐
    ┌─────▼─────┐       ┌─────▼─────┐
    │  Shard 1  │       │  Shard 2  │    Metadata
    │ (3 Nodes) │       │ (3 Nodes) │
    └─────┬─────┘       └─────┬─────┘
          │                   │
    ┌─────┴─┬───────┬───────┬─┴─────┐    Data
    │       │       │       │       │
 ┌──▼──┐ ┌──▼──┐ ┌──▼──┐ ┌──▼──┐ ┌──▼──┐
 │ CS1 │ │ CS2 │ │ CS3 │ │ CS4 │ │ CS5 │
 └─────┘ └─────┘ └─────┘ └─────┘ └─────┘
```

## 自動テストスクリプト

以下のスクリプトを実行することで、主要な障害シナリオを自動テストできます。

### 1. 障害復旧テスト (Transaction Recovery)

トランザクション処理中（Prepare/Commitフェーズ）にShardがクラッシュした際の復旧能力を検証します。

```bash
./fault_recovery_test.sh
```

**シナリオ:**
1. クロスシャードRenameの準備（Transaction Record作成）
2. **Crash**: Transactionに関与するShard（SourceまたはDest）を強制停止
3. **Restart**: Shardを再起動
4. **Verification**: 
   - Crashのタイミングに応じて、Transactionが正しくAbort（ロールバック）またはCommit（完了）されるか確認
   - データの整合性が保たれているか確認

### 2. トランザクションアボートテスト

クロスシャード操作中に相手先のShardが応答しない場合のタイムアウトとロールバックを検証します。

```bash
./transaction_abort_test.sh
```

**シナリオ:**
1. Destination Shardを停止
2. Source ShardからRename要求を送信
3. **Expectation**: タイムアウトにより操作が失敗し、Source側のファイルは消えずに残る（Atomic性が保たれる）

### 3. レプリケーションカオステスト

ChunkServerをランダムに停止・再起動しながら、読み書きの継続性を検証します。

```bash
# 従来のレプリケーションテスト（シャーディング環境向けに要調整）
./chaos_test.sh
```

## 手動テスト手順

### 1. 環境セットアップ

```bash
# シャーディングクラスタの起動
docker compose -f docker-compose.yml up -d --build
```

### 2. Shardリーダーの障害テスト

Raftのリーダー選出機能を確認します。

```bash
# Shard 1の状況確認（Leaderを探す）
docker compose -f docker-compose.yml logs -f master-0-0
docker compose -f docker-compose.yml logs -f master-0-1
docker compose -f docker-compose.yml logs -f master-0-2

# Leader（例えば master-0-0）を停止
docker compose -f docker-compose.yml stop master-0-0

# 新しいLeaderが選出されたかログで確認
# クライアント操作が継続できるか確認
docker exec master-0-1 /app/dfs_cli --master http://localhost:50051 ls /
```

### 3. ChunkServer障害テスト

```bash
# ファイルアップロード
docker exec master-0-0 /app/dfs_cli --master http://localhost:50051 put /file.txt /test.txt

# 1つのChunkServerを停止
docker compose -f docker-compose.yml stop chunkserver1

# ファイルが読み込めるか確認（残りのレプリカから読めるはず）
docker exec master-0-0 /app/dfs_cli --master http://localhost:50051 get /test.txt /downloaded.txt
```

## トラブルシューティング

### クラスタの状態リセット

テスト後に完全にクリーンな状態に戻すには：

```bash
docker compose -f docker-compose.yml down -v
```

### ログ確認

特定のシャードやサーバーのログを確認：

```bash
docker compose -f docker-compose.yml logs -f master-0-0
```

## 今後のテスト計画

- [ ] **Split Brain**: ネットワーク分断時の挙動検証
- [ ] **Config Server Crash**: Meta-Shardの障害とRecovery検証
- [ ] **Rolling Upgrade**: 稼働中のローリングアップデート検証
