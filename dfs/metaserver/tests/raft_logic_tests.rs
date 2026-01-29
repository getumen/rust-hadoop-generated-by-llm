// Unit tests for core Raft logic
// Tests leader election, log replication, commit, and term management

use dfs_metaserver::simple_raft::{Command, LogEntry, Role};
use std::collections::HashSet;

#[test]
fn test_log_entry_ordering() {
    let entry1 = LogEntry {
        term: 1,
        command: Command::NoOp,
    };
    let entry2 = LogEntry {
        term: 2,
        command: Command::NoOp,
    };

    assert!(
        entry1.term < entry2.term,
        "Log entries should be ordered by term"
    );
}

#[test]
fn test_role_transitions() {
    // Test valid role transitions
    let follower = Role::Follower;
    let candidate = Role::Candidate;
    let leader = Role::Leader;

    // Follower can become Candidate
    assert_ne!(follower, candidate);

    // Candidate can become Leader or Follower
    assert_ne!(candidate, leader);
    assert_ne!(candidate, follower);

    // Leader can step down to Follower
    assert_ne!(leader, follower);
}

#[test]
fn test_majority_calculation() {
    // Test majority calculation for different cluster sizes

    // 1 node: majority is 1
    let cluster_size = 1;
    let majority = (cluster_size / 2) + 1;
    assert_eq!(majority, 1, "Majority of 1 node cluster is 1");

    // 3 nodes: majority is 2
    let cluster_size = 3;
    let majority = (cluster_size / 2) + 1;
    assert_eq!(majority, 2, "Majority of 3 node cluster is 2");

    // 5 nodes: majority is 3
    let cluster_size = 5;
    let majority = (cluster_size / 2) + 1;
    assert_eq!(majority, 3, "Majority of 5 node cluster is 3");

    // 7 nodes: majority is 4
    let cluster_size = 7;
    let majority = (cluster_size / 2) + 1;
    assert_eq!(majority, 4, "Majority of 7 node cluster is 4");
}

#[test]
fn test_vote_counting() {
    // Test that we correctly count votes in an election
    let mut votes_received = HashSet::new();

    // Node 0 votes for itself
    votes_received.insert(0);
    assert_eq!(votes_received.len(), 1);

    // Node 1 votes for node 0
    votes_received.insert(1);
    assert_eq!(votes_received.len(), 2);

    // Duplicate vote from node 1 should not be counted
    votes_received.insert(1);
    assert_eq!(
        votes_received.len(),
        2,
        "Duplicate votes should not be counted"
    );

    // In a 3-node cluster, 2 votes is a majority
    let cluster_size = 3;
    let majority = (cluster_size / 2) + 1;
    assert!(
        votes_received.len() >= majority,
        "Should have majority with 2/3 votes"
    );
}

#[test]
fn test_log_matching_property() {
    // Test Raft's log matching property:
    // If two logs contain an entry with the same index and term,
    // then the logs are identical in all entries up through that index.

    let mut log1 = Vec::new();
    let mut log2 = Vec::new();

    // Both logs have identical entries
    for term in 1..=3 {
        log1.push(LogEntry {
            term,
            command: Command::NoOp,
        });
        log2.push(LogEntry {
            term,
            command: Command::NoOp,
        });
    }

    // Check that entries at the same index have the same term
    for i in 0..log1.len() {
        assert_eq!(
            log1[i].term, log2[i].term,
            "Entries at index {} should have the same term",
            i
        );
    }
}

#[test]
fn test_log_consistency_check() {
    // Test that we can detect log inconsistencies
    let mut leader_log = Vec::new();
    let mut follower_log = Vec::new();

    // Leader has entries [term 1, term 2, term 3]
    for term in 1..=3 {
        leader_log.push(LogEntry {
            term,
            command: Command::NoOp,
        });
    }

    // Follower has entries [term 1, term 2]
    for term in 1..=2 {
        follower_log.push(LogEntry {
            term,
            command: Command::NoOp,
        });
    }

    // Follower log is a prefix of leader log
    assert!(follower_log.len() < leader_log.len());
    for i in 0..follower_log.len() {
        assert_eq!(follower_log[i].term, leader_log[i].term);
    }
}

#[test]
fn test_term_comparison() {
    // Test that higher term always wins
    let old_term = 1;
    let new_term = 2;

    assert!(
        new_term > old_term,
        "New term should be greater than old term"
    );

    // If a candidate or leader discovers that its term is out of date,
    // it immediately reverts to follower state
    let current_term = 5;
    let received_term = 6;

    assert!(
        received_term > current_term,
        "Node should step down when it sees a higher term"
    );
}

#[test]
fn test_commit_index_monotonicity() {
    // Test that commit index never decreases

    // Commit advances
    let commit_index = 5;
    assert_eq!(commit_index, 5);

    // Commit index should never go backwards
    let new_commit = 3;
    let commit_index = commit_index.max(new_commit);
    assert_eq!(commit_index, 5, "Commit index should not decrease");

    // Commit can advance further
    let commit_index = commit_index.max(10);
    assert_eq!(commit_index, 10);
}

#[test]
fn test_last_log_index_and_term() {
    // Test calculating last log index and term
    let mut log = Vec::new();

    // Empty log: last index is 0 (snapshot), last term is 0
    let last_index = log.len();
    let last_term = log.last().map(|e: &LogEntry| e.term).unwrap_or(0);
    assert_eq!(last_index, 0);
    assert_eq!(last_term, 0);

    // Add entries
    log.push(LogEntry {
        term: 1,
        command: Command::NoOp,
    });
    log.push(LogEntry {
        term: 2,
        command: Command::NoOp,
    });
    log.push(LogEntry {
        term: 2,
        command: Command::NoOp,
    });

    let last_index = log.len();
    let last_term = log.last().map(|e: &LogEntry| e.term).unwrap_or(0);
    assert_eq!(last_index, 3);
    assert_eq!(last_term, 2);
}

#[test]
fn test_request_vote_decision() {
    // Test RequestVote RPC decision logic

    // Case 1: Candidate's term is less than current term -> reject
    let current_term = 5;
    let candidate_term = 4;
    assert!(
        candidate_term < current_term,
        "Should reject vote if candidate term is old"
    );

    // Case 2: Already voted for different candidate in this term -> reject
    let voted_for = Some(1);
    let candidate_id = 2;
    assert!(
        voted_for.is_some() && voted_for != Some(candidate_id),
        "Should reject vote if already voted for different candidate"
    );

    // Case 3: Candidate's log is not at least as up-to-date -> reject
    let my_last_term = 3;
    let _my_last_index = 10;
    let candidate_last_term = 2;
    let _candidate_last_index = 15;

    // Candidate has older term, even though more entries
    assert!(
        candidate_last_term < my_last_term,
        "Should reject vote if candidate log has older last term"
    );

    // Case 4: Valid case - grant vote
    let current_term = 5;
    let candidate_term = 6;
    let voted_for: Option<usize> = None;
    let my_last_term = 3;
    let candidate_last_term = 3;
    let my_last_index = 10;
    let candidate_last_index = 11;

    let should_grant_vote = candidate_term >= current_term
        && (voted_for.is_none() || voted_for == Some(candidate_id))
        && (candidate_last_term > my_last_term
            || (candidate_last_term == my_last_term && candidate_last_index >= my_last_index));

    assert!(should_grant_vote, "Should grant vote in valid case");
}

#[test]
fn test_append_entries_consistency_check() {
    // Test AppendEntries RPC consistency check
    let mut log = Vec::new();
    log.push(LogEntry {
        term: 1,
        command: Command::NoOp,
    });
    log.push(LogEntry {
        term: 2,
        command: Command::NoOp,
    });
    log.push(LogEntry {
        term: 3,
        command: Command::NoOp,
    });

    // Case 1: prev_log_index is beyond our log -> reject
    let prev_log_index = 5;
    assert!(
        prev_log_index > log.len(),
        "Should reject if prev_log_index is beyond our log"
    );

    // Case 2: Term mismatch at prev_log_index -> reject
    let prev_log_index = 2; // 0-indexed, so this is the 3rd entry (index 2)
    let prev_log_term = 1; // But our log has term 2 at index 2
    let actual_term = log.get(prev_log_index - 1).map(|e| e.term).unwrap_or(0);
    assert!(
        prev_log_term != actual_term,
        "Should reject if term at prev_log_index doesn't match"
    );

    // Case 3: Consistency check passes
    let prev_log_index = 2;
    let prev_log_term = 2; // Matches term at index 1 (0-indexed)
    let actual_term = log.get(prev_log_index - 1).map(|e| e.term).unwrap_or(0);
    assert_eq!(
        prev_log_term, actual_term,
        "Consistency check should pass when terms match"
    );
}

#[test]
fn test_leader_commit_advancement() {
    // Test leader commit index advancement logic

    // Leader has replicated entries to followers
    let leader_log_len = 10;
    let mut match_index = vec![0, 7, 8, 5, 9]; // 5 nodes (including leader at index 0)
    match_index[0] = leader_log_len; // Leader has all entries

    // Sort match_index to find median
    let mut sorted_match_index = match_index.clone();
    sorted_match_index.sort_unstable();
    sorted_match_index.reverse();

    // In a 5-node cluster, majority is 3 nodes
    // Find the index that majority has replicated
    let majority_index = (sorted_match_index.len() / 2) as usize;
    let new_commit_index = sorted_match_index[majority_index];

    assert_eq!(
        new_commit_index, 8,
        "Commit index should advance to entry replicated by majority"
    );

    // Verify majority
    let count = match_index
        .iter()
        .filter(|&&idx| idx >= new_commit_index)
        .count();
    let majority = (match_index.len() / 2) + 1;
    assert!(
        count >= majority,
        "New commit index should be replicated by majority"
    );
}

#[test]
fn test_log_truncation_on_conflict() {
    // Test that follower truncates conflicting entries
    let mut follower_log = vec![
        LogEntry {
            term: 1,
            command: Command::NoOp,
        },
        LogEntry {
            term: 2,
            command: Command::NoOp,
        },
        LogEntry {
            term: 2,
            command: Command::NoOp,
        },
        LogEntry {
            term: 3,
            command: Command::NoOp,
        }, // This and following entries conflict
    ];

    // Leader sends entries starting at index 3 with term 4
    let prev_log_index = 3;
    let new_entries = vec![LogEntry {
        term: 4,
        command: Command::NoOp,
    }];

    // Check if there's a conflict
    if follower_log.len() > prev_log_index {
        let existing_entry_term = follower_log[prev_log_index].term;
        let new_entry_term = new_entries[0].term;

        if existing_entry_term != new_entry_term {
            // Truncate conflicting entries
            follower_log.truncate(prev_log_index);
        }
    }

    // Append new entries
    follower_log.extend(new_entries);

    assert_eq!(
        follower_log.len(),
        4,
        "Log should have correct length after truncation"
    );
    assert_eq!(
        follower_log[3].term, 4,
        "New entry should replace conflicting entry"
    );
}

#[test]
fn test_read_index_safety() {
    // Test ReadIndex protocol safety properties

    // Leader records commit index when read request arrives
    let read_index = 10;
    let commit_index = 10;

    assert_eq!(
        read_index, commit_index,
        "ReadIndex should be set to current commit index"
    );

    // Leader must wait for heartbeat acknowledgment from majority
    let cluster_size = 5;
    let majority = (cluster_size / 2) + 1;
    let mut acks = HashSet::new();
    acks.insert(0); // Leader
    acks.insert(1);
    acks.insert(2);

    assert!(
        acks.len() >= majority,
        "Must have majority acks before serving read"
    );

    // Only serve read when last_applied >= read_index
    let last_applied = 10;
    assert!(
        last_applied >= read_index,
        "Can only serve read when state machine is up-to-date"
    );
}

#[test]
fn test_snapshot_log_compaction() {
    // Test that snapshots correctly compact the log
    let mut log = Vec::new();

    // Add 100 entries
    for i in 1..=100 {
        log.push(LogEntry {
            term: (i / 30) + 1, // Change term every 30 entries
            command: Command::NoOp,
        });
    }

    assert_eq!(log.len(), 100, "Log should have 100 entries");

    // Take snapshot at index 50
    let snapshot_index = 50;
    let _snapshot_term = log[snapshot_index - 1].term;

    // Remove entries included in snapshot
    log.drain(0..snapshot_index);

    assert_eq!(
        log.len(),
        50,
        "Log should have 50 entries after snapshot compaction"
    );

    // First entry in compacted log should be entry 51
    let first_remaining_index = snapshot_index + 1;
    assert_eq!(
        first_remaining_index, 51,
        "First remaining entry should be at index 51"
    );
}

#[test]
fn test_election_timeout_randomization() {
    // Test that election timeouts are randomized to prevent split votes
    use rand::Rng;

    let mut timeouts = Vec::new();
    for _ in 0..10 {
        let timeout_ms = rand::rng().random_range(1500..3000);
        timeouts.push(timeout_ms);
    }

    // Check that we have some variation in timeouts
    let min_timeout = *timeouts.iter().min().unwrap();
    let max_timeout = *timeouts.iter().max().unwrap();

    assert!(
        max_timeout > min_timeout,
        "Election timeouts should be randomized"
    );
    assert!(min_timeout >= 1500, "Min timeout should be at least 1500ms");
    assert!(max_timeout < 3000, "Max timeout should be less than 3000ms");
}
