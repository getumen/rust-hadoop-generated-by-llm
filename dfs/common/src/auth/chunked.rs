use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
type HmacSha256 = Hmac<Sha256>;

pub struct ChunkVerifier {
    signing_key: Vec<u8>,
    timestamp: String,
    scope: String,
    prev_signature: String,
}

impl ChunkVerifier {
    pub fn new(
        signing_key: Vec<u8>,
        timestamp: String,
        scope: String,
        seed_signature: String,
    ) -> Self {
        Self {
            signing_key,
            timestamp,
            scope,
            prev_signature: seed_signature,
        }
    }

    /// Verifies a chunk's signature and returns the data hash if valid.
    pub fn verify_chunk(&mut self, chunk_data: &[u8], expected_signature: &str) -> bool {
        let mut hasher = Sha256::new();
        hasher.update(chunk_data);
        let chunk_hash = hex::encode(hasher.finalize());

        let string_to_sign = format!(
            "AWS4-HMAC-SHA256-PAYLOAD\n{}\n{}\n{}\n{}\n{}",
            self.timestamp,
            self.scope,
            self.prev_signature,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", // hash of empty string for some reason in some specs?
            // AWS spec says: StringToSign = Algorithm + \n + DateTime + \n + Scope + \n + prev-signature + \n + hex(sha256(empty-string)) + \n + hex(sha256(chunk-data))
            chunk_hash
        );

        let mut mac = HmacSha256::new_from_slice(&self.signing_key).unwrap();
        mac.update(string_to_sign.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());

        use subtle::ConstantTimeEq;
        let sig_bytes = signature.as_bytes();
        let expected_bytes = expected_signature.as_bytes();

        if sig_bytes.len() == expected_bytes.len() && sig_bytes.ct_eq(expected_bytes).into() {
            self.prev_signature = signature;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ChunkVerifier;
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};

    type HmacSha256 = Hmac<Sha256>;

    const TIMESTAMP: &str = "20240101T000000Z";
    const SCOPE: &str = "20240101/us-east-1/s3/aws4_request";

    fn test_signing_key() -> Vec<u8> {
        b"test-signing-key".to_vec()
    }

    fn compute_expected_signature(
        signing_key: &[u8],
        timestamp: &str,
        scope: &str,
        prev_signature: &str,
        chunk_data: &[u8],
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(chunk_data);
        let chunk_hash = hex::encode(hasher.finalize());

        let string_to_sign = format!(
            "AWS4-HMAC-SHA256-PAYLOAD\n{}\n{}\n{}\n{}\n{}",
            timestamp,
            scope,
            prev_signature,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            chunk_hash
        );

        let mut mac = HmacSha256::new_from_slice(signing_key).unwrap();
        mac.update(string_to_sign.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn verify_valid_chunk_signature() {
        let signing_key = test_signing_key();
        let timestamp = TIMESTAMP.to_string();
        let scope = SCOPE.to_string();
        let seed_signature = "seed-signature".to_string();

        let mut verifier = ChunkVerifier::new(
            signing_key.clone(),
            timestamp.clone(),
            scope.clone(),
            seed_signature.clone(),
        );

        let chunk_data = b"example-chunk-1";

        let expected_signature = compute_expected_signature(
            &signing_key,
            &timestamp,
            &scope,
            &seed_signature,
            chunk_data,
        );

        assert!(
            verifier.verify_chunk(chunk_data, &expected_signature),
            "valid chunk signature should be accepted"
        );
    }

    #[test]
    fn verify_chained_chunk_signatures() {
        let signing_key = test_signing_key();
        let timestamp = TIMESTAMP.to_string();
        let scope = SCOPE.to_string();
        let seed_signature = "seed-signature".to_string();

        let mut verifier = ChunkVerifier::new(
            signing_key.clone(),
            timestamp.clone(),
            scope.clone(),
            seed_signature.clone(),
        );

        let chunk1 = b"chunk-1-data";
        let sig1 =
            compute_expected_signature(&signing_key, &timestamp, &scope, &seed_signature, chunk1);
        assert!(verifier.verify_chunk(chunk1, &sig1));

        let chunk2 = b"chunk-2-data";
        let sig2 = compute_expected_signature(&signing_key, &timestamp, &scope, &sig1, chunk2);
        assert!(verifier.verify_chunk(chunk2, &sig2));
    }
}
