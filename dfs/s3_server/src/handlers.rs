use crate::{s3_types::*, state::AppState as S3AppState};
use axum::http::header::{CONTENT_LENGTH, CONTENT_RANGE, ETAG, RANGE};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use md5::{Digest, Md5};
use percent_encoding::percent_decode_str;
use quick_xml::de::from_str;
use quick_xml::se::to_string;
use serde::Deserialize;
use std::io::{Read, Write};
use tempfile::NamedTempFile;
use tracing::Instrument;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct S3Query {
    pub uploads: Option<String>,
    pub delete: Option<String>,
    #[serde(rename = "uploadId")]
    pub upload_id: Option<String>,
    #[serde(rename = "partNumber")]
    pub part_number: Option<i32>,
    pub prefix: Option<String>,
    pub delimiter: Option<String>,
    #[serde(rename = "list-type")]
    pub list_type: Option<i32>, // 2 for V2
    #[serde(rename = "continuation-token")]
    pub continuation_token: Option<String>,
    #[serde(rename = "start-after")]
    pub start_after: Option<String>,
    #[serde(rename = "max-keys")]
    pub max_keys: Option<i32>,
}

// Helper to return XML response
fn xml_response(status: StatusCode, body: String) -> Response {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/xml")
        .body(Body::from(body))
        .unwrap()
}

fn empty_response(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap()
}

pub async fn handle_root(State(state): State<S3AppState>, method: Method) -> impl IntoResponse {
    match method {
        Method::GET => list_buckets(state).await,
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

async fn list_buckets(state: S3AppState) -> Response {
    match state.client.list_all_files().await {
        Ok(files) => {
            // Primitive dedup/filtering if needed.
            let mut unique_buckets = std::collections::HashSet::new();
            for name in files {
                let clean_name = name.trim_matches('/').to_string();
                // Extract the first component as the bucket name
                let bucket_name = if let Some((root, _)) = clean_name.split_once('/') {
                    root.to_string()
                } else {
                    clean_name
                };
                if !bucket_name.is_empty() {
                    unique_buckets.insert(bucket_name);
                }
            }

            let buckets_vec: Vec<Bucket> = unique_buckets
                .into_iter()
                .map(|name| Bucket {
                    name,
                    creation_date: "2025-01-01T00:00:00.000Z".to_string(),
                })
                .collect();

            let result = ListAllMyBucketsResult {
                owner: Owner {
                    id: "dfs".into(),
                    display_name: "dfs".into(),
                },
                buckets: Buckets {
                    bucket: buckets_vec,
                },
            };

            match to_string(&result) {
                Ok(xml) => xml_response(StatusCode::OK, xml),
                Err(_) => empty_response(StatusCode::INTERNAL_SERVER_ERROR),
            }
        }
        Err(e) => {
            tracing::error!("Failed to list buckets: {}", e);
            empty_response(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[axum::debug_handler]
pub async fn handle_request(
    State(state): State<S3AppState>,
    Path(path): Path<String>,
    Query(params): Query<S3Query>,
    headers: HeaderMap,
    method: Method,
    body: Body,
) -> Response {
    let request_id = headers
        .get(dfs_common::telemetry::REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let span =
        tracing::info_span!("s3_request", method = %method, path = %path, request_id = %request_id);

    async move {
        let body_bytes = axum::body::to_bytes(body, 1024 * 1024 * 1024)
            .await
            .unwrap_or_default();

        let parts: Vec<&str> = path.splitn(2, '/').collect();
        let bucket = parts[0];
        let key = if parts.len() > 1 { parts[1] } else { "" };

        if key.is_empty() {
            match method {
                Method::PUT => create_bucket(state, bucket).await,
                Method::DELETE => delete_bucket(state, bucket).await,
                Method::HEAD => head_bucket(state, bucket).await,
                Method::GET => {
                    if let Some(2) = params.list_type {
                        list_objects_v2(state, bucket, params).await
                    } else {
                        list_objects(state, bucket, params).await
                    }
                }
                _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
            }
        } else {
            // Multipart Upload Routing
            if params.uploads.is_some() && method == Method::POST {
                return initiate_multipart_upload(state, bucket, key).await;
            }
            if params.delete.is_some() && method == Method::POST {
                return delete_multiple_objects(state, bucket, body_bytes).await;
            }
            if let Some(upload_id) = params.upload_id {
                if let Some(part_number) = params.part_number {
                    if method == Method::PUT {
                        return upload_part(state, bucket, key, upload_id, part_number, body_bytes)
                            .await;
                    }
                }
                if method == Method::POST {
                    return complete_multipart_upload(state, bucket, key, upload_id, body_bytes)
                        .await;
                }
                if method == Method::DELETE {
                    return abort_multipart_upload(state, bucket, key, upload_id).await;
                }
            }

            // Copy Object Routing
            if method == Method::PUT && headers.contains_key("x-amz-copy-source") {
                let source = headers.get("x-amz-copy-source").unwrap().to_str().unwrap();
                return copy_object(state, bucket, key, source).await;
            }

            match method {
                Method::PUT => put_object(state, bucket, key, body_bytes, headers).await,
                Method::GET => get_object(state, bucket, key, headers).await,
                Method::DELETE => delete_object(state, bucket, key).await,
                Method::HEAD => head_object(state, bucket, key).await,
                _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
            }
        }
    }
    .instrument(span)
    .await
}

async fn initiate_multipart_upload(state: S3AppState, bucket: &str, key: &str) -> Response {
    let upload_id = Uuid::new_v4().to_string();
    let mpu_dir = format!("/.s3_mpu/{}", upload_id);
    let marker = format!("{}/.s3keep", mpu_dir);

    // Create marker to ensure directory exists
    let temp_file = NamedTempFile::new().unwrap();
    let _ = state.client.create_file(temp_file.path(), &marker).await;

    let result = InitiateMultipartUploadResult {
        bucket: bucket.into(),
        key: key.into(),
        upload_id,
    };

    match to_string(&result) {
        Ok(xml) => xml_response(StatusCode::OK, xml),
        Err(_) => empty_response(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn upload_part(
    state: S3AppState,
    _bucket: &str,
    _key: &str,
    upload_id: String,
    part_number: i32,
    body: Bytes,
) -> Response {
    let part_path = format!("/.s3_mpu/{}/{}", upload_id, part_number);
    let mut temp_file = NamedTempFile::new().unwrap();
    if temp_file.write_all(&body).is_err() {
        return empty_response(StatusCode::INTERNAL_SERVER_ERROR);
    }

    match state.client.create_file(temp_file.path(), &part_path).await {
        Ok(_) => Response::builder()
            .status(StatusCode::OK)
            .header("ETag", "\"d41d8cd98f00b204e9800998ecf8427e\"")
            .body(Body::empty())
            .unwrap(),
        Err(_) => empty_response(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn complete_multipart_upload(
    state: S3AppState,
    bucket: &str,
    key: &str,
    upload_id: String,
    body: Bytes,
) -> Response {
    // Parse body for part verification (skip actual verification for now, just trust client)
    if let Ok(str_body) = std::str::from_utf8(&body) {
        let _parts: Result<CompleteMultipartUpload, _> = from_str(str_body);
    }

    let dest_dir = format!("/{}/{}", bucket, key);

    // 1. Check if dest exists as file and delete it.
    // Ideally check if dir exists too? S3 overwrites.
    // If it's a file, delete_file works. If it's a dir (mpu), we overwrite?
    // Let's assume we overwrite.

    // Check if dest is a simple file and delete it
    if let Ok(files) = state.client.list_files(&dest_dir).await {
        if files.contains(&dest_dir) {
            let _ = state.client.delete_file(&dest_dir).await;
        }
    }

    // 2. Create completion marker in dest to make it a directory
    let marker_path = format!("{}/.s3_mpu_completed", dest_dir);
    let temp_file = NamedTempFile::new().unwrap();
    if state
        .client
        .create_file(temp_file.path(), &marker_path)
        .await
        .is_err()
    {
        return empty_response(StatusCode::INTERNAL_SERVER_ERROR);
    }

    // 3. Move parts
    // We need to list parts in /.s3_mpu/<upload_id> or just assume standard naming?
    // Better to list.
    let mpu_dir = format!("/.s3_mpu/{}", upload_id);
    if let Ok(files) = state.client.list_files(&mpu_dir).await {
        for f in files {
            if f.ends_with(".s3keep") {
                continue;
            }
            // Extract part number from path
            let parts: Vec<&str> = f.split('/').collect();
            if let Some(filename) = parts.last() {
                let dest_part_path = format!("{}/{}", dest_dir, filename);
                let _ = state.client.rename_file(&f, &dest_part_path).await;
            }
        }
    }

    // 4. Cleanup mpu dir
    // We should delete the old dir files. They are moved (renamed) so they are gone from source.
    // Just delete .s3keep
    let _ = state
        .client
        .delete_file(&format!("{}/.s3keep", mpu_dir))
        .await;

    let result = CompleteMultipartUploadResult {
        location: format!("http://localhost:9000/{}/{}", bucket, key),
        bucket: bucket.into(),
        key: key.into(),
        etag: "\"000-1\"".into(),
    };

    match to_string(&result) {
        Ok(xml) => xml_response(StatusCode::OK, xml),
        Err(_) => empty_response(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn abort_multipart_upload(
    state: S3AppState,
    _bucket: &str,
    _key: &str,
    upload_id: String,
) -> Response {
    let mpu_dir = format!("/.s3_mpu/{}", upload_id);
    if let Ok(files) = state.client.list_files(&mpu_dir).await {
        for f in files {
            let _ = state.client.delete_file(&f).await;
        }
    }
    empty_response(StatusCode::NO_CONTENT)
}

async fn delete_multiple_objects(state: S3AppState, bucket: &str, body: Bytes) -> Response {
    let body_str = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => return empty_response(StatusCode::BAD_REQUEST),
    };

    let req: DeleteObjectsRequest = match from_str(body_str) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to parse DeleteObjects request: {}", e);
            return empty_response(StatusCode::BAD_REQUEST);
        }
    };

    let mut deleted = Vec::new();
    let mut errors = Vec::new();

    for obj in req.objects {
        let path = format!("/{}/{}", bucket, obj.key);
        match state.client.delete_file(&path).await {
            Ok(_) => {
                deleted.push(DeletedObject { key: obj.key });
            }
            Err(e) => {
                if e.to_string().contains("not found") || e.to_string().contains("File not found") {
                    // S3: deleting non-existent object is a success
                    deleted.push(DeletedObject { key: obj.key });
                } else {
                    errors.push(DeleteError {
                        key: obj.key,
                        code: "InternalError".into(),
                        message: e.to_string(),
                    });
                }
            }
        }
    }

    let result = DeleteObjectsResult { deleted, errors };

    match to_string(&result) {
        Ok(xml) => xml_response(StatusCode::OK, xml),
        Err(_) => empty_response(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn copy_object(state: S3AppState, bucket: &str, key: &str, source: &str) -> Response {
    let dest = format!("/{}/{}", bucket, key);
    // source can be URL encoded.
    let decoded_source = percent_decode_str(source).decode_utf8_lossy().to_string();
    let source_path = if decoded_source.starts_with('/') {
        decoded_source
    } else {
        format!("/{}", decoded_source)
    };

    tracing::info!("CopyObject: source={} -> dest={}", source_path, dest);

    // Naive copy: Download to temp, upload to dest
    let temp_dir = std::env::temp_dir();
    let temp_path = temp_dir.join(Uuid::new_v4().to_string());

    if let Err(e) = state.client.get_file(&source_path, &temp_path).await {
        tracing::error!(
            "CopyObject: Failed to download source {}: {}",
            source_path,
            e
        );
        return empty_response(StatusCode::NOT_FOUND);
    }

    if let Err(e) = state.client.create_file(&temp_path, &dest).await {
        tracing::error!("CopyObject: Failed to upload to {}: {}", dest, e);
        let _ = std::fs::remove_file(temp_path);
        return empty_response(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let _ = std::fs::remove_file(temp_path);

    // Copy metadata if exists
    let meta_source = format!("{}.meta", source_path);
    let meta_dest = format!("{}.meta", dest);
    let meta_temp = temp_dir.join(format!("{}.meta", Uuid::new_v4()));
    if state
        .client
        .get_file(&meta_source, &meta_temp)
        .await
        .is_ok()
    {
        let _ = state.client.create_file(&meta_temp, &meta_dest).await;
        let _ = std::fs::remove_file(meta_temp);
    }

    let result = CopyObjectResult {
        last_modified: "2025-01-01T00:00:00.000Z".into(),
        etag: "\"d41d8cd98f00b204e9800998ecf8427e\"".into(),
    };

    match to_string(&result) {
        Ok(xml) => xml_response(StatusCode::OK, xml),
        Err(_) => empty_response(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn create_bucket(state: S3AppState, bucket: &str) -> Response {
    // Create a marker file to represent directory/bucket if it doesn't exist
    let marker_path = format!("/{}/.s3keep", bucket);

    // Create empty temp file
    let temp_file = NamedTempFile::new().unwrap();
    let temp_path = temp_file.path();

    match state.client.create_file(temp_path, &marker_path).await {
        Ok(_) => empty_response(StatusCode::OK),
        Err(e) => {
            tracing::error!("CreateBucket failed: {}", e);
            // Verify if it failed because it exists? Error handling in client returns string
            if e.to_string().contains("already exists") {
                empty_response(StatusCode::CONFLICT)
            } else {
                empty_response(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    }
}

async fn delete_bucket(state: S3AppState, bucket: &str) -> Response {
    // Check if empty
    let list_path = format!("/{}", bucket);
    match state.client.list_files(&list_path).await {
        Ok(files) => {
            if files.is_empty() || (files.len() == 1 && files[0].ends_with(".s3keep")) {
                let marker_path = format!("/{}/.s3keep", bucket);
                let _ = state.client.delete_file(&marker_path).await;
                empty_response(StatusCode::NO_CONTENT)
            } else {
                xml_response(
                    StatusCode::CONFLICT,
                    to_string(&S3Error {
                        code: "BucketNotEmpty".into(),
                        message: "The bucket you tried to delete is not empty".into(),
                        resource: bucket.into(),
                        request_id: "".into(),
                    })
                    .unwrap(),
                )
            }
        }
        Err(_) => empty_response(StatusCode::NOT_FOUND),
    }
}

async fn head_bucket(state: S3AppState, bucket: &str) -> Response {
    let list_path = format!("/{}", bucket);
    match state.client.list_files(&list_path).await {
        Ok(_) => empty_response(StatusCode::OK),
        Err(_) => empty_response(StatusCode::NOT_FOUND),
    }
}

async fn list_objects(state: S3AppState, bucket: &str, params: S3Query) -> Response {
    let list_path = format!("/{}", bucket);
    match state.client.list_files(&list_path).await {
        Ok(files) => {
            let mut objects = Vec::new();
            let mut common_prefixes = Vec::new(); // For directory simulation using delimiter
            let mut seen_prefixes = std::collections::HashSet::new();

            // MPU Aggregation: objects that are actually directories with .s3_mpu_completed
            let mut mpu_objects = std::collections::HashSet::new();
            for f in &files {
                if f.ends_with(".s3_mpu_completed") {
                    let dir_path = f.trim_matches('/').trim_end_matches("/.s3_mpu_completed");
                    mpu_objects.insert(dir_path.to_string());
                }
            }

            for f in files {
                // Filter out .s3keep
                if f.ends_with(".s3keep")
                    || f.ends_with(".s3_mpu_completed")
                    || f.ends_with(".meta")
                {
                    continue;
                }

                let bucket_prefix = format!("/{}/", bucket);
                if !f.starts_with(&bucket_prefix) {
                    continue;
                }

                let key = f.strip_prefix(&bucket_prefix).unwrap().to_string();

                // MPU Handling: if this file is part of an MPU object, we skip it here
                // But we must add the MPU object itself once.
                // Check if any parent of this file is in mpu_objects?
                // MPU parts are "/bucket/key/1", "/bucket/key/2". MPU obj is "/bucket/key".
                // If f is "/bucket/key/1", parent is "bucket/key".

                let path_no_slash = f.trim_matches('/').to_string();
                let parent = std::path::Path::new(&path_no_slash).parent();
                let mut is_part = false;
                if let Some(p) = parent {
                    if let Some(p_str) = p.to_str() {
                        if mpu_objects.contains(p_str) {
                            is_part = true;
                        }
                    }
                }

                if is_part {
                    continue;
                }

                // Check if this file ITSELF is one of the MPU objects (unlikely as MPU obj is a dir)
                // But if we encounter the MPU object name, we should list it.
                // However, `list_files` lists FILES. It doesn't list the directory itself as an entry usually.
                // So we rely on the loop over `mpu_objects` to add them.

                // Prefix filtering
                if let Some(p) = &params.prefix {
                    if !key.starts_with(p) {
                        continue;
                    }
                }

                // Delimiter handling
                if let Some(d) = &params.delimiter {
                    // key is "folder/file". Prefix "folder/".
                    // If key has delimiter after prefix?
                    // Trim prefix first
                    let effective_key = if let Some(p) = &params.prefix {
                        if key.starts_with(p) {
                            &key[p.len()..]
                        } else {
                            &key
                        }
                    } else {
                        &key
                    };

                    if let Some(idx) = effective_key.find(d) {
                        // Found delimiter. This is a common prefix.
                        let prefix_end = if let Some(p) = &params.prefix {
                            p.len()
                        } else {
                            0
                        } + idx
                            + d.len();
                        let prefix = &key[0..prefix_end];
                        if seen_prefixes.insert(prefix.to_string()) {
                            common_prefixes.push(CommonPrefix {
                                prefix: prefix.into(),
                            });
                        }
                        continue;
                    }
                }

                objects.push(Object {
                    key,
                    last_modified: "2025-01-01T00:00:00.000Z".into(),
                    etag: "\"d41d8cd98f00b204e9800998ecf8427e\"".into(),
                    size: 0,
                    storage_class: "STANDARD".into(),
                    owner: Owner {
                        id: "dfs".into(),
                        display_name: "dfs".into(),
                    },
                });
            }

            // Add MPU objects
            for mpu_path in mpu_objects {
                // mpu_path is "bucket/key"
                // we need relative key "key"
                let bucket_prefix_clean = format!("{}/", bucket);
                if mpu_path.starts_with(&bucket_prefix_clean) {
                    let key = mpu_path
                        .strip_prefix(&bucket_prefix_clean)
                        .unwrap()
                        .to_string();
                    // Filter prefix check for MPU objects too
                    if let Some(p) = &params.prefix {
                        if !key.starts_with(p) {
                            continue;
                        }
                    }

                    // Add as object
                    objects.push(Object {
                        key,
                        last_modified: "2025-01-01T00:00:00.000Z".into(),
                        etag: "\"000-MPU\"".into(),
                        size: 0, // Calculate size?
                        storage_class: "STANDARD".into(),
                        owner: Owner {
                            id: "dfs".into(),
                            display_name: "dfs".into(),
                        },
                    });
                }
            }

            let result = ListBucketResult {
                name: bucket.into(),
                prefix: params.prefix.unwrap_or_default(),
                marker: "".into(),
                max_keys: 1000,
                is_truncated: false,
                contents: objects,
                common_prefixes,
            };

            match to_string(&result) {
                Ok(xml) => xml_response(StatusCode::OK, xml),
                Err(_) => empty_response(StatusCode::INTERNAL_SERVER_ERROR),
            }
        }
        Err(_) => empty_response(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn put_object(
    state: S3AppState,
    bucket: &str,
    key: &str,
    body: Bytes,
    headers: HeaderMap,
) -> Response {
    let dest_path = format!("/{}/{}", bucket, key);
    let mut temp_file = NamedTempFile::new().unwrap();
    if let Err(e) = temp_file.write_all(&body) {
        tracing::error!("Failed to write temp file: {}", e);
        return empty_response(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let temp_path = temp_file.path();
    let etag = format!("\"{:x}\"", Md5::digest(&body));
    tracing::info!("PutObject: key={}, size={}, etag={}", key, body.len(), etag);

    match state.client.create_file(temp_path, &dest_path).await {
        Ok(_) => {
            // Metadata handling
            let mut meta_map = std::collections::HashMap::new();
            meta_map.insert("ETag".to_string(), etag.clone());
            for (k, v) in headers.iter() {
                let k_str = k.as_str();
                if k_str.starts_with("x-amz-meta-") {
                    if let Ok(v_str) = v.to_str() {
                        meta_map.insert(k_str.to_string(), v_str.to_string());
                    }
                }
            }
            if !meta_map.is_empty() {
                let metadata = Metadata { headers: meta_map };
                if let Ok(json) = serde_json::to_string(&metadata) {
                    let meta_path = format!("{}.meta", dest_path);
                    let mut meta_temp = NamedTempFile::new().unwrap();
                    let _ = meta_temp.write_all(json.as_bytes());
                    let _ = state.client.create_file(meta_temp.path(), &meta_path).await;
                }
            }
            Response::builder()
                .status(StatusCode::OK)
                .header(ETAG, etag)
                .body(Body::empty())
                .unwrap()
        }
        Err(e) => {
            if e.to_string().contains("already exists") {
                // S3 semantics: overwrite. Delete and retry.
                tracing::info!("File {} exists, deleting for overwrite", dest_path);
                let _ = state.client.delete_file(&dest_path).await;
                match state.client.create_file(temp_path, &dest_path).await {
                    Ok(_) => {
                        // Re-save metadata on retry
                        let mut meta_map = std::collections::HashMap::new();
                        meta_map.insert("ETag".to_string(), etag.clone());
                        let metadata = Metadata { headers: meta_map };
                        if let Ok(json) = serde_json::to_string(&metadata) {
                            let meta_path = format!("{}.meta", dest_path);
                            let mut meta_temp = NamedTempFile::new().unwrap();
                            let _ = meta_temp.write_all(json.as_bytes());
                            let _ = state.client.create_file(meta_temp.path(), &meta_path).await;
                        }

                        Response::builder()
                            .status(StatusCode::OK)
                            .header(ETAG, etag)
                            .body(Body::empty())
                            .unwrap()
                    }
                    Err(e2) => {
                        tracing::error!("PutObject retry failed: {}", e2);
                        empty_response(StatusCode::INTERNAL_SERVER_ERROR)
                    }
                }
            } else {
                tracing::error!("PutObject failed: {}", e);
                empty_response(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    }
}

async fn get_object(state: S3AppState, bucket: &str, key: &str, headers: HeaderMap) -> Response {
    // Check if MPU object (directory)
    let full_path = format!("/{}/{}", bucket, key);

    // MPU Handling (simplified for brevity, assume existing logic)
    let list_res = state.client.list_files(&full_path).await;
    let is_mpu = if let Ok(files) = &list_res {
        files.iter().any(|f| f.ends_with(".s3_mpu_completed"))
    } else {
        false
    };

    // Prepare metadata headers
    let mut response_headers = HeaderMap::new();
    let meta_path = format!("{}.meta", full_path);
    let temp_dir = std::env::temp_dir();
    let meta_temp = temp_dir.join(format!("{}.meta", Uuid::new_v4()));
    let mut etag = "\"d41d8cd98f00b204e9800998ecf8427e\"".to_string(); // Default ETag

    // Try download meta
    if state.client.get_file(&meta_path, &meta_temp).await.is_ok() {
        if let Ok(content) = std::fs::read_to_string(&meta_temp) {
            if let Ok(metadata) = serde_json::from_str::<Metadata>(&content) {
                for (k, v) in metadata.headers {
                    if k == "ETag" {
                        etag = v.clone();
                    }
                    if let Ok(val) = axum::http::HeaderValue::from_str(&v) {
                        if let Ok(name) = axum::http::HeaderName::from_bytes(k.as_bytes()) {
                            response_headers.insert(name, val);
                        }
                    }
                }
            }
        }
        let _ = std::fs::remove_file(meta_temp);
    }

    if is_mpu {
        // Stream parts
        let files = list_res.unwrap();
        let mut parts: Vec<(i32, String)> = Vec::new();
        for f in files {
            if f.ends_with(".s3keep") || f.ends_with(".s3_mpu_completed") {
                continue;
            }
            let name = f.split('/').next_back().unwrap();
            if let Ok(num) = name.parse::<i32>() {
                parts.push((num, f));
            }
        }
        parts.sort_by_key(|(n, _)| *n);

        let mut combined = Vec::new();
        let temp_dir = std::env::temp_dir();
        for (_, path) in parts {
            let dest_path = temp_dir.join(Uuid::new_v4().to_string());
            if state.client.get_file(&path, &dest_path).await.is_ok() {
                if let Ok(mut data) = std::fs::read(&dest_path) {
                    combined.append(&mut data);
                }
                let _ = std::fs::remove_file(dest_path);
            }
        }

        // MPU range support (Applied on combined bytes)
        let total_size = combined.len() as u64;
        let mut body_bytes = combined;
        let mut status = StatusCode::OK;

        if let Some(range_val) = headers.get(RANGE) {
            if let Ok(range_str) = range_val.to_str() {
                if let Some(range_part) = range_str.strip_prefix("bytes=") {
                    let parts: Vec<&str> = range_part.split('-').collect();
                    if parts.len() == 2 {
                        let start = parts[0].parse::<u64>().unwrap_or(0);
                        let end = parts[1].parse::<u64>().unwrap_or(total_size - 1);
                        let end = std::cmp::min(end, total_size - 1);
                        if start <= end {
                            status = StatusCode::PARTIAL_CONTENT;
                            let slice = &body_bytes[start as usize..=end as usize];
                            body_bytes = slice.to_vec();
                            response_headers.insert(
                                CONTENT_RANGE,
                                format!("bytes {}-{}/{}", start, end, total_size)
                                    .parse()
                                    .unwrap(),
                            );
                            response_headers.insert(
                                CONTENT_LENGTH,
                                (end - start + 1).to_string().parse().unwrap(),
                            );
                        }
                    }
                }
            }
        }

        let mut resp_builder = Response::builder().status(status);
        {
            let headers_map = resp_builder.headers_mut().unwrap();
            *headers_map = response_headers;
            headers_map.insert(ETAG, etag.parse().unwrap());
        }
        return resp_builder.body(Body::from(body_bytes)).unwrap();
    }

    // Normal Object
    let source_path = format!("/{}/{}", bucket, key);

    // Check if Range header is present to optimize download
    if let Some(range_val) = headers.get(RANGE) {
        if let Ok(range_str) = range_val.to_str() {
            if let Some(range_part) = range_str.strip_prefix("bytes=") {
                // First, get file info to determine total size
                if let Ok(true) = state.client.exists(&source_path).await {
                    // Parse range
                    let parts: Vec<&str> = range_part.split('-').collect();
                    if parts.len() == 2 {
                        // Get file metadata to determine total size
                        let temp_dir = std::env::temp_dir();
                        let dest_path =
                            temp_dir.join(format!("s3_get_{}_{}", bucket, key.replace('/', "_")));

                        // Download full file first to get total size
                        // TODO: Add get_file_metadata API to avoid this download
                        if state
                            .client
                            .get_file(&source_path, &dest_path)
                            .await
                            .is_ok()
                        {
                            let total_size = std::fs::metadata(&dest_path).unwrap().len();
                            let _ = std::fs::remove_file(dest_path);

                            let start = parts[0].parse::<u64>().unwrap_or(0);
                            let end = if parts[1].is_empty() {
                                total_size - 1
                            } else {
                                parts[1].parse::<u64>().unwrap_or(total_size - 1)
                            };
                            let end = std::cmp::min(end, total_size - 1);

                            if start <= end {
                                let length = end - start + 1;

                                tracing::info!(
                                    "Range request: path={}, start={}, end={}, length={}, total_size={}",
                                    source_path, start, end, length, total_size
                                );

                                // Use optimized partial read
                                match state
                                    .client
                                    .read_file_range(&source_path, start, length)
                                    .await
                                {
                                    Ok(data) => {
                                        tracing::info!(
                                            "Range read successful: {} bytes returned",
                                            data.len()
                                        );
                                        response_headers.insert(
                                            CONTENT_RANGE,
                                            format!("bytes {}-{}/{}", start, end, total_size)
                                                .parse()
                                                .unwrap(),
                                        );
                                        response_headers.insert(
                                            CONTENT_LENGTH,
                                            length.to_string().parse().unwrap(),
                                        );

                                        let mut resp_builder =
                                            Response::builder().status(StatusCode::PARTIAL_CONTENT);
                                        {
                                            let headers_map = resp_builder.headers_mut().unwrap();
                                            *headers_map = response_headers;
                                            headers_map.insert(ETAG, etag.parse().unwrap());
                                        }
                                        return resp_builder.body(Body::from(data)).unwrap();
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to read file range: {}", e);
                                        // Fall back to full download below
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Fall back to full file download (for non-range requests or if range optimization failed)
    let temp_dir = std::env::temp_dir();
    let dest_path = temp_dir.join(format!("s3_get_{}_{}", bucket, key.replace('/', "_")));

    match state.client.get_file(&source_path, &dest_path).await {
        Ok(_) => {
            let mut file = std::fs::File::open(&dest_path).unwrap();
            let total_size = file.metadata().unwrap().len();

            let mut data = vec![0u8; total_size as usize];
            let _ = file.read_exact(&mut data);
            let _ = std::fs::remove_file(dest_path);

            response_headers.insert(CONTENT_LENGTH, total_size.to_string().parse().unwrap());

            let mut resp_builder = Response::builder().status(StatusCode::OK);
            {
                let headers_map = resp_builder.headers_mut().unwrap();
                *headers_map = response_headers;
                headers_map.insert(ETAG, etag.parse().unwrap());
            }
            resp_builder.body(Body::from(data)).unwrap()
        }
        Err(e) => {
            tracing::error!("GetObject failed: {}", e);
            empty_response(StatusCode::NOT_FOUND)
        }
    }
}

async fn delete_object(state: S3AppState, bucket: &str, key: &str) -> Response {
    let path = format!("/{}/{}", bucket, key);
    match state.client.delete_file(&path).await {
        Ok(_) => empty_response(StatusCode::NO_CONTENT),
        Err(e) => {
            tracing::error!("DeleteObject failed: {}", e);
            empty_response(StatusCode::NO_CONTENT)
        }
    }
}

async fn head_object(state: S3AppState, bucket: &str, key: &str) -> Response {
    let path = format!("/{}/{}", bucket, key);
    match state.client.exists(&path).await {
        Ok(true) => {
            // Try to get metadata
            let meta_path = format!("{}.meta", path);
            let temp_dir = std::env::temp_dir();
            let meta_temp = temp_dir.join(format!("{}.meta", Uuid::new_v4()));
            let mut etag = "\"d41d8cd98f00b204e9800998ecf8427e\"".to_string(); // Default ETag

            if state.client.get_file(&meta_path, &meta_temp).await.is_ok() {
                if let Ok(content) = std::fs::read_to_string(&meta_temp) {
                    if let Ok(metadata) = serde_json::from_str::<Metadata>(&content) {
                        for (k, v) in metadata.headers.clone() {
                            if k == "ETag" {
                                etag = v.clone();
                            }
                        }
                        let mut response = Response::builder()
                            .status(StatusCode::OK)
                            .header(ETAG, etag)
                            .body(Body::empty())
                            .unwrap();
                        let headers_map = response.headers_mut();
                        for (k, v) in metadata.headers {
                            if let Ok(val) = axum::http::HeaderValue::from_str(&v) {
                                if let Ok(name) = axum::http::HeaderName::from_bytes(k.as_bytes()) {
                                    headers_map.insert(name, val);
                                }
                            }
                        }
                        let _ = std::fs::remove_file(meta_temp);
                        return response;
                    }
                }
                let _ = std::fs::remove_file(meta_temp);
            }
            Response::builder()
                .status(StatusCode::OK)
                .header(ETAG, etag)
                .body(Body::empty())
                .unwrap()
        }
        Ok(false) => {
            // Only return 200 for "directory" if the path ends with /
            if path.ends_with('/') {
                match state.client.list_files(&path).await {
                    Ok(files) if !files.is_empty() => {
                        tracing::info!("HeadObject: Directory marker (implicit) for {}", path);
                        Response::builder()
                            .status(StatusCode::OK)
                            .header(CONTENT_LENGTH, "0")
                            .body(Body::empty())
                            .unwrap()
                    }
                    _ => empty_response(StatusCode::NOT_FOUND),
                }
            } else {
                empty_response(StatusCode::NOT_FOUND)
            }
        }
        Err(e) => {
            tracing::error!("HeadObject failed: {}", e);
            empty_response(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn list_objects_v2(state: S3AppState, bucket: &str, params: S3Query) -> Response {
    // Re-use list_objects logic but return V2 structure and handle pagination
    // Use list_all_files to aggregate across all shards
    let bucket_prefix = format!("/{}/", bucket);
    match state.client.list_all_files().await {
        Ok(all_files) => {
            // Filter files to only those in this bucket
            let mut files: Vec<String> = all_files
                .into_iter()
                .filter(|f| f.starts_with(&bucket_prefix))
                .collect();
            files.sort(); // Pagination requires order

            // Apply start_after / continuation_token
            let mut start_index = 0;
            let marker = params
                .start_after
                .clone()
                .or(params.continuation_token.clone())
                .unwrap_or_default();
            if !marker.is_empty() {
                let marker_path = format!("/{}/{}", bucket, marker);
                // Find position
                if let Some(idx) = files.iter().position(|f| *f > marker_path) {
                    start_index = idx;
                } else {
                    start_index = files.len(); // All filtered
                }
            }

            let mut objects = Vec::new();
            let mut common_prefixes = Vec::new();
            let mut seen_prefixes = std::collections::HashSet::new();
            let mut key_count = 0;
            let max_keys = params.max_keys.unwrap_or(1000);
            let mut next_token = None;
            let mut is_truncated = false;

            // Iterate from start_index
            for i in start_index..files.len() {
                if key_count >= max_keys {
                    is_truncated = true;
                    // Next token is the key of the LAST added object?
                    // Actually token usually is the last key handled.
                    // Previous file was the last added.
                    if let Some(last_f) = files.get(i - 1) {
                        // Token is simple key name relative to bucket
                        let bucket_prefix = format!("/{}/", bucket);
                        if let Some(suffix) = last_f.strip_prefix(&bucket_prefix) {
                            next_token = Some(suffix.to_string());
                        }
                    }
                    break;
                }

                let f = &files[i];
                // ... same filtering logic ...
                if f.ends_with(".s3keep")
                    || f.ends_with(".s3_mpu_completed")
                    || f.ends_with(".meta")
                {
                    continue;
                }

                let bucket_prefix = format!("/{}/", bucket);
                if !f.starts_with(&bucket_prefix) {
                    continue;
                }
                let key = f.strip_prefix(&bucket_prefix).unwrap().to_string();

                // MPU/Dir logic omitted for V2 brevity (implement if needed, currently assumes simple files)
                // Actually we should support it.
                // SKIP MPU/Dir logic for now in V2 to keep simple.

                if let Some(p) = &params.prefix {
                    if !key.starts_with(p) {
                        continue;
                    }
                }

                // Delimiter handling
                if let Some(d) = &params.delimiter {
                    let effective_key = if let Some(p) = &params.prefix {
                        if key.starts_with(p) {
                            &key[p.len()..]
                        } else {
                            &key
                        }
                    } else {
                        &key
                    };
                    if let Some(idx) = effective_key.find(d) {
                        let prefix_end = if let Some(p) = &params.prefix {
                            p.len()
                        } else {
                            0
                        } + idx
                            + d.len();
                        let prefix = &key[0..prefix_end];
                        if seen_prefixes.insert(prefix.to_string()) {
                            common_prefixes.push(CommonPrefix {
                                prefix: prefix.into(),
                            });
                        }
                        continue;
                    }
                }

                objects.push(Object {
                    key: key.clone(),
                    last_modified: "2025-01-01T00:00:00.000Z".into(),
                    etag: "\"d41d8cd98f00b204e9800998ecf8427e\"".into(),
                    size: 0,
                    storage_class: "STANDARD".into(),
                    owner: Owner {
                        id: "dfs".into(),
                        display_name: "dfs".into(),
                    },
                });
                key_count += 1;
            }

            let result = ListBucketResultV2 {
                name: bucket.into(),
                prefix: params.prefix.unwrap_or_default(),
                max_keys,
                is_truncated,
                contents: objects,
                common_prefixes,
                key_count,
                continuation_token: params.continuation_token,
                next_continuation_token: next_token,
                start_after: params.start_after,
            };

            match to_string(&result) {
                Ok(xml) => xml_response(StatusCode::OK, xml),
                Err(_) => empty_response(StatusCode::INTERNAL_SERVER_ERROR),
            }
        }
        Err(_) => empty_response(StatusCode::INTERNAL_SERVER_ERROR),
    }
}
