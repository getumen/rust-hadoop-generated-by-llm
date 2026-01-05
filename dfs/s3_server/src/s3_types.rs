use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename = "ListAllMyBucketsResult")]
pub struct ListAllMyBucketsResult {
    #[serde(rename = "Owner")]
    pub owner: Owner,
    #[serde(rename = "Buckets")]
    pub buckets: Buckets,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Owner {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "DisplayName")]
    pub display_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Buckets {
    #[serde(rename = "Bucket")]
    pub bucket: Vec<Bucket>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Bucket {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "CreationDate")]
    pub creation_date: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename = "ListBucketResult")]
pub struct ListBucketResult {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Prefix")]
    pub prefix: String,
    #[serde(rename = "Marker")]
    pub marker: String,
    #[serde(rename = "MaxKeys")]
    pub max_keys: i32,
    #[serde(rename = "IsTruncated")]
    pub is_truncated: bool,
    #[serde(rename = "Contents", default)]
    pub contents: Vec<Object>,
    #[serde(rename = "CommonPrefixes", default)]
    pub common_prefixes: Vec<CommonPrefix>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Object {
    #[serde(rename = "Key")]
    pub key: String,
    #[serde(rename = "LastModified")]
    pub last_modified: String,
    #[serde(rename = "ETag")]
    pub etag: String,
    #[serde(rename = "Size")]
    pub size: u64,
    #[serde(rename = "StorageClass")]
    pub storage_class: String,
    #[serde(rename = "Owner")]
    pub owner: Owner,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CommonPrefix {
    #[serde(rename = "Prefix")]
    pub prefix: String,
}

#[derive(Debug, Serialize)]
#[serde(rename = "Error")]
pub struct S3Error {
    #[serde(rename = "Code")]
    pub code: String,
    #[serde(rename = "Message")]
    pub message: String,
    #[serde(rename = "Resource")]
    pub resource: String,
    #[serde(rename = "RequestId")]
    pub request_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename = "InitiateMultipartUploadResult")]
pub struct InitiateMultipartUploadResult {
    #[serde(rename = "Bucket")]
    pub bucket: String,
    #[serde(rename = "Key")]
    pub key: String,
    #[serde(rename = "UploadId")]
    pub upload_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename = "CompleteMultipartUpload")]
pub struct CompleteMultipartUpload {
    #[serde(rename = "Part")]
    #[allow(dead_code)]
    pub parts: Vec<Part>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Part {
    #[serde(rename = "PartNumber")]
    pub part_number: i32,
    #[serde(rename = "ETag")]
    pub etag: String,
}

#[derive(Debug, Serialize)]
#[serde(rename = "CompleteMultipartUploadResult")]
pub struct CompleteMultipartUploadResult {
    #[serde(rename = "Location")]
    pub location: String,
    #[serde(rename = "Bucket")]
    pub bucket: String,
    #[serde(rename = "Key")]
    pub key: String,
    #[serde(rename = "ETag")]
    pub etag: String,
}

#[derive(Debug, Serialize)]
#[serde(rename = "CopyObjectResult")]
pub struct CopyObjectResult {
    #[serde(rename = "LastModified")]
    pub last_modified: String,
    #[serde(rename = "ETag")]
    pub etag: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename = "ListBucketResult")]
pub struct ListBucketResultV2 {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Prefix")]
    pub prefix: String,
    #[serde(rename = "MaxKeys")]
    pub max_keys: i32,
    #[serde(rename = "IsTruncated")]
    pub is_truncated: bool,
    #[serde(rename = "Contents")]
    pub contents: Vec<Object>,
    #[serde(rename = "CommonPrefixes", default)]
    pub common_prefixes: Vec<CommonPrefix>,
    #[serde(rename = "KeyCount")]
    pub key_count: i32,
    #[serde(rename = "ContinuationToken", skip_serializing_if = "Option::is_none")]
    pub continuation_token: Option<String>,
    #[serde(
        rename = "NextContinuationToken",
        skip_serializing_if = "Option::is_none"
    )]
    pub next_continuation_token: Option<String>,
    #[serde(rename = "StartAfter", skip_serializing_if = "Option::is_none")]
    pub start_after: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Metadata {
    pub headers: std::collections::HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename = "Delete")]
pub struct DeleteObjectsRequest {
    #[serde(rename = "Object")]
    pub objects: Vec<ObjectToDelete>,
    #[serde(rename = "Quiet", default)]
    #[allow(dead_code)]
    pub quiet: bool,
}

#[derive(Debug, Deserialize)]
pub struct ObjectToDelete {
    #[serde(rename = "Key")]
    pub key: String,
    #[serde(rename = "VersionId")]
    #[allow(dead_code)]
    pub version_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename = "DeleteResult")]
pub struct DeleteObjectsResult {
    #[serde(rename = "Deleted")]
    pub deleted: Vec<DeletedObject>,
    #[serde(rename = "Error")]
    pub errors: Vec<DeleteError>,
}

#[derive(Debug, Serialize)]
pub struct DeletedObject {
    #[serde(rename = "Key")]
    pub key: String,
}

#[derive(Debug, Serialize)]
pub struct DeleteError {
    #[serde(rename = "Key")]
    pub key: String,
    #[serde(rename = "Code")]
    pub code: String,
    #[serde(rename = "Message")]
    pub message: String,
}
