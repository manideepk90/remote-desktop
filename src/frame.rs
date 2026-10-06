//! Latest captured frame, shared between the capture thread and every client.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// A captured frame in packed BGRX (little-endian 0x00RRGGBB), stride = width * 4.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub seq: u64,
}

impl Frame {
    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    pub fn row(&self, y: u32, x: u32, w: u32) -> &[u8] {
        let start = y as usize * self.stride() + x as usize * 4;
        &self.data[start..start + w as usize * 4]
    }
}

#[derive(Default)]
pub struct FrameStore {
    cur: Mutex<Option<Arc<Frame>>>,
    cv: Condvar,
}

impl FrameStore {
    pub fn publish(&self, width: u32, height: u32, data: Vec<u8>) {
        let mut cur = self.cur.lock().unwrap();
        let seq = cur.as_ref().map_or(1, |f| f.seq + 1);
        *cur = Some(Arc::new(Frame { width, height, data, seq }));
        self.cv.notify_all();
    }

    pub fn latest(&self) -> Option<Arc<Frame>> {
        self.cur.lock().unwrap().clone()
    }

    /// Waits until a frame newer than `seq` is available, or the timeout elapses.
    pub fn wait_newer(&self, seq: u64, timeout: Duration) -> Option<Arc<Frame>> {
        let cur = self.cur.lock().unwrap();
        let (cur, _) = self
            .cv
            .wait_timeout_while(cur, timeout, |c| c.as_ref().is_none_or(|f| f.seq <= seq))
            .unwrap();
        cur.as_ref().filter(|f| f.seq > seq).cloned()
    }
}
