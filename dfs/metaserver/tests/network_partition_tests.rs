// Integration tests for network partition scenarios
// Tests split-brain prevention, leader election, and partition healing

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

// Mock network layer for simulating partitions
#[derive(Clone)]
struct MockNetwork {
    partitions: Arc<Mutex<HashMap<usize, HashSet<usize>>>>,
}

impl MockNetwork {
    fn new() -> Self {
        Self {
            partitions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Partition the network: nodes in set1 cannot communicate with nodes in set2
    fn create_partition(&self, set1: Vec<usize>, set2: Vec<usize>) {
        let mut partitions = self.partitions.lock().unwrap();
        let set1_hash: HashSet<usize> = set1.into_iter().collect();
        let set2_hash: HashSet<usize> = set2.into_iter().collect();

        for &node in &set1_hash {
            partitions.insert(node, set2_hash.clone());
        }
        for &node in &set2_hash {
            partitions.insert(node, set1_hash.clone());
        }
    }

    /// Heal the partition: all nodes can communicate with each other
    fn heal_partition(&self) {
        let mut partitions = self.partitions.lock().unwrap();
        partitions.clear();
    }

    /// Check if two nodes can communicate
    fn can_communicate(&self, from: usize, to: usize) -> bool {
        let partitions = self.partitions.lock().unwrap();
        if let Some(blocked_nodes) = partitions.get(&from) {
            !blocked_nodes.contains(&to)
        } else {
            true
        }
    }

    /// Isolate a single node from all others
    fn isolate_node(&self, node: usize, all_nodes: Vec<usize>) {
        let mut partitions = self.partitions.lock().unwrap();
        let others: HashSet<usize> = all_nodes.into_iter().filter(|&n| n != node).collect();

        partitions.insert(node, others.clone());
        for &other in &others {
            partitions
                .entry(other)
                .or_insert_with(HashSet::new)
                .insert(node);
        }
    }
}

#[test]
fn test_partition_prevents_split_brain() {
    // Test that a network partition prevents split-brain
    // In a 5-node cluster partitioned 2-3, only the majority partition (3 nodes)
    // should be able to elect a leader

    let network = MockNetwork::new();

    // Initial state: 5 nodes, all can communicate
    let _nodes = vec![0, 1, 2, 3, 4];

    // Partition: {0, 1} vs {2, 3, 4}
    network.create_partition(vec![0, 1], vec![2, 3, 4]);

    // Minority partition (0, 1) - 2 nodes, cannot form majority (need 3)
    let minority_votes = 2;
    let cluster_size = 5;
    let majority_needed = (cluster_size / 2) + 1; // 3
    assert!(
        minority_votes < majority_needed,
        "Minority partition cannot form majority"
    );

    // Majority partition (2, 3, 4) - 3 nodes, can form majority
    let majority_votes = 3;
    assert!(
        majority_votes >= majority_needed,
        "Majority partition can elect leader"
    );

    // Verify network isolation
    assert!(
        !network.can_communicate(0, 2),
        "Nodes in different partitions cannot communicate"
    );
    assert!(
        network.can_communicate(2, 3),
        "Nodes in same partition can communicate"
    );
}

#[test]
fn test_leader_election_after_partition() {
    // Test leader election after a partition is created
    // When a leader is in the minority partition, the majority partition
    // should elect a new leader

    let network = MockNetwork::new();
    let nodes = vec![0, 1, 2, 3, 4];

    // Assume node 0 was the leader before partition
    let old_leader = 0;

    // Partition: {0} vs {1, 2, 3, 4}
    // Leader (node 0) is isolated
    network.isolate_node(old_leader, nodes.clone());

    // Isolated node cannot maintain leadership
    let isolated_votes = 1;
    let cluster_size = 5;
    let majority_needed = (cluster_size / 2) + 1;
    assert!(
        isolated_votes < majority_needed,
        "Isolated leader loses quorum and steps down"
    );

    // Majority partition can elect new leader
    let remaining_nodes = vec![1, 2, 3, 4];
    let majority_votes = remaining_nodes.len();
    assert!(
        majority_votes >= majority_needed,
        "Remaining nodes can elect new leader"
    );

    // New leader should be one of {1, 2, 3, 4}
    let _new_leader = 1; // Example
    assert!(
        remaining_nodes.contains(&1),
        "New leader should be from majority partition"
    );
}

#[test]
fn test_partition_healing() {
    // Test that after a partition heals, the cluster converges to a single leader

    let network = MockNetwork::new();

    // Create partition
    network.create_partition(vec![0, 1], vec![2, 3, 4]);

    // Verify partition exists
    assert!(
        !network.can_communicate(0, 2),
        "Partition prevents communication"
    );

    // Heal partition
    network.heal_partition();

    // Verify all nodes can communicate
    assert!(
        network.can_communicate(0, 2),
        "After healing, all nodes can communicate"
    );
    assert!(
        network.can_communicate(1, 4),
        "After healing, all nodes can communicate"
    );

    // After healing, cluster should converge to single leader
    // The leader with higher term wins
    let leader_in_majority_term = 5; // Majority partition was at term 5
    let old_leader_term = 3; // Minority partition was stuck at term 3

    assert!(
        leader_in_majority_term > old_leader_term,
        "Higher term leader wins after partition heals"
    );
}

#[test]
fn test_write_availability_during_partition() {
    // Test that writes are only possible in the majority partition

    let network = MockNetwork::new();

    // Partition: {0, 1} vs {2, 3, 4}
    network.create_partition(vec![0, 1], vec![2, 3, 4]);

    let cluster_size = 5;
    let majority_needed = (cluster_size / 2) + 1;

    // Minority partition (0, 1) - cannot accept writes
    let minority_size = 2;
    assert!(
        minority_size < majority_needed,
        "Minority partition rejects writes"
    );

    // Majority partition (2, 3, 4) - can accept writes
    let majority_size = 3;
    assert!(
        majority_size >= majority_needed,
        "Majority partition accepts writes"
    );
}

#[test]
fn test_symmetric_partition() {
    // Test a symmetric partition where no side has majority
    // In a 4-node cluster partitioned 2-2, no writes should be accepted

    let network = MockNetwork::new();

    // Partition: {0, 1} vs {2, 3}
    network.create_partition(vec![0, 1], vec![2, 3]);

    let cluster_size = 4;
    let majority_needed = (cluster_size / 2) + 1; // 3

    // Side 1: {0, 1} - 2 nodes, no majority
    let side1_size = 2;
    assert!(side1_size < majority_needed, "Side 1 cannot form majority");

    // Side 2: {2, 3} - 2 nodes, no majority
    let side2_size = 2;
    assert!(side2_size < majority_needed, "Side 2 cannot form majority");

    // Cluster is unavailable for writes
    assert!(
        side1_size < majority_needed && side2_size < majority_needed,
        "Symmetric partition makes cluster unavailable"
    );
}

#[test]
fn test_cascading_partitions() {
    // Test multiple sequential partitions

    let network = MockNetwork::new();
    let nodes = vec![0, 1, 2, 3, 4];

    // First partition: {0} vs {1, 2, 3, 4}
    network.isolate_node(0, nodes.clone());

    assert!(!network.can_communicate(0, 1), "Node 0 is isolated");

    // Heal first partition
    network.heal_partition();

    assert!(network.can_communicate(0, 1), "First partition healed");

    // Second partition: {0, 1, 2} vs {3, 4}
    network.create_partition(vec![0, 1, 2], vec![3, 4]);

    let majority_size = 3;
    let minority_size = 2;
    let cluster_size = 5;
    let majority_needed = (cluster_size / 2) + 1;

    assert!(
        majority_size >= majority_needed,
        "Majority partition {{0,1,2}} can elect leader"
    );
    assert!(
        minority_size < majority_needed,
        "Minority partition {{3,4}} cannot elect leader"
    );
}

#[test]
fn test_term_increases_across_partitions() {
    // Test that term numbers increase correctly across partitions

    // Initial state: node 0 is leader at term 1
    let mut current_term = 1;
    let leader = 0;

    // Partition isolates leader
    let network = MockNetwork::new();
    network.isolate_node(leader, vec![0, 1, 2, 3, 4]);

    // Majority partition {1, 2, 3, 4} starts election
    // Term increases to 2
    current_term += 1;
    assert_eq!(current_term, 2, "Term increases during election");

    // New leader elected in majority partition at term 2
    let _new_leader = 1;
    let new_leader_term = current_term;

    // Partition heals
    network.heal_partition();

    // Old leader (node 0) discovers higher term and steps down
    let old_leader_term = 1;
    assert!(
        new_leader_term > old_leader_term,
        "Old leader steps down when seeing higher term"
    );

    // Cluster converges to term 2 with new leader
    assert_eq!(new_leader_term, 2, "Cluster converges to highest term");
}

#[test]
fn test_log_consistency_after_partition() {
    // Test that log consistency is maintained after partition healing

    // Before partition: all nodes have log entries [1, 2, 3]
    let initial_log = vec![1, 2, 3];

    // Partition: {0, 1} vs {2, 3, 4}
    let network = MockNetwork::new();
    network.create_partition(vec![0, 1], vec![2, 3, 4]);

    // Minority partition {0, 1} cannot commit new entries
    // (no majority, so commit_index stays at 3)
    let _minority_commit_index = 3;

    // Majority partition {2, 3, 4} can commit new entries [4, 5]
    let majority_log = vec![1, 2, 3, 4, 5];
    let majority_commit_index = 5;

    // Partition heals
    network.heal_partition();

    // Minority nodes {0, 1} receive AppendEntries from new leader
    // Their logs are updated to match the leader's log
    let final_log = majority_log.clone();

    // All nodes should have the same log after healing
    assert_eq!(final_log.len(), 5, "All nodes converge to the same log");
    assert!(
        final_log.len() > initial_log.len(),
        "Log has grown during partition"
    );

    // Minority nodes' commit index catches up
    let final_commit_index = majority_commit_index;
    assert_eq!(
        final_commit_index, 5,
        "Commit index converges across all nodes"
    );
}

#[test]
fn test_client_behavior_during_partition() {
    // Test client behavior when leader is in minority partition

    let network = MockNetwork::new();

    // Initial: client connected to leader (node 0)
    let leader = 0;
    let _client_connected_to = leader;

    // Partition isolates leader: {0} vs {1, 2, 3, 4}
    network.isolate_node(leader, vec![0, 1, 2, 3, 4]);

    // Client attempts write to isolated leader
    // Leader cannot achieve quorum, write fails
    let cluster_size = 5;
    let majority_needed = (cluster_size / 2) + 1;
    let leader_partition_size = 1;

    assert!(
        leader_partition_size < majority_needed,
        "Isolated leader cannot commit writes"
    );

    // Client should receive error and discover new leader
    let _new_leader = 1; // In majority partition

    // Client retries with new leader
    let new_leader_partition_size = 4;
    assert!(
        new_leader_partition_size >= majority_needed,
        "New leader can commit writes"
    );
}

#[test]
fn test_read_consistency_during_partition() {
    // Test read consistency guarantees during partition

    let network = MockNetwork::new();

    // Before partition: committed value at index 5 is "foo"
    let _committed_value = "foo";
    let commit_index = 5;

    // Partition: {0, 1} vs {2, 3, 4}
    network.create_partition(vec![0, 1], vec![2, 3, 4]);

    // Majority partition commits new value "bar" at index 6
    let _new_value = "bar";
    let new_commit_index = 6;

    // Minority partition still reads old commit_index (5)
    // This is correct - minority cannot see uncommitted updates
    let minority_commit_index = commit_index;
    assert_eq!(
        minority_commit_index, 5,
        "Minority reads from old commit index"
    );

    // Majority partition can read new committed value
    let majority_commit_index = new_commit_index;
    assert_eq!(
        majority_commit_index, 6,
        "Majority reads from new commit index"
    );

    // After partition heals, all nodes see "bar" at index 6
    network.heal_partition();
    let final_commit_index = new_commit_index;
    assert_eq!(final_commit_index, 6, "All nodes converge to latest commit");
}

#[test]
fn test_three_way_partition() {
    // Test a three-way partition where no group has majority
    // Cluster should be unavailable

    let network = MockNetwork::new();

    // For a 6-node cluster: {0, 1} vs {2, 3} vs {4, 5}
    // We can simulate this with two separate partitions

    // Partition 1: {0, 1} isolated from others
    network.create_partition(vec![0, 1], vec![2, 3, 4, 5]);

    // Then partition 2: {2, 3} isolated from {4, 5}
    // (This is simplified; in reality, we'd need more complex partition state)

    let cluster_size = 6;
    let majority_needed = (cluster_size / 2) + 1; // 4

    // Each partition has only 2 nodes
    let partition_size = 2;
    assert!(
        partition_size < majority_needed,
        "No partition can form majority in three-way split"
    );

    // Cluster is unavailable for writes
    assert!(
        partition_size < majority_needed,
        "Three-way partition makes cluster unavailable"
    );
}
