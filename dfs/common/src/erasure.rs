use anyhow::{bail, Result};
use reed_solomon_erasure::galois_8::ReedSolomon;

/// Encodes `data` into `data_shards + parity_shards` equal-sized shards.
/// Returns Vec of (data_shards + parity_shards) byte vectors.
/// The last `parity_shards` entries are parity.
pub fn encode(data: &[u8], data_shards: usize, parity_shards: usize) -> Result<Vec<Vec<u8>>> {
    if data_shards == 0 || parity_shards == 0 {
        bail!("data_shards and parity_shards must both be > 0");
    }
    let r = ReedSolomon::new(data_shards, parity_shards)
        .map_err(|e| anyhow::anyhow!("RS init error: {:?}", e))?;
    let shard_size = shard_len(data.len(), data_shards);
    let mut padded = data.to_vec();
    padded.resize(shard_size * data_shards, 0);
    let mut shards: Vec<Vec<u8>> = padded
        .chunks(shard_size)
        .map(|c| c.to_vec())
        .collect();
    shards.resize(data_shards + parity_shards, vec![0u8; shard_size]);
    r.encode(&mut shards)
        .map_err(|e| anyhow::anyhow!("RS encode error: {:?}", e))?;
    Ok(shards)
}

/// Reconstructs original data from available shards.
/// `shards` has length `data_shards + parity_shards`.
/// Missing shards are represented as `None`.
/// Returns the original data (without padding), truncated to `original_len`.
pub fn decode(
    shards: &mut Vec<Option<Vec<u8>>>,
    data_shards: usize,
    parity_shards: usize,
    original_len: usize,
) -> Result<Vec<u8>> {
    let r = ReedSolomon::new(data_shards, parity_shards)
        .map_err(|e| anyhow::anyhow!("RS init error: {:?}", e))?;
    r.reconstruct(shards)
        .map_err(|e| anyhow::anyhow!("RS reconstruct error: {:?}", e))?;
    let mut result = Vec::new();
    for i in 0..data_shards {
        result.extend_from_slice(shards[i].as_ref().unwrap());
    }
    result.truncate(original_len);
    Ok(result)
}

/// Returns the byte length of each shard for a given total data length.
/// Pads up to the nearest multiple of data_shards.
pub fn shard_len(data_len: usize, data_shards: usize) -> usize {
    (data_len + data_shards - 1) / data_shards
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let data = b"Hello, Erasure Coding World!";
        let shards = encode(data, 4, 2).unwrap();
        assert_eq!(shards.len(), 6);
        let mut opt_shards: Vec<Option<Vec<u8>>> = shards.iter().map(|s| Some(s.clone())).collect();
        let recovered = decode(&mut opt_shards, 4, 2, data.len()).unwrap();
        assert_eq!(recovered, data);
    }

    #[test]
    fn test_decode_with_missing_shards() {
        let data = b"Hello, Erasure Coding World!";
        let shards = encode(data, 4, 2).unwrap();
        let mut opt_shards: Vec<Option<Vec<u8>>> = shards.iter().map(|s| Some(s.clone())).collect();
        opt_shards[1] = None;
        opt_shards[4] = None;
        let recovered = decode(&mut opt_shards, 4, 2, data.len()).unwrap();
        assert_eq!(recovered, data);
    }

    #[test]
    fn test_encode_large_data() {
        let data: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();
        let shards = encode(&data, 4, 2).unwrap();
        assert_eq!(shards.len(), 6);
        let mut opt_shards: Vec<Option<Vec<u8>>> = shards.iter().map(|s| Some(s.clone())).collect();
        let recovered = decode(&mut opt_shards, 4, 2, data.len()).unwrap();
        assert_eq!(recovered, data);
    }

    #[test]
    fn test_shard_len() {
        assert_eq!(shard_len(28, 4), 7);
        assert_eq!(shard_len(10_000, 4), 2500);
        assert_eq!(shard_len(1, 4), 1);  // ceil(1/4) = 1
    }
}
