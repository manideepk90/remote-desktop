//! Live client sessions, for the settings page and for kicking clients.

use std::net::{Shutdown, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

use crate::config::now;
use crate::pairing::Device;

#[derive(Clone, Serialize)]
pub struct SessionInfo {
    pub id: u64,
    pub device: Device,
    pub connected_at: u64,
    pub bytes_sent: u64,
    /// Per-session override on top of the global view-only setting.
    pub view_only: bool,
}

struct Entry {
    info: SessionInfo,
    stream: TcpStream,
}

#[derive(Default)]
pub struct Sessions {
    list: Mutex<Vec<Entry>>,
    next_id: AtomicU64,
}

impl Sessions {
    pub fn add(&self, device: &Device, stream: TcpStream) -> SessionInfo {
        let info = SessionInfo {
            id: self.next_id.fetch_add(1, Ordering::Relaxed) + 1,
            device: device.clone(),
            connected_at: now(),
            bytes_sent: 0,
            view_only: false,
        };
        self.list.lock().unwrap().push(Entry { info: info.clone(), stream });
        info
    }

    pub fn remove(&self, id: u64) {
        self.list.lock().unwrap().retain(|e| e.info.id != id);
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        self.list.lock().unwrap().iter().map(|e| e.info.clone()).collect()
    }

    pub fn has_device(&self, device_id: &str) -> bool {
        self.list.lock().unwrap().iter().any(|e| e.info.device.id == device_id)
    }

    pub fn add_bytes(&self, id: u64, n: usize) {
        if let Some(e) = self.list.lock().unwrap().iter_mut().find(|e| e.info.id == id) {
            e.info.bytes_sent += n as u64;
        }
    }

    pub fn is_view_only(&self, id: u64) -> bool {
        self.list.lock().unwrap().iter().any(|e| e.info.id == id && e.info.view_only)
    }

    pub fn set_view_only(&self, id: u64, view_only: bool) -> bool {
        let mut list = self.list.lock().unwrap();
        let Some(e) = list.iter_mut().find(|e| e.info.id == id) else { return false };
        e.info.view_only = view_only;
        true
    }

    /// Closes a session's socket; its threads notice and clean up.
    pub fn disconnect(&self, id: u64) -> bool {
        let list = self.list.lock().unwrap();
        let Some(e) = list.iter().find(|e| e.info.id == id) else { return false };
        let _ = e.stream.shutdown(Shutdown::Both);
        true
    }

    pub fn disconnect_device(&self, device_id: &str) {
        for e in self.list.lock().unwrap().iter().filter(|e| e.info.device.id == device_id) {
            let _ = e.stream.shutdown(Shutdown::Both);
        }
    }
}
