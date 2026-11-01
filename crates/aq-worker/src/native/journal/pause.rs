//! Explicit test-only fsync barrier; dropping its controller always releases it.
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
pub(crate) struct Gate {
    entered: AtomicBool,
    released: Mutex<bool>,
    wake: Condvar,
}
impl Gate {
    pub(crate) fn wait(&self) -> anyhow::Result<()> {
        self.entered.store(true, Ordering::SeqCst);
        let mut released = self
            .released
            .lock()
            .map_err(|_| anyhow::anyhow!("test write barrier poisoned"))?;
        while !*released {
            released = self
                .wake
                .wait(released)
                .map_err(|_| anyhow::anyhow!("test write barrier poisoned"))?;
        }
        Ok(())
    }
}
pub struct WritePause {
    pub(super) gate: Arc<Gate>,
}
impl WritePause {
    pub fn new() -> Self {
        Self {
            gate: Arc::new(Gate {
                entered: AtomicBool::new(false),
                released: Mutex::new(false),
                wake: Condvar::new(),
            }),
        }
    }
    #[cfg(test)]
    pub(crate) fn clone_gate(&self) -> Arc<Gate> {
        self.gate.clone()
    }
    pub fn entered(&self) -> bool {
        self.gate.entered.load(Ordering::SeqCst)
    }
    pub fn release(&self) {
        *self.gate.released.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.gate.wake.notify_all();
    }
}
impl Default for WritePause {
    fn default() -> Self {
        Self::new()
    }
}
impl Drop for WritePause {
    fn drop(&mut self) {
        self.release();
    }
}
