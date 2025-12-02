# Master High Availability (HA) ガイド

このドキュメントでは、Rust Hadoop DFSのMasterサーバーの高可用性（HA）構成について説明します。
以前のファイルロックベースのHAは廃止され、より堅牢な**Raftコンセンサスアルゴリズム**に基づいた実装に移行しました。

## 概要

Master HAは、以下のRaftの仕組みで実現されています：

- **Leader/Followerモデル**: 3つ以上の奇数個のMasterノードでクラスタを構成し、1つのLeaderを選出します。
- **ログレプリケーション**: すべてのメタデータ変更操作（ファイル作成、ブロック割り当てなど）はRaftログとして全ノードに複製されます。
- **強整合性**: 過半数のノードがログを保存した時点でコミットとみなされ、データの消失を防ぎます。
- **自動フェイルオーバー**: Leaderがダウンした場合、残りのノードから自動的に新しいLeaderが選出されます。
- **永続化**: RocksDBを使用してRaftの状態（Term, Vote, Log）をディスクに永続化し、再起動後も状態を維持します。

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
│ CS 1 │ │ CS 2 │ │ CS 3 │ │ CS 4 │
│(Data)│ │(Data)│ │(Data)│ │(Data)│
└──────┘ └──────┘ └──────┘ └──────┘
```

## 実装の詳細

### 1. リーダー選出 (Leader Election)

- 各ノードは起動時、**Follower**として開始します。
- Leaderからのハートビートが一定時間（選挙タイムアウト：150-300msのランダム）途絶えると、**Candidate**になり選挙を開始します。
- 過半数の票を集めたCandidateが新しい**Leader**になります。
- Leaderは定期的にハートビート（AppendEntries RPC）を送信し、権威を維持します。

### 2. ログレプリケーション (Log Replication)

1. クライアントはLeaderにリクエスト（例: `CreateFile`）を送信します。
2. Leaderはリクエストを自身のログに追加し、Followerに並列で送信します。
3. 過半数のFollowerがログを保存したことを確認すると、Leaderはログを**コミット**し、状態マシン（ファイルシステムメタデータ）に適用します。
4. Leaderはクライアントに応答を返します。
5. FollowerはLeaderからの次のハートビートでコミットインデックスの更新を知り、自身の状態マシンにログを適用します。

### 3. 永続化 (Persistence)

RocksDBを使用して以下のデータを永続化しています：

- `current_term`: 現在のTerm番号
- `voted_for`: 現在のTermで投票した候補者ID
- `log`: ログエントリ（Term, Command）

これにより、ノードがクラッシュして再起動しても、以前の状態から再開できます。

### 4. スナップショット (Snapshot)

ログが無限に増えるのを防ぐため、定期的にスナップショットを作成します。

- ログサイズが一定（例: 100エントリ）を超えると、現在のアプリケーション状態（MasterState）をシリアライズして保存します。
- スナップショットに含まれる古いログエントリは削除（Truncate）されます。
- 遅れているFollowerには、ログの代わりにスナップショットを転送（InstallSnapshot RPC）して同期させます。

### 5. クライアントの動作

- クライアント（CLI, ChunkServer）は複数のMasterアドレスを知っています。
- 最初はいずれかのMasterに接続を試みます。
- 接続したMasterがLeaderでない場合、Masterは「Not Leader」エラーと共に**Leader Hint**（現在のLeaderのアドレス）を返します。
- クライアントはHintを元にLeaderに再接続します。
- Leaderがダウンしている場合、クライアントは他のMasterに接続を試み、新しいLeaderが見つかるまでリトライします。

## Docker Compose構成

3ノード構成の例：

```yaml
  master1:
    image: rust-hadoop-master
    container_name: dfs-master1
    command: /app/master --id 1 --http-port 8080 --addr 0.0.0.0:50051 --peers http://dfs-master2:8080,http://dfs-master3:8080 --advertise-addr dfs-master1:50051 --storage-dir /data/raft
    volumes:
      - master1-data:/data/raft
    ports:
      - "50051:50051"
      - "8081:8080"

  master2:
    image: rust-hadoop-master
    container_name: dfs-master2
    command: /app/master --id 2 --http-port 8080 --addr 0.0.0.0:50051 --peers http://dfs-master1:8080,http://dfs-master3:8080 --advertise-addr dfs-master2:50051 --storage-dir /data/raft
    volumes:
      - master2-data:/data/raft
    ports:
      - "50052:50051"
      - "8082:8080"

  master3:
    image: rust-hadoop-master
    container_name: dfs-master3
    command: /app/master --id 3 --http-port 8080 --addr 0.0.0.0:50051 --peers http://dfs-master1:8080,http://dfs-master2:8080 --advertise-addr dfs-master3:50051 --storage-dir /data/raft
    volumes:
      - master3-data:/data/raft
    ports:
      - "50053:50051"
      - "8083:8080"
```

## 運用・トラブルシューティング

### クラスタの状態確認

現在の実装ではHTTPエンドポイントはRaft内部通信用ですが、将来的にはステータス確認用APIを追加予定です。
現状はログを確認するのが確実です。

```bash
docker-compose logs -f master1
```

`Node 1 becoming LEADER` や `Node 1 starting election` などのログで状態を確認できます。

### ノードの復旧

ノードがダウンしても、データは永続化ボリューム（`masterX-data`）に残っています。
コンテナを再起動すれば、自動的にクラスタに復帰し、遅れているログやスナップショットを取得して同期します。

```bash
docker-compose start master1
```

### 全ノード停止からの復旧

全ノードが停止しても、永続化データがあれば復旧可能です。
過半数のノード（3ノード構成なら2ノード）が起動すれば、サービスは再開します。

### データの整合性

- **メタデータ**: Raftにより強整合性が保証されます。
- **ファイルデータ**: ChunkServerへの書き込みはパイプラインレプリケーションで行われますが、Masterへのメタデータ登録（`CompleteFile`）が完了して初めてファイルとして認識されます。

## 今後の改善予定

- **動的なメンバーシップ変更**: 稼働中のクラスタへのノード追加・削除
- **ReadIndex**: Followerからの読み取り（負荷分散）
- **Pre-Vote**: 無駄な選挙の抑制
```
