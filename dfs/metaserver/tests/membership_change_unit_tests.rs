// Unit tests for Dynamic Membership Changes
// Tests joint consensus, catch-up protocol, and configuration management

use dfs_metaserver::simple_raft::{
    CatchUpProgress, ClusterConfiguration, ConfigChangeState, ServerRole,
};
use std::collections::{HashMap, HashSet};

#[test]
fn test_cluster_configuration_simple_majority() {
    let mut members = HashMap::new();
    members.insert(0, "addr0".to_string());
    members.insert(1, "addr1".to_string());
    members.insert(2, "addr2".to_string());

    let config = ClusterConfiguration::Simple {
        members,
        version: 1,
    };

    // Test majority with 3 members (need 2)
    let mut acks = HashSet::new();
    acks.insert(0);
    acks.insert(1);
    assert!(
        config.has_joint_majority(&acks),
        "Should have majority with 2/3"
    );

    // Test no majority
    let mut acks = HashSet::new();
    acks.insert(0);
    assert!(
        !config.has_joint_majority(&acks),
        "Should not have majority with 1/3"
    );
}

#[test]
fn test_cluster_configuration_joint_majority() {
    let mut old_members = HashMap::new();
    old_members.insert(0, "addr0".to_string());
    old_members.insert(1, "addr1".to_string());
    old_members.insert(2, "addr2".to_string());

    let mut new_members = HashMap::new();
    new_members.insert(0, "addr0".to_string());
    new_members.insert(1, "addr1".to_string());
    new_members.insert(3, "addr3".to_string());

    let config = ClusterConfiguration::Joint {
        old_members,
        new_members,
        version: 2,
    };

    // Test: Need majority in BOTH old (2/3) and new (2/3)
    let mut acks = HashSet::new();
    acks.insert(0);
    acks.insert(1);
    assert!(
        config.has_joint_majority(&acks),
        "Should have joint majority with nodes 0,1 (majority in both)"
    );

    // Test: Only majority in old, not in new
    let mut acks = HashSet::new();
    acks.insert(0);
    acks.insert(2); // Node 2 is only in old config
    assert!(
        !config.has_joint_majority(&acks),
        "Should not have joint majority (only 1/3 in new config)"
    );

    // Test: Only majority in new, not in old
    let mut acks = HashSet::new();
    acks.insert(0);
    acks.insert(3); // Node 3 is only in new config
    assert!(
        !config.has_joint_majority(&acks),
        "Should not have joint majority (only 1/3 in old config)"
    );

    // Test: Majority in both configs with different members
    let mut acks = HashSet::new();
    acks.insert(0);
    acks.insert(1);
    acks.insert(3);
    assert!(
        config.has_joint_majority(&acks),
        "Should have joint majority with nodes 0,1,3"
    );
}

#[test]
fn test_cluster_configuration_all_members() {
    let mut old_members = HashMap::new();
    old_members.insert(0, "addr0".to_string());
    old_members.insert(1, "addr1".to_string());

    let mut new_members = HashMap::new();
    new_members.insert(1, "addr1".to_string());
    new_members.insert(2, "addr2".to_string());

    let config = ClusterConfiguration::Joint {
        old_members,
        new_members,
        version: 1,
    };

    let all = config.all_members();

    // Should return union of old and new
    assert_eq!(all.len(), 3, "Should have 3 total members");
    assert!(all.contains_key(&0));
    assert!(all.contains_key(&1));
    assert!(all.contains_key(&2));
}

#[test]
fn test_cluster_configuration_version() {
    let config = ClusterConfiguration::Simple {
        members: HashMap::new(),
        version: 42,
    };
    assert_eq!(config.version(), 42);

    let config = ClusterConfiguration::Joint {
        old_members: HashMap::new(),
        new_members: HashMap::new(),
        version: 99,
    };
    assert_eq!(config.version(), 99);
}

#[test]
fn test_catchup_progress_initial_state() {
    let progress = CatchUpProgress::new(1000);

    assert_eq!(progress.match_index, 0);
    assert_eq!(progress.rounds_caught_up, 0);
    assert_eq!(progress.added_at, 1000);
    assert!(!progress.is_caught_up(100));
}

#[test]
fn test_catchup_progress_update() {
    let mut progress = CatchUpProgress::new(1000);

    // Update progress
    progress.update(50);
    assert_eq!(progress.match_index, 50);
    assert_eq!(progress.rounds_caught_up, 1);

    // Update again
    progress.update(100);
    assert_eq!(progress.match_index, 100);
    assert_eq!(progress.rounds_caught_up, 2);

    // No update if index doesn't increase
    progress.update(100);
    assert_eq!(progress.match_index, 100);
    assert_eq!(progress.rounds_caught_up, 2);
}

#[test]
fn test_catchup_progress_is_caught_up() {
    let mut progress = CatchUpProgress::new(1000);

    // Not caught up initially
    assert!(!progress.is_caught_up(100));

    // Update 10 times to reach caught up threshold
    for i in 1..=10 {
        progress.update(i * 10);
    }

    assert_eq!(progress.rounds_caught_up, 10);
    assert!(
        progress.is_caught_up(100),
        "Should be caught up after 10 rounds at commit index 100"
    );
}

#[test]
fn test_catchup_progress_not_caught_up_if_behind() {
    let mut progress = CatchUpProgress::new(1000);

    // Even with 10 rounds, not caught up if behind commit index
    for i in 1..=10 {
        progress.update(i * 5);
    }

    assert_eq!(progress.match_index, 50);
    assert_eq!(progress.rounds_caught_up, 10);
    assert!(
        !progress.is_caught_up(100),
        "Should not be caught up if match_index < commit_index"
    );
}

#[test]
fn test_server_role_enum() {
    let voting = ServerRole::Voting;
    let non_voting = ServerRole::NonVoting;

    assert_ne!(voting, non_voting);
    assert_eq!(voting, ServerRole::Voting);
}

#[test]
fn test_config_change_state_none() {
    let state = ConfigChangeState::None;
    assert!(matches!(state, ConfigChangeState::None));
}

#[test]
fn test_config_change_state_adding_servers() {
    let mut servers = HashMap::new();
    servers.insert(3, ("addr3".to_string(), CatchUpProgress::new(1000)));

    let state = ConfigChangeState::AddingServers {
        servers,
        started_at: 1000,
    };

    if let ConfigChangeState::AddingServers {
        servers,
        started_at,
    } = state
    {
        assert_eq!(servers.len(), 1);
        assert_eq!(started_at, 1000);
    } else {
        panic!("Wrong state variant");
    }
}

#[test]
fn test_config_change_state_in_joint_consensus() {
    let mut target = HashMap::new();
    target.insert(0, "addr0".to_string());
    target.insert(1, "addr1".to_string());

    let state = ConfigChangeState::InJointConsensus {
        joint_config_index: 42,
        target_config: target,
    };

    if let ConfigChangeState::InJointConsensus {
        joint_config_index,
        target_config,
    } = state
    {
        assert_eq!(joint_config_index, 42);
        assert_eq!(target_config.len(), 2);
    } else {
        panic!("Wrong state variant");
    }
}

#[test]
fn test_config_change_state_transferring_leadership() {
    let state = ConfigChangeState::TransferringLeadership {
        target_server: 2,
        servers_to_remove: vec![0, 1],
    };

    if let ConfigChangeState::TransferringLeadership {
        target_server,
        servers_to_remove,
    } = state
    {
        assert_eq!(target_server, 2);
        assert_eq!(servers_to_remove.len(), 2);
    } else {
        panic!("Wrong state variant");
    }
}

#[test]
fn test_joint_consensus_edge_case_single_node() {
    let mut members = HashMap::new();
    members.insert(0, "addr0".to_string());

    let config = ClusterConfiguration::Simple {
        members,
        version: 1,
    };

    let mut acks = HashSet::new();
    acks.insert(0);
    assert!(
        config.has_joint_majority(&acks),
        "Single node should have majority with itself"
    );

    let acks = HashSet::new();
    assert!(
        !config.has_joint_majority(&acks),
        "Single node with no acks should not have majority"
    );
}

#[test]
fn test_joint_consensus_five_node_cluster() {
    let mut members = HashMap::new();
    for i in 0..5 {
        members.insert(i, format!("addr{}", i));
    }

    let config = ClusterConfiguration::Simple {
        members,
        version: 1,
    };

    // Test: Need 3/5
    let mut acks = HashSet::new();
    acks.insert(0);
    acks.insert(1);
    acks.insert(2);
    assert!(config.has_joint_majority(&acks));

    // Test: Only 2/5 is not enough
    let mut acks = HashSet::new();
    acks.insert(0);
    acks.insert(1);
    assert!(!config.has_joint_majority(&acks));
}

#[test]
fn test_joint_consensus_adding_two_servers() {
    // Original: 3 nodes (0, 1, 2)
    let mut old_members = HashMap::new();
    old_members.insert(0, "addr0".to_string());
    old_members.insert(1, "addr1".to_string());
    old_members.insert(2, "addr2".to_string());

    // New: 5 nodes (0, 1, 2, 3, 4)
    let mut new_members = HashMap::new();
    new_members.insert(0, "addr0".to_string());
    new_members.insert(1, "addr1".to_string());
    new_members.insert(2, "addr2".to_string());
    new_members.insert(3, "addr3".to_string());
    new_members.insert(4, "addr4".to_string());

    let config = ClusterConfiguration::Joint {
        old_members,
        new_members,
        version: 1,
    };

    // Need: 2/3 in old AND 3/5 in new
    let mut acks = HashSet::new();
    acks.insert(0);
    acks.insert(1);
    acks.insert(3);
    assert!(
        config.has_joint_majority(&acks),
        "Should have 2/3 old and 3/5 new"
    );

    // Not enough in old
    let mut acks = HashSet::new();
    acks.insert(0);
    acks.insert(3);
    acks.insert(4);
    assert!(
        !config.has_joint_majority(&acks),
        "Only 1/3 in old is not enough"
    );
}

#[test]
fn test_joint_consensus_removing_two_servers() {
    // Original: 5 nodes (0, 1, 2, 3, 4)
    let mut old_members = HashMap::new();
    for i in 0..5 {
        old_members.insert(i, format!("addr{}", i));
    }

    // New: 3 nodes (0, 1, 2) - removing 3 and 4
    let mut new_members = HashMap::new();
    new_members.insert(0, "addr0".to_string());
    new_members.insert(1, "addr1".to_string());
    new_members.insert(2, "addr2".to_string());

    let config = ClusterConfiguration::Joint {
        old_members,
        new_members,
        version: 1,
    };

    // Need: 3/5 in old AND 2/3 in new
    let mut acks = HashSet::new();
    acks.insert(0);
    acks.insert(1);
    acks.insert(2);
    assert!(
        config.has_joint_majority(&acks),
        "Should have 3/5 old and 3/3 new (all new members)"
    );

    // Nodes 3 and 4 (being removed) plus node 0 = 3/5 in old but only 1/3 in new
    let mut acks = HashSet::new();
    acks.insert(0);
    acks.insert(3);
    acks.insert(4);
    assert!(
        !config.has_joint_majority(&acks),
        "Only 1/3 in new is not enough"
    );
}
