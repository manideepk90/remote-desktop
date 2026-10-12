//! Saved connections. Passwords are never written here; see [`crate::secrets`].

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scale {
    /// Shrink or grow the remote screen to fit the window.
    #[default]
    Fit,
    /// One remote pixel per screen pixel, with scrollbars.
    Actual,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub remember_password: bool,
    /// A password for this connection is in the system credential store.
    pub password_saved: bool,
    pub view_only: bool,
    pub scale: Scale,
    /// Unix seconds of the last successful connection.
    pub last_used: u64,
}

impl Profile {
    pub fn new_id() -> String {
        let n: u64 = rand::random();
        format!("{n:016x}")
    }

    pub fn title(&self) -> String {
        if self.name.trim().is_empty() { self.address() } else { self.name.trim().to_string() }
    }

    /// "host" or "host:port", the way it is typed in the address box.
    pub fn address(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.port == 5900 { host } else { format!("{host}:{}", self.port) }
    }

    pub fn touch(&mut self) {
        self.last_used = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    }
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    connections: Vec<Profile>,
}

pub struct Store {
    path: Option<PathBuf>,
    pub profiles: Vec<Profile>,
}

pub fn config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "Remote Desk Viewer").map(|d| d.config_dir().to_path_buf())
}

impl Store {
    /// Loads saved connections. A missing or unreadable file starts an empty list
    /// (an unreadable one is kept aside rather than overwritten).
    pub fn load() -> Store {
        let path = config_dir().map(|d| d.join("connections.json"));
        let profiles = path.as_ref().map_or_else(Vec::new, |p| match std::fs::read(p) {
            Ok(bytes) => match serde_json::from_slice::<File>(&bytes) {
                Ok(f) => f.connections,
                Err(e) => {
                    log::warn!("{}: {e}; starting with no saved connections", p.display());
                    let _ = std::fs::rename(p, p.with_extension("json.bad"));
                    Vec::new()
                }
            },
            Err(_) => Vec::new(),
        });
        Store { path, profiles }
    }

    #[cfg(test)]
    pub fn at(path: PathBuf) -> Store {
        Store { path: Some(path), profiles: Vec::new() }
    }

    pub fn save(&self) -> Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let json = serde_json::to_vec_pretty(&File { connections: self.profiles.clone() })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|p| p.id == id)
    }

    /// Adds the profile, or replaces the one with the same id.
    pub fn upsert(&mut self, profile: Profile) {
        match self.get_mut(&profile.id) {
            Some(p) => *p = profile,
            None => self.profiles.push(profile),
        }
    }

    pub fn remove(&mut self, id: &str) -> Option<Profile> {
        let i = self.profiles.iter().position(|p| p.id == id)?;
        Some(self.profiles.remove(i))
    }

    /// An existing connection to the same place with the same account.
    pub fn find_same(&self, host: &str, port: u16, username: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.host.eq_ignore_ascii_case(host) && p.port == port && p.username == username)
    }

    /// Most recently used first, then by name.
    pub fn sorted(&self) -> Vec<&Profile> {
        let mut v: Vec<&Profile> = self.profiles.iter().collect();
        v.sort_by(|a, b| {
            b.last_used.cmp(&a.last_used).then_with(|| a.title().to_lowercase().cmp(&b.title().to_lowercase()))
        });
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_never_contains_passwords() {
        let dir = std::env::temp_dir().join(format!("rdv-test-{}", Profile::new_id()));
        let path = dir.join("connections.json");
        let mut s = Store::at(path.clone());
        s.upsert(Profile {
            id: "a".into(),
            host: "10.0.0.2".into(),
            port: 5900,
            username: "me".into(),
            password_saved: true,
            ..Default::default()
        });
        s.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.to_lowercase().contains("\"password\""));
        let f: File = serde_json::from_str(&text).unwrap();
        assert_eq!(f.connections, s.profiles);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn addresses_and_titles() {
        let mut p = Profile { host: "fe80::1".into(), port: 5901, ..Default::default() };
        assert_eq!(p.address(), "[fe80::1]:5901");
        p.host = "pc".into();
        p.port = 5900;
        assert_eq!(p.title(), "pc");
        p.name = " Office ".into();
        assert_eq!(p.title(), "Office");
    }

    #[test]
    fn sorted_by_recent_use() {
        let mut s = Store::at(PathBuf::from("unused"));
        s.upsert(Profile { id: "1".into(), name: "b".into(), last_used: 5, ..Default::default() });
        s.upsert(Profile { id: "2".into(), name: "a".into(), last_used: 9, ..Default::default() });
        s.upsert(Profile { id: "3".into(), name: "c".into(), last_used: 9, ..Default::default() });
        let ids: Vec<&str> = s.sorted().iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["2", "3", "1"]);
    }
}
