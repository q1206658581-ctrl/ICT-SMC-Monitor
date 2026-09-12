use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::broadcast;

use crate::detector::engine::state::IctEngine;
use crate::detector::types::StructureEvent;

/// Thread-safe handle the bin holds inside `AppState`. Wraps the engine
/// behind a parking_lot Mutex so detectors run synchronously inside the
/// subscription loop without yielding the async runtime.
#[derive(Clone)]
pub struct EngineHandle {
    inner: Arc<Mutex<IctEngine>>,
    pub event_tx: broadcast::Sender<StructureEvent>,
}

impl EngineHandle {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        let engine = IctEngine::new(tx.clone());
        Self {
            inner: Arc::new(Mutex::new(engine)),
            event_tx: tx,
        }
    }

    pub fn lock(&self) -> parking_lot::MutexGuard<'_, IctEngine> {
        self.inner.lock()
    }

    /// Non-blocking lock attempt. Returns `None` when the engine is busy
    /// (e.g. during a PO3 full-history replay that holds the Mutex for
    /// several seconds). Callers can fall back to a cached result instead
    /// of blocking the Tauri command and freezing the chart.
    pub fn try_lock(&self) -> Option<parking_lot::MutexGuard<'_, IctEngine>> {
        self.inner.try_lock()
    }
}
