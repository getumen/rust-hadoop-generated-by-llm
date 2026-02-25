use tonic::metadata::MetadataValue;
use tonic::{Request, Status};
use uuid::Uuid;

pub mod telemetry {
    use super::*;

    pub const REQUEST_ID_HEADER: &str = "x-request-id";

    /// Client-side interceptor to inject or generate a request ID
    #[allow(clippy::result_large_err)]
    pub fn tracing_interceptor(mut req: Request<()>) -> Result<Request<()>, Status> {
        let request_id = match req.metadata().get(REQUEST_ID_HEADER) {
            Some(id) => id.clone(),
            None => {
                let new_id = Uuid::new_v4().to_string();
                MetadataValue::try_from(new_id).unwrap_or_else(|_| MetadataValue::from_static(""))
            }
        };
        req.metadata_mut().insert(REQUEST_ID_HEADER, request_id);
        Ok(req)
    }

    /// Extractor to get request ID from gRPC request
    pub fn get_request_id<T>(req: &Request<T>) -> String {
        req.metadata()
            .get(REQUEST_ID_HEADER)
            .and_then(|id| id.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }

    /// Create a tracing span with request ID the server-side
    pub fn create_server_span<T>(req: &Request<T>, name: &'static str) -> tracing::Span {
        let request_id = get_request_id(req);
        tracing::info_span!("server_rpc", rpc = %name, request_id = %request_id)
    }

    /// Interceptor to propagate a SPECIFIC request ID (used in replication)
    #[allow(clippy::result_large_err)]
    pub fn propagation_interceptor(
        request_id: String,
    ) -> impl Fn(Request<()>) -> Result<Request<()>, Status> {
        move |mut req: Request<()>| {
            if let Ok(id) = MetadataValue::try_from(request_id.clone()) {
                req.metadata_mut().insert(REQUEST_ID_HEADER, id);
            }
            Ok(req)
        }
    }
}

pub mod security;
pub mod sharding;
