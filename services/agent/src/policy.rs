//! Concurrency caps and policy knobs.

use std::sync::Arc;
use tokio::sync::Semaphore;

#[derive(Clone)]
pub struct ConcurrencyPolicy {
    pub tool_semaphore: Arc<Semaphore>,
    pub max_queue_depth: usize,
}

impl ConcurrencyPolicy {
    pub fn from_env() -> Self {
        let max_concurrent: usize = std::env::var("ESON_MAX_CONCURRENT_TOOLS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8)
            .clamp(1, 256);
        let max_queue_depth: usize = std::env::var("ESON_MAX_TOOL_QUEUE_DEPTH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64)
            .clamp(1, 4096);
        Self {
            tool_semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_queue_depth,
        }
    }

    pub async fn acquire_tool_permit(&self) -> tokio::sync::SemaphorePermit<'_> {
        self.tool_semaphore.acquire().await.expect("semaphore closed")
    }
}
