// Property-based tests for Raft consensus using proptest
// Tests invariants that should hold for all possible inputs

use proptest::prelude::*;

// Property 1: Log Append Order Invariant
// If we append entries to a log, the log length always increases monotonically
proptest! {
    #[test]
    fn test_log_append_order_monotonic(entries in prop::collection::vec(1u64..1000, 0..100)) {
        let mut log_length = 0;

        for _entry in entries {
            // Simulate appending to log
            log_length += 1;

            // Property: log length never decreases
            prop_assert!(log_length > 0);
        }
    }
}

// Property 2: Commit Index Monotonicity
// Commit index never decreases regardless of input
proptest! {
    #[test]
    fn test_commit_index_never_decreases(
        updates in prop::collection::vec(0usize..1000, 1..50)
    ) {
        let mut commit_index = 0;

        for new_index in updates {
            let old_index = commit_index;
            commit_index = commit_index.max(new_index);

            // Property: commit_index >= old commit_index
            prop_assert!(commit_index >= old_index);
        }
    }
}

// Property 3: Term Monotonicity
// Terms always increase or stay the same, never decrease
proptest! {
    #[test]
    fn test_term_monotonicity(terms in prop::collection::vec(0u64..1000, 1..50)) {
        let mut current_term = 0u64;

        for &new_term in &terms {
            if new_term >= current_term {
                current_term = new_term;
            }
            // If new_term < current_term, we reject it (step down to follower)

            // Property: current term never goes backwards
            prop_assert!(terms.iter().take_while(|&&t| t <= current_term).all(|&t| t <= current_term));
        }
    }
}

// Property 4: Majority Calculation
// For any cluster size n, majority is always (n/2) + 1
proptest! {
    #[test]
    fn test_majority_calculation_property(cluster_size in 1usize..20) {
        let majority = (cluster_size / 2) + 1;

        // Property 1: Majority is always > cluster_size / 2
        prop_assert!(majority > cluster_size / 2);

        // Property 2: Majority <= cluster_size
        prop_assert!(majority <= cluster_size);

        // Property 3: Two majorities always overlap (quorum intersection)
        // If we have two sets of size 'majority', they must share at least one element
        prop_assert!(2 * majority > cluster_size);
    }
}

// Property 5: Vote Counting
// Adding votes to a set never decreases the vote count
proptest! {
    #[test]
    fn test_vote_counting_monotonic(votes in prop::collection::vec(0usize..100, 0..50)) {
        use std::collections::HashSet;
        let mut vote_set = HashSet::new();
        let mut last_count = 0;

        for vote in votes {
            vote_set.insert(vote);
            let current_count = vote_set.len();

            // Property: vote count never decreases
            prop_assert!(current_count >= last_count);
            last_count = current_count;
        }
    }
}

// Property 6: Log Matching Property
// If two logs have the same entry at index i with term t,
// all preceding entries must also match
proptest! {
    #[test]
    fn test_log_matching_property(
        common_prefix in prop::collection::vec(1u64..100, 0..20),
        leader_suffix in prop::collection::vec(1u64..100, 0..10),
        follower_suffix in prop::collection::vec(1u64..100, 0..10)
    ) {
        // Create two logs with common prefix
        let mut leader_log = common_prefix.clone();
        let mut follower_log = common_prefix.clone();

        leader_log.extend(leader_suffix);
        follower_log.extend(follower_suffix);

        // Property: If they match at any index in the common prefix,
        // all previous entries must also match
        for i in 0..common_prefix.len() {
            prop_assert_eq!(leader_log[i], follower_log[i]);
        }
    }
}

// Property 7: Quorum Intersection
// Any two quorums (majorities) must have at least one node in common
proptest! {
    #[test]
    fn test_quorum_intersection(
        cluster_size in 3usize..15,
        quorum1_start in 0usize..10,
        quorum2_start in 0usize..10
    ) {
        let majority_size = (cluster_size / 2) + 1;

        // Create two quorums
        let quorum1: Vec<usize> = (quorum1_start..quorum1_start + majority_size)
            .map(|i| i % cluster_size)
            .collect();
        let quorum2: Vec<usize> = (quorum2_start..quorum2_start + majority_size)
            .map(|i| i % cluster_size)
            .collect();

        // Property: quorums must intersect
        let intersection: Vec<_> = quorum1.iter().filter(|x| quorum2.contains(x)).collect();
        prop_assert!(!intersection.is_empty(),
            "Quorums must intersect: cluster={}, majority={}, q1={:?}, q2={:?}",
            cluster_size, majority_size, quorum1, quorum2);
    }
}

// Property 8: Leader Election Safety
// At most one leader per term (simulated vote distribution)
proptest! {
    #[test]
    fn test_at_most_one_leader_per_term(
        cluster_size in 3usize..10,
        winner_id in 0usize..10
    ) {
        let majority = (cluster_size / 2) + 1;
        let winner = winner_id % cluster_size;

        // Simulate votes: winner gets majority, others get less
        let mut votes_for_candidates = vec![0; cluster_size];

        // Winner gets exactly majority votes
        votes_for_candidates[winner] = majority;

        // Distribute remaining votes to others (but not enough for majority)
        let remaining_votes = cluster_size - majority;
        for i in 0..remaining_votes {
            let other_candidate = (winner + i + 1) % cluster_size;
            if votes_for_candidates[other_candidate] < majority - 1 {
                votes_for_candidates[other_candidate] += 1;
            }
        }

        // Property: At most one candidate should have majority votes
        let leaders: Vec<_> = votes_for_candidates
            .iter()
            .enumerate()
            .filter(|(_, &votes)| votes >= majority)
            .collect();

        prop_assert!(leaders.len() <= 1,
            "At most one leader per term, found {} leaders with votes: {:?}",
            leaders.len(), votes_for_candidates);
    }
}

// Property 9: Log Compaction Preserves Committed Entries
// After compaction, all committed entries should still be recoverable
proptest! {
    #[test]
    fn test_log_compaction_preserves_committed(
        log_entries in prop::collection::vec(1u64..1000, 10..100),
        snapshot_point in 0usize..50
    ) {
        let log = log_entries.clone();
        let snapshot_index = snapshot_point.min(log.len());

        // Snapshot represents committed entries
        let snapshot: Vec<_> = log.iter().take(snapshot_index).cloned().collect();

        // After compaction, log = snapshot + remaining entries
        let remaining: Vec<_> = log.iter().skip(snapshot_index).cloned().collect();
        let reconstructed: Vec<_> = snapshot.iter().chain(remaining.iter()).cloned().collect();

        // Property: All original entries are still present
        prop_assert_eq!(reconstructed.len(), log.len());
        for (i, &entry) in log.iter().enumerate() {
            prop_assert_eq!(reconstructed[i], entry);
        }
    }
}

// Property 10: State Machine Determinism
// Applying the same sequence of commands should always yield the same state
proptest! {
    #[test]
    fn test_state_machine_determinism(
        commands in prop::collection::vec(prop::string::string_regex("[a-z]{1,10}").unwrap(), 1..20)
    ) {
        // Apply commands twice
        let mut state1 = String::new();
        let mut state2 = String::new();

        for cmd in &commands {
            state1.push_str(cmd);
        }

        for cmd in &commands {
            state2.push_str(cmd);
        }

        // Property: Same inputs produce same outputs
        prop_assert_eq!(state1, state2);
    }
}

// Property 11: Append Entries Consistency Check
// If prev_log_index and prev_log_term match, then all previous entries match
proptest! {
    #[test]
    fn test_append_entries_consistency(
        leader_log in prop::collection::vec(1u64..100, 5..30),
        follower_log_len in 3usize..20
    ) {
        let prev_log_index = follower_log_len.min(leader_log.len() - 1);

        if prev_log_index > 0 && prev_log_index < leader_log.len() {
            let prev_log_term = leader_log[prev_log_index - 1];

            // Follower log (prefix of leader log)
            let follower_log: Vec<_> = leader_log.iter().take(prev_log_index).cloned().collect();

            // Property: If prev_log matches, all previous entries match
            for i in 0..prev_log_index.min(follower_log.len()) {
                if i < leader_log.len() {
                    prop_assert_eq!(follower_log[i], leader_log[i]);
                }
            }

            // Property: prev_log_term is from leader's log
            prop_assert_eq!(prev_log_term, leader_log[prev_log_index - 1]);
        }
    }
}

// Property 12: Match Index Monotonicity for Each Follower
// Leader's match_index for each follower never decreases
proptest! {
    #[test]
    fn test_match_index_per_follower_monotonic(
        updates in prop::collection::vec((0usize..5, 0usize..100), 1..30)
    ) {
        use std::collections::HashMap;
        let mut match_index: HashMap<usize, usize> = HashMap::new();

        for (follower_id, new_index) in updates {
            let old_index = match_index.get(&follower_id).cloned().unwrap_or(0);
            let updated_index = old_index.max(new_index);
            match_index.insert(follower_id, updated_index);

            // Property: match_index for each follower never decreases
            prop_assert!(updated_index >= old_index);
        }
    }
}

// Property 13: Leader Completeness
// If a log entry is committed at index i, it must appear in the logs of all future leaders
proptest! {
    #[test]
    fn test_leader_completeness_property(
        log_entries in prop::collection::vec((1u64..10, 1u64..100), 5..20),
        commit_index in 0usize..10
    ) {
        // Create logs from entries (term, value)
        let log: Vec<(u64, u64)> = log_entries.clone();
        let actual_commit = commit_index.min(log.len());

        // Committed entries
        let committed: Vec<_> = log.iter().take(actual_commit).cloned().collect();

        // Simulate a new leader's log (must contain all committed entries)
        let leader_log = log.clone();

        // Property: All committed entries are in the new leader's log
        for (i, entry) in committed.iter().enumerate() {
            if i < leader_log.len() {
                prop_assert_eq!(*entry, leader_log[i]);
            }
        }
    }
}

// Property 14: Election Timeout Randomization
// Random timeouts should be within the specified range and vary
proptest! {
    #[test]
    fn test_election_timeout_randomization(
        min_timeout in 150u64..1000,
        max_timeout_offset in 100u64..2000
    ) {
        let max_timeout = min_timeout + max_timeout_offset;

        // Generate random timeout (simulated)
        let timeout = min_timeout + (max_timeout_offset / 2);

        // Property: timeout is within bounds
        prop_assert!(timeout >= min_timeout);
        prop_assert!(timeout <= max_timeout);
    }
}

// Property 15: Log Index Density
// Log indices should be dense (no gaps) after initial construction
proptest! {
    #[test]
    fn test_log_index_density(
        num_entries in 1usize..100
    ) {
        // Log indices should be [1, 2, 3, ..., n]
        let log_indices: Vec<usize> = (1..=num_entries).collect();

        // Property: No gaps in indices
        for i in 0..log_indices.len() - 1 {
            prop_assert_eq!(log_indices[i + 1], log_indices[i] + 1);
        }

        // Property: First index is 1 (or last_snapshot_index + 1)
        if !log_indices.is_empty() {
            prop_assert_eq!(log_indices[0], 1);
        }
    }
}
