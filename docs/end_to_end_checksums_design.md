# End-to-End Checksums 実装計画および設計

## 1. 目的と要件
`TODO.md` に記載されている `End-to-End Checksums` を実装し、クライアント (S3 Client -> S3 Server) から ChunkServer に至るすべての経路におけるデータの静的および動的破壊を防ぐための機構を構築します。

**主な要件**:
- `S3 Server (DFS Client)` におけるデータストリーム書き込み時のオブジェクト全体およびチャンクごとのチェックサム計算。
- 計算されたファイル全体およびブロック単位のチェックサムの `MetaServer` への保存と管理。
- S3プロトコルの標準である `ETag` (MD5) の提供、およびAWSの追加チェックサム機能への拡張可能構成。
- 読み出し時（ChunkServer -> S3 Server -> S3 Client）の整合性確認の実行。

## 2. 現状の構成と課題
1. **ChunkServer**: 既にローカルディスクへのブロック書き込み時に `crc32fast` を用いたCRC32Cを計算し、`.meta` ファイルに保存しています。また、バックグラウンドでの `Scrubber` タスクが機能し定期検証・自動修復 (`recover_block`) を行っています。しかし、現状では「S3 ServerからChunkServerへのネットワーク転送中の破壊」を検知することができません。
2. **S3 Server**: HTTPリクエストからストリームを読み出し、内容をもとにブロックとして切り出して ChunkServer に送っていますが、オブジェクト全体のハッシュ (MD5) やブロックのチェックサムは管理・計算していません。ダミーのETagを出力するコードが一部存在しますが、実際のMD5は計算されていません。`CopyObject` は常に空ファイルのMD5である `d41d8cd98f00b204e9800998ecf8427e` をハードコードして返しており、完全に不正です。
3. **MetaServer**: `FileMetadata` および `BlockInfo` にプロトコル定義として Checksum や ETag のフィールドが存在しないため、S3 API として正式な ETag を永続的に保持・返却できていません。`MasterCommand::CompleteFile` は `path` と `size` のみを持ち、ETag を受け取るシグネチャになっていません。また CompleteFile 内のブロックサイズは「均等割り近似」となっており、実際のブロック単位のチェックサム保存には不十分です。

## 3. 実装の設計・変更箇所

### Phase 1: プロトコル (Protocol) と メタデータ (MetaServer) の拡張 (`.meta` ファイル依存からの脱却)
`proto/dfs.proto` のスキーマを拡張し、チェックサム情報をRPC全体で引き回せるようにします。現在S3 Serverは `<key>.meta` ファイルを作成してETagを保存する代替手段を取っていますが、本フェーズによりMetaServerへネイティブ統合し、余分なI/Oを削減します。

*   **`dfs.proto` の修正**:
    *   `FileMetadata` メッセージに `string etag_md5 = 4;` と `uint64 created_at_ms = 5;`（ミリ秒エポック）を追加。
    *   `BlockInfo` メッセージに `uint32 checksum_crc32c = 4;` と `uint64 actual_size = 5;` を追加。
    *   `WriteBlockRequest` メッセージに `uint32 expected_checksum_crc32c = 4;` を追加。
    *   `ReplicateBlockRequest` メッセージに `uint32 expected_checksum_crc32c = 4;` を追加（ChunkServer間のレプリケーション経路での破損検知用）。
    *   `CompleteFileRequest` に `string etag_md5 = 4;`、`uint64 created_at_ms = 5;`（書き込み完了タイムスタンプ）を追加。また、ブロック単位の実サイズとチェックサムをまとめて伝えるため `repeated BlockChecksumInfo block_checksums = 6;` を追加し、`BlockChecksumInfo` メッセージ (`block_id`, `checksum_crc32c`, `actual_size`) を新設する。

*   **Raftログの後方互換性 (Breaking Change 回避)**:
    *   `MasterCommand::CompleteFile` に `etag_md5` と `block_checksums` を追加する際は、Raftログに記録された古いコマンドがデシリアライズ失敗しないよう、**`#[serde(default)]` + `Option<T>` を必須とする**。ローリングアップグレード時は旧バイナリが新フィールドを無視し、新バイナリは `None` の場合にデフォルト動作（チェックサムなし）にフォールバックすることで、クラスタの整合性を守る。
    ```rust
    CompleteFile {
        path: String,
        size: u64,
        #[serde(default)]
        etag_md5: Option<String>,
        #[serde(default)]
        block_checksums: Vec<BlockChecksumEntry>, // block_id -> crc32c, actual_size
    }
    ```

*   **ブロックサイズの確定方法の変更**:
    *   現在 `CompleteFile` 時に均等割りでブロックサイズを近似しているが、これでは正確なブロック単位のチェックサム保存ができない。DFSクライアントは各ブロックに書き込んだ実際のバイト数とCRC32Cを把握しているため、`CompleteFileRequest` にブロックごとの情報リスト (`block_checksums`) を含めて MetaServer へ送る設計に変更する。これにより均等割り近似を排除する。

*   **メタデータの永続化とメモリ管理 (`dfs/metaserver/src/`)**:
    *   `CompleteFile` 処理時に `etag_md5` と `block_checksums` をメタデータとして確定させ、Raft のログ/スナップショットに保存させる。
    *   `GetFileInfo` および `ListFiles` 等のレスポンス時にも `FileMetadata` へエンコードしてクライアントへ返すようにする。

### Phase 2: S3 Server (DFS Client) によるチェックサム計算とS3互換性向上
S3 Serverは、S3クライアントとChunkServerの間に立つ「Proxy 兼 DFSクライアント」として機能し、ストリーム・ハッシュ計算を担います。

*   **オブジェクト書き込み (`put_object`) および `dfs_client` への統合**:
    *   現在バッファーを全てオンメモリに展開している `dfs/client/src/mod.rs` (`create_file_from_buffer`) のレイヤーでブロック送信前に `crc32fast` を用いて CRC32C を計算させます。これにより、S3 Serverだけでなく他のDFSクライアントでもEnd-to-End Checksumの恩恵を受けられます。
    *   計算した `CRC32C` と実際のブロックサイズをセットで `WriteBlockRequest` の `expected_checksum_crc32c` として ChunkServer へ送信します。
    *   `S3 Server` では全体のMD5 (`ETag`) を計算し、各ブロックのCRC32C+サイズリストとともに `CompleteFileRequest` 経由でMetaServerに保存します。

*   **クライアントからの事前チェックサム検証 (`x-amz-checksum-*` 対応)**:
    *   AWS SDK がリクエストに付与する `x-amz-checksum-crc32c` などのヘッダーを読み取り、S3 Server がChunkServerへ送信する前にストリーム計算した結果と突き合わせます。不一致なら ChunkServer へ送る前に `400 Bad Request` で遮断し、広義のEnd-to-End (S3 Client -> DFS) を保証します。

*   **Multipart Upload (MPU) 計算の標準化（`upload_part` のパートMD5永続化含む）**:
    *   `upload_part`: 現在パートのETagは `d41d8cd98f00b204e9800998ecf8427e`（空ファイルのMD5）を固定返却しており、かつMD5値を永続化していない。パートデータを書き込む際に `md5` クレートで計算したMD5を `/.s3_mpu/<upload_id>/<part_number>.etag` という隠しファイルとしてDFSへ書き込む。このMD5をレスポンスの `ETag` ヘッダとしても返す。
    *   `complete_multipart_upload`: 各パートの `.etag` ファイルをパート番号順に読み取り、バイナリMD5を連結した上で全体MD5を再計算し、`-<パート数>` を末尾に付与するAWS互換のMPU ETagアルゴリズム (`md5(md5(p1)+md5(p2)...)-N`) を実装する。計算後に `.etag` ファイルをクリーンアップする。

*   **`CopyObject` における ETag・タイムスタンプ伝播**:
    *   現在 `CopyObject` は常に `d41d8cd98f00b204e9800998ecf8427e`（空ファイルのMD5）をハードコードして返しており完全に不正です。コピー元ファイルの `FileMetadata.etag_md5` をMetaServerから取得し、コピー先に対して `CompleteFileRequest` でその ETag をそのまま書き込む処理を追加します。`created_at_ms` は書き込み時点の現在時刻を新たに設定します（S3標準のコピー後タイムスタンプ更新動作と整合）。

*   **オブジェクト読み取り・一覧取得 (`GetObject`, `HeadObject`, `ListObjects`)**:
    *   MetaServerから取得した `FileMetadata` から `etag_md5` を読み出し、S3標準の `ETag` レスポンスヘッダとしてクライアントに返す。これにより AWS CLI 等での整合性チェックが正しく機能するようになる。
    *   `FileMetadata.created_at_ms` を `LastModified` フィールドとして ISO 8601 形式に変換してレスポンスへ含める。現在 `list_objects` / `list_objects_v2` の `last_modified` は `"2025-01-01T00:00:00.000Z"` にハードコードされており、`HeadObject` も同様に不正確。
    *   **N+1 RPC 問題（本スコープ外・注記）**: `ListObjects` は現在オブジェクトごとに `.meta` ファイルを個別 RPC で読んでいる (N+1問題)。本実装後もETagを `GetFileInfo` で1件ずつ取得する形になる。将来タスクとして `ListFilesRequest` レスポンスに `FileMetadata` ごと含める最適化を検討する。本スコープでは対処しない。

*   **S3 読み取り時の再検証 (In-transit Check)**:
    *   `GetObject` で ChunkServer からデータを読み出し、HTTPストリームに乗せる際、再度 CRC32C を計算して MetaServer が保持する `BlockInfo.checksum_crc32c` の期待値と突合する。
    *   ブロック読み込み中の破損が発覚した場合、直ちにエラー (`std::io::Error` / 接続切断) を発生させて破損データのクライアントへの流出を防ぐ。

### Phase 3: ChunkServer の検証強化 (End-to-End Validation)
*   **`WriteBlock` および `ReplicateBlock` ハンドラの改修 (`dfs/chunkserver/src/chunkserver.rs`)**:
    *   受信した `WriteBlockRequest` および `ReplicateBlockRequest` 内の `expected_checksum_crc32c` に対して、実際に受信した `data` 全体のCRC32C計算値を比較する（ネットワーク Ingress 時の中間者・インフライト破損検知）。
    *   不一致の場合は即座にエラー (gRPC `Status::data_loss` 等) を返し、書き込みを中止することで、ネットワーク転送中の破損データをディスクに書き込まない。
    *   **既存のScrubberとの役割分担の明確化**: 現在 ChunkServer が内部的に生成している 512バイト単位の `.meta` チェックサムは「ディスク上の Bit-Rot（静的破壊）検知用 (Scrubber用)」としてそのまま残し、今回追加するブロック全体の `expected_checksum_crc32c` は「ネットワーク転送時の In-flight 破損検知用」として役割を分離・共存される。
    *   `expected_checksum_crc32c` が `0`（未設定）の場合はチェックをスキップし、移行期間中（古いクライアントと共存するローリングアップグレード）においても正常に動作するようフォールバックする。

## 4. 移行ステップと実装順序
1. **スキーマ拡張**: `dfs.proto` に ETag・Checksum・`BlockChecksumInfo` 等のフィールドを追加し、自動生成される全クレートを再コンパイル。
2. **MetaServerの対応**: `MasterCommand::CompleteFile` に `#[serde(default)]` + `Option` でフィールドを追加し後方互換を保持。Raftのシリアライズ動作を確認しRolling Upgradeの正常動作を検証。
3. **ChunkServerの対応**: `WriteBlock` と `ReplicateBlock` 内での `expected_checksum` との照合関数を実装。`0`（未設定）の場合のフォールバックロジックを含める。
4. **DFS Client (`dfs_client`) の対応**: `create_file_from_buffer` 内でCRC32CとETagを計算し `WriteBlockRequest`、`ReplicateBlockRequest` 中継、および `CompleteFileRequest` に付与する。
5. **S3 Serverの対応**: `PutObject` でのMD5計算・ETagヘッダ返却、`CopyObject` でのETag伝播、`UploadPart` でのパートMD5保存、`CompleteMultipartUpload` でのMPU ETag計算を実装。
6. **結合テスト**: `aws s3 cp` → `aws s3api head-object` でETagが一致することをE2Eテストで確認。意図的に破損させたブロックがChunkServerで拒絶されることを確認。
