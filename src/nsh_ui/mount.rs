//! Generic one-shot mount registry for handing non-`Clone` host payloads
//! (channels + initial snapshot) into an NSH root component via a `u64` ID.
//!
//! Entries are removed on first [`take`](Self::take) or in the host's `Drop`
//! impl, preventing leaks across repeated open/close cycles.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

pub struct MountRegistry<T> {
    next_id: AtomicU64,
    pending: OnceLock<Mutex<HashMap<u64, T>>>,
}

impl<T> MountRegistry<T> {
    pub const fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            pending: OnceLock::new(),
        }
    }

    fn map(&self) -> &Mutex<HashMap<u64, T>> {
        self.pending.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub fn register(&self, mount: T) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut map) = self.map().lock() {
            map.insert(id, mount);
        }
        id
    }

    pub fn take(&self, id: u64) -> Option<T> {
        self.map().lock().ok()?.remove(&id)
    }
}
