pub mod pipeline;
pub mod reconciler;
pub mod routing;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Lazily-created async mutexes keyed by id (one per app for deploys, one per node for
/// proxy sync).
#[derive(Default)]
pub struct KeyedLocks(Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>);

impl KeyedLocks {
    pub fn get(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.entry(key.to_string()).or_default().clone()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DeployTiming {
    /// How long a candidate container may take to become healthy.
    pub health_timeout: Duration,
    /// Grace period for in-flight requests before the old container is stopped.
    pub drain: Duration,
    /// Wait after a self-healing action before the next one on the same app; doubles
    /// with each attempt.
    pub heal_backoff: Duration,
    /// How long an app must stay healthy before its self-healing attempts are forgiven.
    pub heal_stable: Duration,
}

impl Default for DeployTiming {
    fn default() -> Self {
        Self {
            health_timeout: Duration::from_secs(30),
            drain: Duration::from_secs(10),
            heal_backoff: Duration::from_secs(30),
            heal_stable: Duration::from_secs(600),
        }
    }
}
