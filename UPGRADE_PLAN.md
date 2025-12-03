# Library Upgrade Plan: tonic 0.14 & axum Migration

## 目標
- `tonic`: 0.12 → 0.14
- `prost`: 0.13 → 0.14
- `tonic-build`: 0.12 → 0.14
- `warp` → `axum` への移行

## 主な課題
1. `tonic-build 0.14` の API 変更（`configure()`, `compile_protos()` の削除）
2. `http` クレートのバージョン: 0.2 → 1.0
3. `warp` は `http 0.2` を使用、`tonic 0.14` は `http 1.0` を使用
4. **戦略**: 先に `axum` への移行を完了させてから、`tonic 0.14` にアップグレード

---

## マイルストーン（修正版）

### Milestone 1: warp 使用箇所の調査 ✓
**目標**: `warp` を使用している箇所を特定し、`axum` への移行計画を立てる

**タスク**:
- [x] `Cargo.toml` の依存関係を更新
- [ ] `build.rs` を `tonic-build 0.14` の新しい API に対応
- [ ] proto コンパイルが成功することを確認

**成功基準**: `cargo build` でビルドスクリプトが成功する

---

### Milestone 2: HTTP サーバーの移行準備
**目標**: `warp` を使用している箇所を特定し、`axum` への移行計画を立てる

**タスク**:
- [ ] `warp` を使用しているファイルをリストアップ
  - `src/bin/master.rs` (Raft HTTP endpoints)
  - その他
- [ ] 各エンドポイントの機能を文書化
- [ ] `axum` の等価な実装方法を調査

**成功基準**: 移行が必要な箇所が明確になる

---

### Milestone 3: axum への段階的移行
**目標**: `warp` のコードを `axum` に置き換える

**タスク**:
- [ ] `Cargo.toml` に `axum` を追加
- [ ] `src/bin/master.rs` の HTTP サーバーを `axum` に移行
  - [ ] `/raft/vote` エンドポイント
  - [ ] `/raft/append` エンドポイント
  - [ ] `/raft/snapshot` エンドポイント
- [ ] エラーハンドリングを `axum` スタイルに変更
- [ ] 動作確認

**成功基準**: HTTP サーバーが `axum` で動作する

---

### Milestone 4: tonic 0.14 への移行
**目標**: `tonic` と `prost` を 0.14 にアップグレード

**タスク**:
- [ ] `Cargo.toml` で `tonic = "0.14"`, `prost = "0.14"` に更新
- [ ] コンパイルエラーを修正
  - [ ] `http 1.0` の型変更に対応
  - [ ] `tonic::body::Body` の変更に対応
  - [ ] `empty_body()` などの削除された関数を置き換え
- [ ] gRPC クライアント/サーバーの動作確認

**成功基準**: `cargo build` が成功し、gRPC 通信が動作する

---

### Milestone 5: 統合テスト
**目標**: すべての機能が正常に動作することを確認

**タスク**:
- [ ] ユニットテストの実行
- [ ] Master サーバーの起動確認
- [ ] ChunkServer の起動確認
- [ ] CLI の動作確認
- [ ] Raft の動作確認

**成功基準**: すべてのテストが通過し、既存機能が動作する

---

### Milestone 6: クリーンアップ
**目標**: 不要なコードを削除し、ドキュメントを更新

**タスク**:
- [ ] `warp` の依存関係を削除
- [ ] 未使用のコードを削除
- [ ] `README.md` を更新
- [ ] 変更履歴を記録

**成功基準**: コードベースがクリーンで、ドキュメントが最新

---

## 現在の状態
- ✅ `rocksdb 0.24` にアップグレード済み
- ⏸️ `tonic`, `prost`, `tonic-build` は 0.12/0.13 のまま
- 📍 **現在地**: Milestone 1 - ビルドシステムの更新

## 次のステップ
1. `build.rs` を `tonic-build 0.14` の API に対応させる
2. ビルドが成功することを確認
3. Milestone 2 に進む
