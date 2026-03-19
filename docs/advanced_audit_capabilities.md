# Advanced Audit Capabilities 実装計画および設計 (v2: 堅牢性・一貫性強化版)

## 1. 目的と要件
TODO.md に記載されている `Advanced Audit Capabilities` 以下の3つの機能を実装し、S3互換サーバーの監査ログをより堅牢で効率的かつ分析しやすいものにする。
今回の設計では、パフォーマンスとセキュリティ上の潜在的な課題を解決するための改善策（HMAC, 順序保証, ZSTD, CF分離）を盛り込んでいる。

1. **Tamper-evident Logging (HMAC-SHA256 Chaining)**: ハッシュチェーンとシークレットキーによる強固なログ改ざん検知。
2. **Resource-based Indexing (CF)**: Column Familiesによるリソースごとの独立した高速検索インデックス空間。
3. **Audit Log Compression (ZSTD)**: 高圧縮率のZSTDを用いたRocksDBのデータ圧縮によるストレージ効率化。

---

## 2. 詳細設計と変更箇所

### 2.1 Tamper-evident Logging (HMAC-SHA256 Chaining)
署名鍵（HMACシークレット）がないとハッシュの再計算が行えないようにし、書き込みの順序関係を厳密に保証する。

*   **データ構造の変更** (`dfs/common/src/auth/audit.rs`)
    *   `AuditRecord` に以下のフィールドを追加する。過去のログとの互換性のために `Option<String>` 型を採用。
        *   `previous_hash: Option<String>`
        *   `record_hash: Option<String>`
*   **ハッシュ計算と直列化** (`dfs/s3_server/src/audit.rs`)
    *   **フラッシュの直列化**: 複数スレッドによる非同期のDB書き込み (`tokio::spawn(spawn_blocking)`) を直列化（キューによる同期、またはシングルワーカー化）し、RocksDB への `Batch N` のコミットが完了してから `Batch N+1` を処理・書き込みするように修正する。
    *   **HMACの適用**: `hmac` と `sha2` クレートを使用。
    *   **起動時の初期化**: 最も新しい（最新のタイムスタンプを持つ）ログレコードを取得し、その `record_hash` を `last_hash` の初期値として復元する。
*   **検証ツールへの統合** (`dfs/s3_server/src/bin/audit_reader.rs`)
    *   `--verify-chain <SECRET>` フラグを実装し、指定したシークレットで全件のHMAC値を順次再計算し、チェーンが途切れていないかを検証する。

### 2.2 Resource-based Indexing & Column Families
`audit.rs` および `audit_reader.rs` を改修し、独立した Column Family (CF) によって役割（プライマリログ、ユーザーインデックス、リソースインデックス）を分割する。

*   **Column Familiesの定義**
    *   `default`: 使用しない（またはメタデータ用）
    *   `logs`: プライマリの監査ログ実体
    *   `idx_user`: ユーザーごとのセカンダリインデックス
    *   `idx_resource`: リソース（バケット/ARN）ごとのセカンダリインデックス
*   **書き込み処理の拡張 (RocksDB WriteBatch)**
    *   CFごとに設定したハンドル（Handle）に向かって Batch Write を行う。
    *   リソース用のキーフォーマット: `<bucket_name>:<ts_be>:<req_id>` （値はプライマリキー `:<ts_be>:<req_id>` へのポインタ）
*   **古いログのパージ (Cleanup Task)**
    *   `cleanup_old_logs` ではプライマリ CF（`logs`）から削除対象のキーを抽出し、事前に関連するセカンダリインデックス（`idx_user` および `idx_resource`）を削除したのちに、実体を `DeleteRange` 等で削除する構造へ変更する。
*   **CLIツールの対応**
    *   `audit_reader` の引数に `--resource <RESOURCE>` を追加し、CF `idx_resource` を走査して高速に履歴を抽出する最適化パスを設ける。

### 2.3 Audit Log Compression (ZSTD)
*   **設定の有効化** (`dfs/s3_server/src/audit.rs`)
    *   RocksDBを開く際の `Options` インスタンスにて `opts.set_compression_type(rocksdb::DBCompressionType::Zstd)` を指定し、各 Column Family に対してZSTD圧縮を有効にする。JSON形式の監査ログにおいて、LZ4よりも高い圧縮効果を発揮する。
    *   `dfs/s3_server/Cargo.toml` の `rocksdb` 依存関係で `zstd` フィーチャーを明示的に有効化する。

---

## 3. 実装のステップ

1.  **Phase 1: スキーマ、依存関係更新とCF/ZSTD設定**
    *   `AuditRecord` へのフィールド追加、`Cargo.toml` に `zstd` と `hmac` 依存追加。
    *   `audit.rs` でRocksDBの `ColumnFamilyDescriptor` を用いたCFオープンと圧縮設定。
    *   既存のコード（`flush_batch`, `cleanup_old_logs`, `audit_reader`）を新しい CF 構造に合わせるリファクタリング（マイグレーション）。
2.  **Phase 2: インデックスの拡充 (Resource)**
    *   CF `idx_resource` への書き込みとクリーンアップを追加。
    *   `audit_reader.rs` で `--resource` 検索を実装して動作確認。
3.  **Phase 3: ハッシュチェーン・HMACと順序保証の実装**
    *   バックグラウンドでの書き込み機構を「直列化されたシングルワーカー」に変更。
    *   `AuditLogger::new` 引数または環境変数経由で HMAC シークレットを受け取る。
    *   受信時に HMAC-SHA256 ハッシュをチェーン計算して `AuditRecord` を更新してからバッチ書き込みするようロジックを調整。
4.  **Phase 4: 検証ツールの実装と結合テスト**
    *   `audit_reader.rs` に `--verify-chain` オプションを設け、整合性を確認するスクリプトを整備。
5.  **Phase 5: 完了報告**
    *   全てビルド・テスト後、`TODO.md` のチェックボックスを更新する。
