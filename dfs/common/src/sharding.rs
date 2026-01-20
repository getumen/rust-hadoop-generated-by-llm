use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

/// ShardId is a unique identifier for a Raft Group (Shard).
pub type ShardId = String;

/// Helper to compute hash of a string key using CRC32 for deterministic behavior.
fn hash_key(key: &str) -> u64 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(key.as_bytes());
    hasher.finalize() as u64
}

/// ShardingStrategy defines how keys are mapped to Shards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ShardingStrategy {
    /// Consistent Hashing (current static implementation)
    ConsistentHash {
        /// The hash ring mapping hash values to ShardIds.
        ring: BTreeMap<u64, ShardId>,
        /// Number of virtual nodes per physical shard.
        virtual_nodes: usize,
    },
    /// Range-based Partitioning (S3/Colossus style)
    Range {
        /// Map of range-end (exclusive) to ShardId.
        /// The last entry typically has an empty string or a very high value as the key.
        /// We use lexicographical order for prefix locality.
        ranges: BTreeMap<String, ShardId>,
    },
}

/// ShardMap manages the mapping between keys and Shards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardMap {
    /// The sharding strategy being used.
    pub strategy: ShardingStrategy,
    /// Set of active ShardIds.
    shards: HashSet<ShardId>,
    /// Map of ShardId to list of peer addresses.
    shard_peers: HashMap<ShardId, Vec<String>>,
}

impl ShardMap {
    /// Create a new ShardMap using Consistent Hashing.
    pub fn new_consistent_hash(virtual_nodes: usize) -> Self {
        Self {
            strategy: ShardingStrategy::ConsistentHash {
                ring: BTreeMap::new(),
                virtual_nodes,
            },
            shards: HashSet::new(),
            shard_peers: HashMap::new(),
        }
    }

    /// Create a new ShardMap using Range-based Partitioning.
    pub fn new_range() -> Self {
        Self {
            strategy: ShardingStrategy::Range {
                ranges: BTreeMap::new(),
            },
            shards: HashSet::new(),
            shard_peers: HashMap::new(),
        }
    }

    /// Add a new Shard to the map with its peers.
    pub fn add_shard(&mut self, shard_id: ShardId, peers: Vec<String>) {
        if self.shards.contains(&shard_id) {
            // Update peers if already exists? For now, we update.
            self.shard_peers.insert(shard_id.clone(), peers.clone());
            return;
        }
        self.shards.insert(shard_id.clone());
        self.shard_peers.insert(shard_id.clone(), peers.clone());

        match &mut self.strategy {
            ShardingStrategy::ConsistentHash {
                ring,
                virtual_nodes,
            } => {
                for i in 0..*virtual_nodes {
                    let hash = hash_key(&format!("{}:{}", shard_id, i));
                    ring.insert(hash, shard_id.clone());
                }
            }
            ShardingStrategy::Range { ranges } => {
                // For a new range shard, we need a split point.
                // If it's the first shard, it covers everything.
                if ranges.is_empty() {
                    ranges.insert("\u{10FFFF}".to_string(), shard_id);
                } else if ranges.len() == 1 {
                    // If we have one shard covering everything, split it at '/m' to provide some balance
                    // Move the old range end to a mid-point
                    let old_shard = ranges.values().next().unwrap().clone();
                    ranges.clear();
                    ranges.insert("/m".to_string(), shard_id);
                    ranges.insert("\u{10FFFF}".to_string(), old_shard);
                } else {
                    // For subsequent shards, just append for now
                    ranges.insert(format!("z-{}", shard_id), shard_id);
                }
            }
        }
    }

    pub fn has_shard(&self, shard_id: &ShardId) -> bool {
        self.shards.contains(shard_id)
    }

    pub fn get_peers(&self, shard_id: &ShardId) -> Option<Vec<String>> {
        self.shard_peers.get(shard_id).cloned()
    }

    /// Remove a Shard from the map.
    pub fn remove_shard(&mut self, shard_id: &ShardId) {
        if !self.shards.contains(shard_id) {
            return;
        }
        self.shards.remove(shard_id);
        self.shard_peers.remove(shard_id);

        match &mut self.strategy {
            ShardingStrategy::ConsistentHash { ring, .. } => {
                let keys_to_remove: Vec<u64> = ring
                    .iter()
                    .filter(|(_, sid)| *sid == shard_id)
                    .map(|(k, _)| *k)
                    .collect();

                for k in keys_to_remove {
                    ring.remove(&k);
                }
            }
            ShardingStrategy::Range { ranges } => {
                let keys_to_remove: Vec<String> = ranges
                    .iter()
                    .filter(|(_, sid)| *sid == shard_id)
                    .map(|(k, _)| k.clone())
                    .collect();

                for k in keys_to_remove {
                    ranges.remove(&k);
                }
            }
        }
    }

    /// Get the Shard responsible for the given key (e.g., file path).
    pub fn get_shard(&self, key: &str) -> Option<ShardId> {
        match &self.strategy {
            ShardingStrategy::ConsistentHash { ring, .. } => {
                if ring.is_empty() {
                    return None;
                }
                let hash = hash_key(key);
                let entry = ring.range(hash..).next().or_else(|| ring.iter().next());
                entry.map(|(_, shard_id)| shard_id.clone())
            }
            ShardingStrategy::Range { ranges } => {
                if ranges.is_empty() {
                    return None;
                }
                // Find the first entry with a key (end-range) > key
                // Since we use exclusive end, we want the first range-end that is >= key
                let entry = ranges.range(key.to_string()..).next();
                entry.map(|(_, shard_id)| shard_id.clone())
            }
        }
    }

    /// Add a split point for Range sharding.
    pub fn split_shard(
        &mut self,
        split_key: String,
        new_shard_id: ShardId,
        peers: Vec<String>,
    ) -> bool {
        if let ShardingStrategy::Range { ranges } = &mut self.strategy {
            if self.shards.contains(&new_shard_id) {
                return false;
            }

            // Find the shard that currently contains the split_key
            if let Some(old_shard_end) = ranges
                .range(split_key.clone()..)
                .next()
                .map(|(k, _)| k.clone())
            {
                let _old_shard_id = ranges.get(&old_shard_end).unwrap().clone();

                // Insert the new split point.
                // The new shard will cover [previous_end, split_key)
                // The old shard will cover [split_key, old_shard_end)
                // Actually, if we want [ ..., split_key) -> new_shard, we insert (split_key, new_shard)
                ranges.insert(split_key, new_shard_id.clone());
                self.shards.insert(new_shard_id.clone());
                self.shard_peers.insert(new_shard_id, peers);
                return true;
            }
        }
        false
    }

    /// Get all registered shards
    pub fn get_all_shards(&self) -> Vec<ShardId> {
        self.shards.iter().cloned().collect()
    }

    /// Get peers for a specific shard.
    pub fn get_shard_peers(&self, shard_id: &ShardId) -> Option<Vec<String>> {
        self.shard_peers.get(shard_id).cloned()
    }

    /// Get all master addresses across all shards.
    pub fn get_all_masters(&self) -> Vec<String> {
        let mut masters = HashSet::new();
        for peers in self.shard_peers.values() {
            for peer in peers {
                masters.insert(peer.clone());
            }
        }
        masters.into_iter().collect()
    }

    /// Create a new ShardMap with a default configuration (Consistent Hashing for backward compatibility).
    pub fn new(virtual_nodes: usize) -> Self {
        Self::new_consistent_hash(virtual_nodes)
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct ShardConfig {
    pub shards: HashMap<ShardId, Vec<String>>,
}

impl ShardConfig {
    pub fn from_file(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: ShardConfig = serde_json::from_str(&content)?;
        Ok(config)
    }

    pub fn to_shard_map(&self, _virtual_nodes: usize) -> ShardMap {
        let mut map = ShardMap::new_range();
        // Sort shard IDs for deterministic ordering
        let mut sorted_shards: Vec<_> = self.shards.iter().collect();
        sorted_shards.sort_by_key(|(shard_id, _)| *shard_id);
        for (shard_id, peers) in sorted_shards {
            map.add_shard(shard_id.clone(), peers.clone());
        }
        map
    }
}

pub fn load_shard_map_from_config(path: Option<&str>, virtual_nodes: usize) -> ShardMap {
    if let Some(p) = path {
        match ShardConfig::from_file(p) {
            Ok(config) => {
                println!("Loaded shard config from {}", p);
                return config.to_shard_map(virtual_nodes);
            }
            Err(e) => {
                eprintln!("Failed to load shard config: {}. Using empty map.", e);
            }
        }
    }
    ShardMap::new(virtual_nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_add_get_shard() {
        let mut map = ShardMap::new(10);
        map.add_shard("shard-1".to_string(), vec![]);
        map.add_shard("shard-2".to_string(), vec![]);

        let shard = map.get_shard("/user/data/file1.txt");
        assert!(shard.is_some());
        let s = shard.unwrap();
        assert!(s == "shard-1" || s == "shard-2");
    }

    #[test]
    fn test_remove_shard() {
        let mut map = ShardMap::new(10);
        map.add_shard("shard-1".to_string(), vec![]);
        map.add_shard("shard-2".to_string(), vec![]);

        // Find a key that maps to shard-1
        let mut key_for_shard1 = String::new();
        for i in 0..1000 {
            let key = format!("key-{}", i);
            if map.get_shard(&key).unwrap() == "shard-1" {
                key_for_shard1 = key;
                break;
            }
        }
        assert!(!key_for_shard1.is_empty());

        // Remove shard-1
        map.remove_shard(&"shard-1".to_string());

        // The key should now map to shard-2 (since it's the only one left)
        assert_eq!(map.get_shard(&key_for_shard1).unwrap(), "shard-2");
    }

    #[test]
    fn test_empty_map() {
        let map = ShardMap::new(10);
        assert!(map.get_shard("any-key").is_none());
        assert!(map.get_shard_peers(&"any-shard".to_string()).is_none());
    }

    #[test]
    fn test_shard_config_parsing() {
        let json = r#"
        {
            "shards": {
                "shard-1": ["addr1", "addr2"],
                "shard-2": ["addr3"]
            }
        }
        "#;
        let config: ShardConfig = serde_json::from_str(json).expect("Failed to parse JSON");
        assert_eq!(config.shards.len(), 2);
        assert_eq!(config.shards.get("shard-1").unwrap().len(), 2);
        assert_eq!(config.shards.get("shard-2").unwrap().len(), 1);

        let map = config.to_shard_map(10);
        assert!(map.get_all_shards().contains(&"shard-1".to_string()));
        assert!(map.get_all_shards().contains(&"shard-2".to_string()));
        assert_eq!(
            map.get_shard_peers(&"shard-1".to_string()).unwrap(),
            vec!["addr1", "addr2"]
        );
    }

    #[test]
    fn test_consistent_hashing_stability() {
        let mut map = ShardMap::new_consistent_hash(100);
        map.add_shard("shard-A".to_string(), vec![]);
        map.add_shard("shard-B".to_string(), vec![]);

        let key = "test-file.txt";
        let shard1 = map.get_shard(key).unwrap();

        // Same map, same key -> same result
        assert_eq!(shard1, map.get_shard(key).unwrap());

        // New map, same configuration -> same result
        let mut map2 = ShardMap::new_consistent_hash(100);
        map2.add_shard("shard-A".to_string(), vec![]);
        map2.add_shard("shard-B".to_string(), vec![]);

        assert_eq!(shard1, map2.get_shard(key).unwrap());
    }

    #[test]
    fn test_range_sharding() {
        let mut map = ShardMap::new_range();
        // Initial shard covers everything up to max char
        map.add_shard("shard-0".to_string(), vec![]);

        // Split at "/m" -> shard-1 takes ["", "/m"), shard-0 remains as ["/m", max)
        map.split_shard("/m".to_string(), "shard-1".to_string(), vec![]);

        // Split at "/t" -> shard-2 takes ["/m", "/t"), shard-0 remains as ["/t", max)
        map.split_shard("/t".to_string(), "shard-2".to_string(), vec![]);

        assert_eq!(map.get_shard("/apple").unwrap(), "shard-1");
        assert_eq!(map.get_shard("/banana").unwrap(), "shard-1");
        assert_eq!(map.get_shard("/mango").unwrap(), "shard-2");
        assert_eq!(map.get_shard("/orange").unwrap(), "shard-2");
        assert_eq!(map.get_shard("/zebra").unwrap(), "shard-0");
    }
}
