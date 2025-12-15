use crate::{s3_types::*, state::AppState as S3AppState};
use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use quick_xml::se::to_string;
use std::io::Write;
use tempfile::NamedTempFile;

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
    req: Request,
) -> impl IntoResponse {
    let (parts, body) = req.into_parts();
    let method = parts.method;
    // Limit body size if needed, here MAX
    let body = axum::body::to_bytes(body, 1024 * 1024 * 1024)
        .await
        .unwrap_or_default(); // 1GB limit?

    // path: "bucket" or "bucket/key/subkey"

    let parts: Vec<&str> = path.splitn(2, '/').collect();
    let bucket = parts[0];
    let key = if parts.len() > 1 { parts[1] } else { "" };

    if key.is_empty() {
        match method {
            Method::PUT => create_bucket(state, bucket).await,
            Method::DELETE => delete_bucket(state, bucket).await,
            Method::HEAD => head_bucket(state, bucket).await,
            Method::GET => list_objects(state, bucket).await,
            _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
        }
    } else {
        match method {
            Method::PUT => put_object(state, bucket, key, body).await,
            Method::GET => get_object(state, bucket, key).await,
            Method::DELETE => delete_object(state, bucket, key).await,
            Method::HEAD => head_object(state, bucket, key).await,
            _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
        }
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

async fn list_objects(state: S3AppState, bucket: &str) -> Response {
    let list_path = format!("/{}", bucket);
    match state.client.list_files(&list_path).await {
        Ok(files) => {
            let mut objects = Vec::new();
            for f in files {
                if f.ends_with(".s3keep") {
                    continue;
                }
                let bucket_prefix = format!("/{}/", bucket);
                if f.starts_with(&bucket_prefix) {
                    let key = f.strip_prefix(&bucket_prefix).unwrap().to_string();
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
            }
            let result = ListBucketResult {
                name: bucket.into(),
                prefix: "".into(),
                marker: "".into(),
                max_keys: 1000,
                is_truncated: false,
                contents: objects,
                common_prefixes: vec![],
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
