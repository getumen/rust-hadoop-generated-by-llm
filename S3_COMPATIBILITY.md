# S3 REST API 互換性

Rust Hadoop DFSは、標準的なS3ツールやエコシステム（AWS CLI, Spark, SDKs等）からアクセス可能にするためのS3互換APIゲートウェイを提供しています。

## 特徴

- **標準クライアント対応**: AWS CLI, `boto3`, Apache Spark (S3A) 等で利用可能。
- **メタデータ対応**: ユーザー定義のメタデータ（`x-amz-meta-*`）をサポート。
- **マルチパートアップロード**: 大容量ファイルの分割アップロードに対応。
- **ETag (MD5)**: オブジェクトの整合性検証のためのMD5 ETag計算をサポート。

## 起動方法

S3サーバーはデフォルトでポート `9000` で待機します。

```bash
# Docker Composeを使用する場合
docker compose up -d s3-server

# バイナリを直接実行する場合 (必要環境変数: MASTER_ADDR, SHARD_CONFIG)
MASTER_ADDR=http://localhost:50051 SHARD_CONFIG=shard_config.json ./target/release/s3-server
```

## エコシステムとの統合

### AWS CLI の設定

`--endpoint-url` を指定することで、ローカルのS3サーバーに対して操作を行えます。

```bash
# バケットの作成
aws --endpoint-url http://localhost:9000 s3 mb s3://my-bucket

# ファイルのアップロード
aws --endpoint-url http://localhost:9000 s3 cp file.txt s3://my-bucket/remote.txt

# ファイルの一覧表示
aws --endpoint-url http://localhost:9000 s3 ls s3://my-bucket/
```

### Apache Spark (S3A) の設定

Sparkで使用する場合、以下の設定（`spark-submit` 時に指定）を推奨します。

| プロパティ                       | 設定値                         | 説明                                     |
| :------------------------------- | :----------------------------- | :--------------------------------------- |
| `fs.s3a.endpoint`                | `http://<s3-server-host>:9000` | S3サーバーのエンドポイント               |
| `fs.s3a.path.style.access`       | `true`                         | パススタイルアクセスを強制               |
| `fs.s3a.payload.signing.enabled` | `false`                        | ペイロード署名を無効化（ETag不一致回避） |
| `fs.s3a.fast.upload`             | `true`                         | 高速アップロードを有効化                 |

**注意点**:
Spark (S3A) はデフォルトで S3 Chunked Encoding を使用し、リクエストボディの前に署名を付与します。
現在のS3サーバーの実装では、デコード後のボディではなく受信した生のボディに対してMD5を計算するため、`fs.s3a.payload.signing.enabled=false` を設定しないとクライアント側で計算したMD5（ETag）と不一致が発生します。

## パフォーマンス最適化

### Range Request の最適化

S3サーバーはHTTP Range Request（`Range: bytes=start-end`）を効率的に処理します。

**動作**:
1. Rangeヘッダーを検出
2. DFSクライアントの `read_file_range()` APIを使用して必要な範囲のみを取得
3. HTTP 206 Partial Content レスポンスを返却
4. `Content-Range` ヘッダーでバイト範囲と総サイズを通知

**利点**:
- **帯域幅削減**: 大容量ファイルの一部のみが必要な場合、全体をダウンロードしない
- **低レイテンシ**: 必要なデータのみをChunkServerから取得
- **Spark最適化**: Parquet/ORC形式での列指向アクセスを効率化

**例**:
```bash
# AWS CLIでのRange Request
aws --endpoint-url http://localhost:9000 s3api get-object \
  --bucket my-bucket \
  --key large-file.dat \
  --range bytes=1000-2000 \
  output.dat

# curlでのRange Request
curl -H "Range: bytes=0-1023" \
  http://localhost:9000/my-bucket/large-file.dat
```

内部的には、DFSの部分読み取り機能（Partial Block Reads）とLRUキャッシュを活用し、最小限のディスクI/Oで応答します。

## 実装されている主要なAPI

- **Bucket**: `CreateBucket`, `DeleteBucket`, `ListBuckets`, `HeadBucket`
- **Object**: `PutObject`, `GetObject`, `DeleteObject`, `HeadObject`, `CopyObject`
- **Multipart**: `CreateMultipartUpload`, `UploadPart`, `CompleteMultipartUpload`, `AbortMultipartUpload`
- **Batch**: `DeleteObjects` (Multi-Object Delete)
- **Pagination**: `ListObjectsV2` 対応
- **Range Requests**: HTTP Range ヘッダーによる部分取得（HTTP 206 Partial Content）

## 制限事項と今後の改善

- **認証**: 現在、アクセスキー/シークレットキーの検証を行わない「ダミー認証」となっています。任意の資格情報でアクセス可能です。
- **CopyObject**: 現在、サーバー側で一度ファイルを一時的に読み込み、再書き込みするナイーブな実装となっています。将来的にはメタデータ操作のみで完結する最適化を予定しています。
- **Presigned URLs**: 現在未実装です。Phase 5で実装予定です。
