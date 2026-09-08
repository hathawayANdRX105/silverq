//! MeowMeasurer - real implementation using meow-rs's health module.
//!
//! Uses `meow_proxy::health::probe_and_record` to perform protocol-aware
//! delay measurement through a real proxy connection.
//!
//! lift only handles scheduling; all protocol/transport concerns are
//! delegated to meow-rs.
#![cfg(feature = "meow")]

use crate::batch::Measurer;
use crate::node::Node;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// URL to probe for delay measurement.
/// gstatic's generate_204 is small (no body), stable, and widely used.
const PROBE_URL: &str = "https://www.gstatic.com/generate_204";

/// MeowMeasurer uses a meow-rs Proxy and probes it via probe_and_record.
///
/// To use this, you must provide a pre-built `Arc<dyn meow_proxy::Proxy>`
/// (or a way to construct one) that represents the proxy under test.
pub struct MeowMeasurer {
    /// Optional probe target proxy. If `None`, this measurer returns `None`
    /// for every node (effectively disabled — useful for testing the scheduler
    /// without a real meow pipeline).
    ///
    /// In production, this would be either:
    proxy: Option<Arc<dyn meow_common::adapter::Proxy>>,
}

impl MeowMeasurer {
    pub fn new() -> Self {
        Self { proxy: None }
    }

    /// Construct a measurer with a pre-built meow Proxy.
    pub fn with_proxy(proxy: Arc<dyn meow_common::adapter::Proxy>) -> Self {
        Self { proxy: Some(proxy) }
    }
}

impl Default for MeowMeasurer {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Measurer for MeowMeasurer {
    async fn measure(&self, _node: &Node, timeout_ms: u64) -> Option<f64> {
        // No proxy configured → skip. The caller will treat this as None
        // (timeout) and apply the heavy penalty.
        let proxy = self.proxy.as_ref()?;

        let url: Arc<str> = Arc::from(PROBE_URL);
        let expected: Option<Arc<str>> = Some(Arc::from("200,204"));
        let timeout = Duration::from_millis(timeout_ms);

        // Wall-clock start to detect internal timeouts.
        let started = Instant::now();

        let result = meow_proxy::health::probe_and_record(
            proxy,
            url.as_ref(),
            expected.as_deref(),
            timeout,
        )
        .await;

        let elapsed_ms = started.elapsed().as_millis();

        match result {
            Ok(delay) if delay > 0 => Some(elapsed_ms.min(u32::MAX as u128) as f64),
            _ => None,
        }
    }
}