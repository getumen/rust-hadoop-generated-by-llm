use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    pub timestamp: String,  // ISO8601 (e.g. 2024-03-13T10:00:00Z)
    pub timestamp_ms: u64,  // Epoch milliseconds for fast indexing/ordered keys
    pub request_id: String, // Request UUID
    pub remote_ip: String,
    pub user_id: String, // Access Key ID or OIDC Sub
    pub role_arn: Option<String>,
    pub action: String,             // S3 Action (e.g. s3:PutObject)
    pub resource: String,           // S3 Resource (ARN)
    pub status_code: u16,           // HTTP Status
    pub error_code: Option<String>, // S3 Error Code (e.g. AccessDenied)
    pub user_agent: Option<String>,
    pub duration_ms: Option<u64>, // Duration of the whole request in milliseconds
}
