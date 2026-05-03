//! Keychain wrapper for storing host passwords on macOS.
//!
//! Passwords for `AuthMethod::Password` hosts live in the macOS
//! Keychain — they never round-trip through Tauri IPC after being
//! saved. The frontend's host editor sends the secret once via
//! `host_save_password`, we hand it to the Keychain, and the secret is
//! only read back inside Rust during `connect_for_host`.
//!
//! Service: `app.helm.host`. Account: the host's UUID, stringified.
//! That gives us per-host scoping (delete a host → delete its
//! password) without the user having to manage Keychain entries by
//! hand.
//!
//! Non-mac builds get a stub that returns an error. Stage 2D will add
//! a Linux/Windows credential store if/when we ship beyond macOS.

use helm_domain::HostId;

const SERVICE: &str = "app.helm.host";

#[cfg(target_os = "macos")]
pub fn set_password(host_id: HostId, password: &str) -> Result<(), String> {
    use security_framework::passwords::set_generic_password;
    let account = host_id.0.to_string();
    set_generic_password(SERVICE, &account, password.as_bytes())
        .map_err(|e| format!("keychain set: {e}"))
}

#[cfg(target_os = "macos")]
pub fn get_password(host_id: HostId) -> Result<String, String> {
    use security_framework::passwords::get_generic_password;
    let account = host_id.0.to_string();
    let bytes =
        get_generic_password(SERVICE, &account).map_err(|e| format!("keychain get: {e}"))?;
    String::from_utf8(bytes).map_err(|e| format!("keychain decode: {e}"))
}

#[cfg(target_os = "macos")]
pub fn delete_password(host_id: HostId) -> Result<(), String> {
    use security_framework::passwords::delete_generic_password;
    let account = host_id.0.to_string();
    match delete_generic_password(SERVICE, &account) {
        Ok(()) => Ok(()),
        // Missing entry isn't an error from the user's perspective —
        // they want it gone, and it's already gone.
        Err(e) if e.code() == -25300 /* errSecItemNotFound */ => Ok(()),
        Err(e) => Err(format!("keychain delete: {e}")),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_password(_host_id: HostId, _password: &str) -> Result<(), String> {
    Err("Keychain support is macOS-only for now".into())
}

#[cfg(not(target_os = "macos"))]
pub fn get_password(_host_id: HostId) -> Result<String, String> {
    Err("Keychain support is macOS-only for now".into())
}

#[cfg(not(target_os = "macos"))]
pub fn delete_password(_host_id: HostId) -> Result<(), String> {
    Ok(())
}
