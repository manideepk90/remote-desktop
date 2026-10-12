//! Passwords live in the operating system's credential store: Windows Credential
//! Manager, the macOS Keychain, or the Secret Service (GNOME Keyring, KWallet) on Linux.

use keyring::{Entry, Error};

const SERVICE: &str = "remote-desk-viewer";

fn entry(profile_id: &str) -> Result<Entry, String> {
    Entry::new(SERVICE, profile_id).map_err(describe)
}

fn describe(e: Error) -> String {
    match e {
        Error::NoDefaultStore | Error::NoStorageAccess(_) | Error::PlatformFailure(_) => {
            format!("The system password store is not available ({e}).")
        }
        e => e.to_string(),
    }
}

/// Name of the store, for labels like "Saved in Windows Credential Manager".
pub fn store_name() -> &'static str {
    if cfg!(windows) {
        "Windows Credential Manager"
    } else if cfg!(target_os = "macos") {
        "macOS Keychain"
    } else {
        "the system keyring"
    }
}

pub fn save(profile_id: &str, password: &str) -> Result<(), String> {
    entry(profile_id)?.set_password(password).map_err(describe)
}

pub fn load(profile_id: &str) -> Result<Option<String>, String> {
    match entry(profile_id)?.get_password() {
        Ok(p) => Ok(Some(p)),
        Err(Error::NoEntry) => Ok(None),
        Err(e) => Err(describe(e)),
    }
}

pub fn delete(profile_id: &str) -> Result<(), String> {
    match entry(profile_id)?.delete_credential() {
        Ok(()) | Err(Error::NoEntry) => Ok(()),
        Err(e) => Err(describe(e)),
    }
}
