# Dynamic Sharding ガイド

このドキュメントでは、Rust Hadoop DFSの**Dynamic Sharding**機能について詳しく説明します。
この実装は、S3やGoogle Colossusなどの大規模分散ストレージシステムで採用されているRange-basedシャーディングと負荷適応型スケーリングの手法を参考にしています。

## 概要

Dynamic Shardingは、メタデータの負荷に応じて自動的にShardを分割（Split）または統合（Merge）することで、システム全体のスループットとスケーラビリティを向上させる機能です。

### 主な特徴

- **Range-based Partitioning**: ファイルパスの辞書順範囲でShardを分割
- **プレフィックス局所性**: 同じディレクトリのファイルが同じShardに配置される
- **負荷監視**: リアルタイムでスループット（RPS, BPS）を計測
- **自動Split**: 閾値を超えた場合、Shardを自動的に2つに分割
- **データ移行（Shuffle）**: 分割後、該当するメタデータを新Shardへ転送
- **透過的な運用**: クライアントは自動的にShardMap更新でルーティング変更に対応

## アーキテクチャ

### Range-based Partitioning

Consistent Hashingではなく、**辞書順の範囲分割**を採用しています。

```
初期状態:
┌────────────────────────────────┐
│  Shard-1: "" から "" (全範囲)  │
└────────────────────────────────┘

分割後:
┌─────────────────┐  ┌─────────────────┐
│ Shard-1: ""~"/m"│  │ Shard-2: "/m"~""│
└─────────────────┘  └─────────────────┘
     (/a, /b, ...)        (/n, /z, ...)

さらに分割:
┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐
│Shard-1  │ │Shard-2  │ │Shard-3  │ │Shard-4  │
│ ""~"/e" │ │"/e"~"/m"│ │"/m"~"/s"│ │"/s"~"" │
└─────────┘ └─────────┘ └─────────┘ └─────────┘
```

### ShardMap構造

Config Serverが管理するShardMapは以下の情報を持ちます：

```rust
pub struct ShardMap {
    shards: Vec<ShardId>,
    ranges: HashMap<ShardId, (String, String)>,  // (start_key, end_key)
    leaders: HashMap<ShardId, String>,           // Leader address
    strategy: ShardingStrategy,                  // Range or Hash
}

pub enum ShardingStrategy {
    Range,
    Hash { virtual_nodes: usize },
}
```

現在の実装では`ShardingStrategy::Range`を使用しています。

## スループット監視

### ThroughputMonitor

各Masterは`ThroughputMonitor`を保持し、リクエストごとにメトリクスを記録します。

```rust
pub struct ThroughputMonitor {
    window: RwLock<VecDeque<RequestRecord>>,
    window_duration: Duration,
}

pub struct RequestRecord {
    timestamp: Instant,
    path: String,
    size: u64,
    is_write: bool,
}
```

#### 計測対象のリクエスト

- **書き込み**: `CreateFile`, `AllocateBlock`, `CompleteFile`, `Rename`（Source側）
- **読み取り**: `GetFileInfo`, `ListFiles`, `Rename`（Dest側）

#### メトリクスの取得

```rust
pub struct ThroughputMetrics {
    pub read_rps: f64,   // 読み取りリクエスト/秒
    pub write_rps: f64,  // 書き込みリクエスト/秒
    pub read_bps: f64,   // 読み取りバイト/秒
    pub write_bps: f64,  // 書き込みバイト/秒
}
```

スライディングウィンドウ（デフォルト60秒）内のリクエストを集計し、秒あたりの値を計算します。

## シャード分割（Split）

### Split判定

Leaderは定期的（10秒ごと）に以下のロジックでSplitを判定します：

```rust
async fn check_and_split(&self) {
    let metrics = self.monitor.get_metrics();
    let total_rps = metrics.read_rps + metrics.write_rps;

    if total_rps > self.split_threshold_rps {
        // Cooldown期間をチェック（前回Split後一定時間経過しているか）
        if last_split_time.elapsed() < self.split_cooldown {
            return;
        }

        // Split実行
        self.initiate_split().await;
    }
}
```

### Split実行フロー

#### Phase 1: 分割点の決定

```rust
fn determine_split_key(&self, state: &MasterState) -> String {
    let mut paths: Vec<_> = state.files.keys().collect();
    paths.sort();

    // 中央値を分割点として選択
    let mid_idx = paths.len() / 2;
    paths[mid_idx].clone()
}
```

#### Phase 2: Raft合意

```rust
Command::SplitShard {
    new_shard_id: String,      // 新Shard ID（UUIDなど）
    split_key: String,         // 分割点のパス
    new_shard_leader: String,  // 新ShardのLeaderアドレス
}
```

このコマンドがRaftログに書き込まれ、過半数の合意を得た後、状態マシンに適用されます。

#### Phase 3: Config Server更新

```rust
// 新Shardの範囲を登録
config_client.add_shard(AddShardRequest {
    shard_id: new_shard_id,
    peers: vec![new_shard_leader],
    start_key: split_key.clone(),
    end_key: original_end_key,
}).await?;

// 旧Shardの範囲を更新
config_client.update_shard_range(UpdateShardRangeRequest {
    shard_id: original_shard_id,
    start_key: original_start_key,
    end_key: split_key,
}).await?;
```

#### Phase 4: データ移行（Shuffle）

旧Shardから新Shardへファイルメタデータを転送します。

```rust
// 1. 旧Shardが新Shardへ InitiateShuffle RPC を送信
let files_to_transfer: Vec<FileMetadata> = state.files
    .iter()
    .filter(|(path, _)| path >= &split_key)
    .map(|(_, meta)| meta.clone())
    .collect();

new_shard_client.initiate_shuffle(InitiateShuffleRequest {
    files: files_to_transfer,
}).await?;

// 2. 新Shardがファイルを受信し、Raftログに書き込む
for file in request.files {
    apply_command(Command::CreateFileFromShuffle {
        path: file.path,
        metadata: file,
    });
}

// 3. 旧Shardから該当ファイルを削除
for path in transferred_paths {
    apply_command(Command::DeleteFileFromShuffle { path });
}
```

### Splitのタイミング調整

#### Cooldown期間

頻繁なSplitを防ぐため、クールダウン期間（デフォルト300秒）を設けています。

```bash
# Masterコマンドライン引数
--split-cooldown-secs 300
```

#### 閾値の設定

```bash
# Split閾値（RPS）
--split-threshold-rps 1000.0

# Merge閾値（RPS、将来実装）
--merge-threshold-rps 100.0
```

## クライアント側の対応

### ShardMapキャッシュ

クライアントライブラリは`ShardMap`をキャッシュし、パスから適切なShardを判定します。

```rust
impl Client {
    pub fn get_shard_for_path(&self, path: &str) -> Option<String> {
        let shard_map = self.shard_map.lock().unwrap();

        for (shard_id, (start, end)) in &shard_map.ranges {
            if path >= start && (end.is_empty() || path < end) {
                return Some(shard_id.clone());
            }
        }
        None
    }
}
```

### REDIRECTハンドリング

Shardが分割された後、古いShardMapを持つクライアントは誤ったShardにアクセスする可能性があります。

```rust
match master_client.create_file(request).await {
    Err(e) if e.code() == Code::OutOfRange => {
        // REDIRECT エラーを検出
        if let Some(hint) = extract_redirect_hint(&e) {
            // ShardMapを更新
            self.refresh_shard_map().await?;

            // 正しいShardにリトライ
            let correct_shard = self.get_shard_for_path(&path)?;
            master_client = self.get_master_client(&correct_shard)?;
            master_client.create_file(request).await?;
        }
    }
    Ok(response) => return Ok(response),
    Err(e) => return Err(e),
}
```

## 運用と監視

### メトリクス確認

各MasterのHTTPエンドポイントでスループットメトリクスを確認できます。

```bash
# Prometheus メトリクス
curl http://localhost:8080/metrics

# Raft状態（JSONフォーマット）
curl http://localhost:8080/raft/state | jq .
```

主要メトリクス：
- `dfs_master_read_rps`: 読み取りリクエスト/秒
- `dfs_master_write_rps`: 書き込みリクエスト/秒
- `dfs_master_read_bps`: 読み取りバイト/秒
- `dfs_master_write_bps`: 書き込みバイト/秒
- `dfs_master_split_count`: Split実行回数

### ShardMap確認

Config ServerからShardMapを取得：

```bash
curl http://localhost:50050/shards | jq .
```

出力例：
```json
{
  "shards": ["shard-1", "shard-2", "shard-3"],
  "ranges": {
    "shard-1": ["", "/e"],
    "shard-2": ["/e", "/m"],
    "shard-3": ["/m", ""]
  },
  "leaders": {
    "shard-1": "http://master1-shard1:50051",
    "shard-2": "http://master1-shard2:50051",
    "shard-3": "http://master1-shard3:50051"
  }
}
```

### CLIツールでの確認

```bash
# Inspect コマンドでShardMapを表示
docker exec dfs-master1-shard1 /app/dfs_cli \
  --master http://localhost:50051 \
  --config-servers http://config-server:50050 \
  inspect
```

## テストとデバッグ

### 自動スケーリングテスト

```bash
# ダイナミックシャーディングのテスト
./test_scripts/auto_scaling_test.sh
```

このテストは以下を検証します：
1. 初期状態（単一Shard）の確認
2. 大量ファイルアップロードによる負荷発生
3. 自動Split発生の確認
4. ShardMap更新の確認
5. 新旧Shard間でのファイルアクセス確認

### 手動Split実行

開発・デバッグ用に、CLIから手動でSplitを実行することも可能です（将来実装予定）：

```bash
# 手動Split（コマンド実装待ち）
docker exec dfs-master1-shard1 /app/dfs_cli \
  --master http://localhost:50051 \
  admin split --shard-id shard-1 --split-key "/m"
```

## パフォーマンス考慮事項

### Split時のレイテンシ

- **Raft合意**: 数百ミリ秒（ネットワーク遅延 + ログ書き込み）
- **データ移行**: ファイル数に比例（1000ファイルで約1-2秒）
- **Config Server更新**: 数十ミリ秒

Split中も読み取りは継続可能ですが、書き込みは一時的にブロックされる場合があります。

### スループット改善効果

- **Split前**: 単一Shard = 1,000 RPS（ボトルネック）
- **Split後**: 2 Shards = 各500 RPS → 合計1,000 RPS維持（負荷分散）
- **さらなるSplit**: 4 Shards → 各250 RPS → スケールアウト

### メモリ使用量

- **ThroughputMonitor**: 60秒 × 1,000 RPS = 最大60,000レコード（約5-10 MB）
- **ShardMap**: Shard数 × 数KB（ほぼ無視可能）

## トラブルシューティング

### Q: Splitが実行されない

**確認項目**:
1. スループット閾値に到達しているか？
   ```bash
   curl http://localhost:8080/metrics | grep rps
   ```
2. Cooldown期間が経過しているか？
3. Leaderが存在するか？（Followerでは実行されない）

### Q: Shuffle中にエラーが発生

**対処法**:
- Transaction Recordと同様、RaftログでShuffleを管理しているため、再起動後も復旧可能
- ログを確認: `docker logs dfs-master1-shard1 | grep Shuffle`

### Q: クライアントがREDIRECTループ

**原因**: ShardMapの更新失敗

**対処法**:
1. Config Serverが正常か確認
   ```bash
   curl http://localhost:50050/health
   ```
2. クライアントのShardMapキャッシュをクリア（再接続）

## Shard統合（Merge）

低負荷時の隣接Shardマージも実装済みです。

### Merge判定

Leaderは定期的（10秒ごと）に以下のロジックでMergeを判定します：

```rust
async fn check_and_merge(&self) {
    let metrics = self.monitor.get_metrics();
    let total_rps = metrics.read_rps + metrics.write_rps;

    if total_rps < self.merge_threshold_rps {
        // 隣接Shardを探してMerge提案
        if let Some(neighbor_shard) = find_adjacent_shard() {
            self.initiate_merge(neighbor_shard).await;
        }
    }
}
```

### Merge実行フロー

1. **低負荷検出**: Leaderがスループット閾値を下回ったことを検出
2. **隣接Shard特定**: ShardMapから範囲が隣接するShardを探索
3. **Merge提案**: 隣接ShardへMerge RPCを送信
4. **データ移行**: 被統合Shard（victim）から統合先Shard（retained）へメタデータを転送
5. **Raft合意**: `MergeShard`コマンドをRaftログに書き込み
6. **Config Server更新**: 被統合Shardを削除し、統合先Shardの範囲を更新
7. **完了**: 被統合Shardは停止

### 設定

```bash
# Merge閾値（RPS）- 負の値で無効化
--merge-threshold-rps 100.0

# テスト環境では無効化推奨（-1.0）
--merge-threshold-rps=-1.0
```

## 今後の拡張

### マルチレベルSplit

プレフィックスごとに再帰的にSplitする機能（例: `/user/alice/*` 専用のShard）

### 負荷予測

過去のスループット履歴から、Split/Mergeを先読みして実行

## 参考資料

- [Google Colossus](https://cloud.google.com/blog/products/storage-data-transfer/a-peek-behind-colossus-googles-file-system) - Range-based sharding
- [Amazon S3](https://aws.amazon.com/blogs/aws/amazon-s3-object-lambda/) - Prefix-based partitioning
- [Facebook Tectonic](https://www.usenix.org/conference/fast21/presentation/pan) - Dynamic sharding and load balancing
- [MASTER_HA.md](MASTER_HA.md) - 本システムのHA構成とシャーディング詳細

---

**最終更新日**: 2026-01-27
**メンテナー**: Development Team
