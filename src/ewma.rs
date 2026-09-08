//! EWMA configuration and helpers.
use crate::node::Node;

/// Default EWMA alpha (responsiveness).
/// 0.3 gives good balance between stability and reaction to real changes.
pub const DEFAULT_ALPHA: f64 = 0.3;

/// Update multiple nodes from a batch of measurements.
pub fn apply_measurements(nodes: &mut [Node], measurements: &[crate::batch::Measurement], alpha: f64) {
    for m in measurements {
        if let Some(delay) = m.delay_ms {
            if let Some(node) = nodes.iter_mut().find(|n| n.tag == m.tag) {
                node.update_ewma(delay, alpha);
            }
        }
    }
}
