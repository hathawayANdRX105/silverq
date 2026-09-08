//! Fast path: apply measurement results immediately without waiting for the entire pool.
//! Timeout results are heavily penalized and the node is moved toward the back of observation.
use crate::batch::Measurement;
use crate::node::Node;

/// Apply a batch of measurements immediately.
/// - Successful measurements update EWMA normally.
/// - Timeout/failure results apply a large penalty (move node to back).
pub fn apply_batch(nodes: &mut [Node], measurements: &[Measurement], timeout_penalty: f64) {
    for m in measurements {
        if let Some(node) = nodes.iter_mut().find(|n| n.tag == m.tag) {
            if let Some(delay) = m.delay_ms {
                node.update(delay);
            } else {
                // Timeout or failure: apply heavy penalty and mark as needing observation
                if node.ewma.is_finite() {
                    node.ewma = (node.ewma + timeout_penalty).min(9999.0);
                } else {
                    node.ewma = timeout_penalty;
                }
            }
        }
    }
}
