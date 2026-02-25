use rustls_pemfile::certs;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use tonic::transport::{Certificate, ClientTlsConfig, Identity, ServerTlsConfig};

/// Load certificates from a PEM file
pub fn load_certs<P: AsRef<Path>>(path: P) -> anyhow::Result<Vec<Vec<u8>>> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut certs_vec = Vec::new();
    for cert in certs(&mut reader) {
        certs_vec.push(cert?.to_vec());
    }
    Ok(certs_vec)
}

/// Load a private key from a PEM file
pub fn load_private_key<P: AsRef<Path>>(path: P) -> anyhow::Result<Vec<u8>> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);

    // In rustls-pemfile v2, we use private_key() or similar.
    // Let's use the generic private_key() function.
    if let Some(key) = rustls_pemfile::private_key(&mut reader)? {
        return Ok(key.secret_der().to_vec());
    }

    Err(anyhow::anyhow!("No private keys found in PEM file"))
}

/// Create a Tonic ServerTlsConfig
pub fn get_server_tls_config(cert_path: &str, key_path: &str) -> anyhow::Result<ServerTlsConfig> {
    let cert = std::fs::read(cert_path)?;
    let key = std::fs::read(key_path)?;
    let identity = Identity::from_pem(cert, key);
    Ok(ServerTlsConfig::new().identity(identity))
}

/// Create a Tonic ClientTlsConfig
pub fn get_client_tls_config(
    ca_cert_path: &str,
    domain_name: &str,
) -> anyhow::Result<ClientTlsConfig> {
    let ca_cert = std::fs::read(ca_cert_path)?;
    let cert = Certificate::from_pem(ca_cert);
    Ok(ClientTlsConfig::new()
        .ca_certificate(cert)
        .domain_name(domain_name))
}

/// Create an Axum RustlsConfig
pub async fn get_axum_tls_config(
    cert_path: &str,
    key_path: &str,
) -> anyhow::Result<axum_server::tls_rustls::RustlsConfig> {
    axum_server::tls_rustls::RustlsConfig::from_pem_file(cert_path, key_path)
        .await
        .map_err(Into::into)
}

/// Helper to connect to a gRPC endpoint with optional TLS
pub async fn connect_endpoint(
    url: &str,
    ca_cert_path: Option<&str>,
    domain_name: Option<&str>,
) -> anyhow::Result<tonic::transport::Channel> {
    let has_ca = ca_cert_path.is_some();
    let addr_with_scheme = if !url.contains("://") {
        if has_ca {
            format!("https://{}", url)
        } else {
            format!("http://{}", url)
        }
    } else {
        url.to_string()
    };

    // Upgrade http to https if ca present
    let final_addr = if has_ca && addr_with_scheme.starts_with("http://") {
        addr_with_scheme.replace("http://", "https://")
    } else {
        addr_with_scheme
    };

    let mut endpoint = tonic::transport::Endpoint::from_shared(final_addr.clone())?;

    if let Some(ca_path) = ca_cert_path {
        let domain = domain_name.map(|s| s.to_string()).unwrap_or_else(|| {
            final_addr
                .split("://")
                .last()
                .unwrap_or("")
                .split(':')
                .next()
                .unwrap_or("localhost")
                .to_string()
        });

        let tls_config = get_client_tls_config(ca_path, &domain)?;
        endpoint = endpoint.tls_config(tls_config)?;
    }

    Ok(endpoint.connect().await?)
}
