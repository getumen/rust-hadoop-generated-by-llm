use crate::{s3_types::*, state::AppState as S3AppState};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use quick_xml::de::from_str;
use quick_xml::se::to_string;
use serde::Deserialize;
use std::io::Write;
use tempfile::NamedTempFile;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct S3Query {
    pub uploads: Option<String>,
    #[serde(rename = "uploadId")]
    pub upload_id: Option<String>,
    #[serde(rename = "partNumber")]
    pub part_number: Option<i32>,
    pub prefix: Option<String>,
    pub delimiter: Option<String>,
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
    match state.client.list_files("/").await {
        Ok(files) => {
            // Primitive dedup/filtering if needed.
            let buckets_vec: Vec<Bucket> = files
                .into_iter()
                .map(|name| {
                    // Remove trailing slash and leading slash if present (though client usually handles paths)
                    let clean_name = name.trim_matches('/').to_string();
                    Bucket {
                        name: clean_name,
                        creation_date: "2025-01-01T00:00:00.000Z".to_string(),
                    }
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
) -> impl IntoResponse {
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
            Method::GET => list_objects(state, bucket, params).await,
            _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
        }
    } else {
        // Multipart Upload Routing
        if params.uploads.is_some() && method == Method::POST {
            return initiate_multipart_upload(state, bucket, key).await;
        }
        if let Some(upload_id) = params.upload_id {
            if let Some(part_number) = params.part_number {
                if method == Method::PUT {
                    return upload_part(state, bucket, key, upload_id, part_number, body_bytes)
                        .await;
                }
            }
            if method == Method::POST {
                return complete_multipart_upload(state, bucket, key, upload_id, body_bytes).await;
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
            Method::PUT => put_object(state, bucket, key, body_bytes).await,
            Method::GET => get_object(state, bucket, key).await,
            Method::DELETE => delete_object(state, bucket, key).await,
            Method::HEAD => head_object(state, bucket, key).await,
            _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
        }
    }
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
            .header("ETag", "\"000\"")
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

async fn copy_object(state: S3AppState, bucket: &str, key: &str, source: &str) -> Response {
    // source format: "/bucket/key" or "bucket/key"
    let source = if source.starts_with('/') {
        source.to_string()
    } else {
        format!("/{}", source)
    };
    let dest = format!("/{}/{}", bucket, key);

    // Naive copy: Download to temp, upload to dest
    let temp_dir = std::env::temp_dir();
    let temp_path = temp_dir.join(Uuid::new_v4().to_string());

    if state.client.get_file(&source, &temp_path).await.is_err() {
        return empty_response(StatusCode::NOT_FOUND);
    }

    if state.client.create_file(&temp_path, &dest).await.is_err() {
        let _ = std::fs::remove_file(temp_path);
        return empty_response(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let _ = std::fs::remove_file(temp_path);

    let result = CopyObjectResult {
        last_modified: "2025-01-01T00:00:00.000Z".into(),
        etag: "\"000\"".into(),
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
                if f.ends_with(".s3keep") || f.ends_with(".s3_mpu_completed") {
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
                    etag: "\"000\"".into(),
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

async fn put_object(state: S3AppState, bucket: &str, key: &str, body: Bytes) -> Response {
    let dest_path = format!("/{}/{}", bucket, key);
    let mut temp_file = NamedTempFile::new().unwrap();
    if let Err(e) = temp_file.write_all(&body) {
        tracing::error!("Failed to write temp file: {}", e);
        return empty_response(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let temp_path = temp_file.path();
    match state.client.create_file(temp_path, &dest_path).await {
        Ok(_) => empty_response(StatusCode::OK),
        Err(e) => {
            tracing::error!("PutObject failed: {}", e);
            empty_response(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_object(state: S3AppState, bucket: &str, key: &str) -> Response {
    // Check if MPU object (directory)
    let full_path = format!("/{}/{}", bucket, key);

    // We can't easily check 'exists' without listing or get_file_info (which we implemented in client but s3 server uses client struct which doesn't expose it yet? check client/mod.rs)
    // Client::get_file_info was private in previous context, but `get_file` uses it.
    // Wait, `Client::list_files` is available.

    // Check if marker exists in the directory (if it is a directory)
    // list_files returns list of files inside if it's a dir?
    // list_files on master returns all keys that START with path.
    // So if I list "/bucket/key", I get "/bucket/key/.s3_mpu_completed", "/bucket/key/1", etc.

    let list_res = state.client.list_files(&full_path).await;
    let is_mpu = if let Ok(files) = &list_res {
        files.iter().any(|f| f.ends_with(".s3_mpu_completed"))
    } else {
        false
    };

    if is_mpu {
        // Stream parts
        // Parts are in `full_path`. Sort by numeric filename.
        let files = list_res.unwrap();
        // Filter parts: just digits
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

        // Combine contents
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

        return Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(combined))
            .unwrap();
    }

    let source_path = format!("/{}/{}", bucket, key);

    let temp_dir = std::env::temp_dir();
    let dest_path = temp_dir.join(format!("s3_get_{}_{}", bucket, key.replace('/', "_")));

    match state.client.get_file(&source_path, &dest_path).await {
        Ok(_) => match std::fs::read(&dest_path) {
            Ok(data) => {
                let _ = std::fs::remove_file(dest_path);
                Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::from(data))
                    .unwrap()
            }
            Err(_) => empty_response(StatusCode::INTERNAL_SERVER_ERROR),
        },
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
    // I will stick to what's available.
    // Wait, delete_file returns error if not found?
    // Actually, I can use a simpler trick: use `list_files` and check if key exists.
    // Since `list_files` currently returns ALL files (as analyzed above), I can iterate.
    // Not efficient but works for now.

    let path = format!("/{}/{}", bucket, key);
    match state.client.list_files("/").await {
        Ok(files) => {
            if files.contains(&path) {
                empty_response(StatusCode::OK)
            } else {
                empty_response(StatusCode::NOT_FOUND)
            }
        }
        Err(_) => empty_response(StatusCode::INTERNAL_SERVER_ERROR),
    }
}
