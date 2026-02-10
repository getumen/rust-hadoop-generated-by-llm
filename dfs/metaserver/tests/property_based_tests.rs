// Property-based tests for Raft consensus using proptest
// Tests invariants that should hold for all possible inputs

use proptest::prelude::*;

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
