use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};

/// ShardId is a unique identifier for a Raft Group (Shard).
pub type ShardId = String;

use serde::{Deserialize, Serialize};

/// ShardMap manages the mapping between keys and Shards using Consistent Hashing.
/// It uses Virtual Nodes to ensure even distribution of load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardMap {
    /// The hash ring mapping hash values to ShardIds.
    /// We use BTreeMap to keep keys sorted for efficient lookup.
    ring: BTreeMap<u64, ShardId>,
    /// Set of active ShardIds.
    shards: HashSet<ShardId>,
    /// Map of ShardId to list of peer addresses.
    shard_peers: HashMap<ShardId, Vec<String>>,
    /// Number of virtual nodes per physical shard.
    virtual_nodes: usize,
}

impl ShardMap {
    /// Create a new empty ShardMap with a specified number of virtual nodes.
    /// A higher number of virtual nodes provides better load balancing but uses more memory.
    /// Recommended value: 100-200.
    pub fn new(virtual_nodes: usize) -> Self {
        Self {
            ring: BTreeMap::new(),
            shards: HashSet::new(),
            shard_peers: HashMap::new(),
            virtual_nodes,
        }
    }

    /// Add a new Shard to the ring with its peers.
    /// This creates `virtual_nodes` entries in the ring for the shard.
    pub fn add_shard(&mut self, shard_id: ShardId, peers: Vec<String>) {
        if self.shards.contains(&shard_id) {
            return;
        }
        self.shards.insert(shard_id.clone());
        self.shard_peers.insert(shard_id.clone(), peers);

        for i in 0..self.virtual_nodes {
            let hash = self.hash_key(&format!("{}:{}", shard_id, i));
            self.ring.insert(hash, shard_id.clone());
        }
    }

    /// Remove a Shard from the ring.
    pub fn remove_shard(&mut self, shard_id: &ShardId) {
        if !self.shards.contains(shard_id) {
            return;
        }
        self.shards.remove(shard_id);
        self.shard_peers.remove(shard_id);

        // Remove all virtual nodes associated with this shard
        // Note: This is O(N) where N is ring size. Could be optimized if we tracked keys per shard.
        let keys_to_remove: Vec<u64> = self
            .ring
            .iter()
            .filter(|(_, sid)| *sid == shard_id)
            .map(|(k, _)| *k)
            .collect();

        for k in keys_to_remove {
            self.ring.remove(&k);
        }
    }

    /// Get the Shard responsible for the given key (e.g., file path).
    pub fn get_shard(&self, key: &str) -> Option<ShardId> {
        if self.ring.is_empty() {
            return None;
        }

        let hash = self.hash_key(key);

        // Find the first entry with a key >= hash
        // If not found (end of ring), wrap around to the first entry
        let entry = self
            .ring
            .range(hash..)
            .next()
            .or_else(|| self.ring.iter().next());

        entry.map(|(_, shard_id)| shard_id.clone())
    }

    /// Get peers for a specific shard.
    pub fn get_shard_peers(&self, shard_id: &ShardId) -> Option<Vec<String>> {
        self.shard_peers.get(shard_id).cloned()
    }

    /// Helper to compute hash of a string key.
    fn hash_key(&self, key: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish()
    }

    /// Get all registered shards
    pub fn get_all_shards(&self) -> Vec<ShardId> {
        self.shards.iter().cloned().collect()
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

    pub fn to_shard_map(&self, virtual_nodes: usize) -> ShardMap {
        let mut map = ShardMap::new(virtual_nodes);
        for (shard_id, peers) in &self.shards {
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
    fn test_uniform_distribution() {
        let mut map = ShardMap::new(100); // 100 virtual nodes
        let shards = vec!["shard-A", "shard-B", "shard-C", "shard-D", "shard-E"];

        for s in &shards {
            map.add_shard(s.to_string(), vec![]);
        }

        let total_keys = 10000;
        let mut counts: HashMap<ShardId, usize> = HashMap::new();

        for i in 0..total_keys {
            let key = format!("file-path-{}", i);
            let shard = map.get_shard(&key).unwrap();
            *counts.entry(shard).or_insert(0) += 1;
        }

        println!("Distribution with 5 shards and 100 vnodes:");
        for s in &shards {
            let count = counts.get(&s.to_string()).unwrap_or(&0);
            let percentage = (*count as f64 / total_keys as f64) * 100.0;
            println!("{}: {} ({:.2}%)", s, count, percentage);
        }

        // Check if distribution is roughly uniform (e.g., within 20% deviation from ideal 20%)
        // Ideal is 20%. Allow 15% - 25%.
        for s in &shards {
            let count = *counts.get(&s.to_string()).unwrap_or(&0);
            let percentage = (count as f64 / total_keys as f64) * 100.0;
            assert!(
                percentage > 15.0 && percentage < 25.0,
                "Shard {} has {:.2}% keys, expected ~20%",
                s,
                percentage
            );
        }
    }

    #[test]
    fn test_rebalancing_impact() {
        let mut map = ShardMap::new(100);
        let initial_shards = vec!["shard-A", "shard-B", "shard-C"];
        for s in &initial_shards {
            map.add_shard(s.to_string(), vec![]);
        }

        let total_keys = 10000;
        let mut key_mapping: HashMap<String, ShardId> = HashMap::new();

        for i in 0..total_keys {
            let key = format!("key-{}", i);
            let shard = map.get_shard(&key).unwrap();
            key_mapping.insert(key, shard);
        }

        // Add a new shard
        map.add_shard("shard-D".to_string(), vec![]);

        let mut changed_count = 0;
        let mut new_shard_count = 0;

        for (key, old_shard) in &key_mapping {
            let new_shard = map.get_shard(key).unwrap();
            if new_shard != *old_shard {
                changed_count += 1;
            }
            if new_shard == "shard-D" {
                new_shard_count += 1;
            }
        }

        let changed_percentage = (changed_count as f64 / total_keys as f64) * 100.0;
        println!(
            "Keys moved after adding shard-D: {} ({:.2}%)",
            changed_count, changed_percentage
        );

        // Ideally, when going from 3 to 4 shards, 1/4 of keys (25%) should move to the new shard.
        // And almost all moved keys should go to the new shard.

        // Allow some variance, but it should be close to 25%
        assert!(changed_percentage > 20.0 && changed_percentage < 30.0);

        // Verify that most moved keys went to the new shard (Consistent Hashing property)
        // In pure consistent hashing, keys only move TO the new node.
        // However, due to hash collisions or vnode placement, slight variations might occur, but generally:
        // moved_keys should be approx equal to keys_on_new_shard
        assert!((changed_count as i32 - new_shard_count as i32).abs() < (total_keys / 100) as i32);
    }
}
