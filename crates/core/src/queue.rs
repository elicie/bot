//! Serialized command queue: HTTP mutations, scheduler ticks, and broker mutations run one at a
//! time, in arrival order.

use std::future::Future;
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct CommandQueue {
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl CommandQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `operation` after every previously enqueued operation finished.
    pub async fn run<T, F: Future<Output = T>>(&self, operation: F) -> T {
        let _turn = self.lock.lock().await;
        operation.await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn operations_run_in_order() {
        let queue = CommandQueue::new();
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (a, b) = (log.clone(), log.clone());
        let first = queue.run(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            a.lock().unwrap().push(1);
        });
        let second = queue.run(async move { b.lock().unwrap().push(2) });
        tokio::join!(first, second);
        assert_eq!(*log.lock().unwrap(), [1, 2]);
    }
}
