//! Round-trips a password through Windows Credential Manager. Only built on
//! Windows, where CI runs it; elsewhere it would touch the desktop keyring.
#![cfg(windows)]

use remote_desk_viewer::secrets;

#[test]
fn credential_manager_round_trip() {
    let id = format!("selftest-{}", std::process::id());
    assert_eq!(secrets::load(&id), Ok(None));
    secrets::save(&id, "pässwörd 🔑").unwrap();
    assert_eq!(secrets::load(&id), Ok(Some("pässwörd 🔑".to_string())));
    secrets::save(&id, "changed").unwrap();
    assert_eq!(secrets::load(&id), Ok(Some("changed".to_string())));
    secrets::delete(&id).unwrap();
    assert_eq!(secrets::load(&id), Ok(None));
    secrets::delete(&id).unwrap();
}
