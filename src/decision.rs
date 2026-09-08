//! Decision module: pure EWMA-based selection (no hysteresis rounds).
//! Nodes are ranked solely by current EWMA score.
//! Capacity constraint is enforced (top N become active).
use crate::node::Node;

/// Select the top N nodes by EWMA score (lower is better).
/// Used to determine the active proxy group.
/// No hysteresis or round-based switching — pure score driven.
pub fn select_top(nodes: &[Node], capacity: usize) -> Vec<String> {
    let mut ranked: Vec<_> = nodes.iter().collect();
    ranked.sort_by(|a, b| a.score().partial_cmp(&b.score()).unwrap());
    ranked.into_iter().take(capacity).map(|n| n.tag.clone()).collect()
}
