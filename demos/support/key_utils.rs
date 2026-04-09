//! Shared helper: load a persisted iroh `SecretKey` from the XDG cache
//! directory, or generate a fresh one and save it for next time.

use std::path::PathBuf;

use iroh::SecretKey;

pub fn cache_dir(app_name: &str) -> PathBuf {
    let base = if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(dir)
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".cache")
    } else {
        PathBuf::from(".")
    };
    base.join(app_name)
}

#[allow(dead_code, clippy::collapsible_if)]
pub fn load_or_generate_key(app_name: &str) -> SecretKey {
    load_or_generate_key_in(&cache_dir(app_name))
}

#[allow(clippy::collapsible_if)]
pub fn load_or_generate_key_in(dir: &std::path::Path) -> SecretKey {
    let path = dir.join("private.key");
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) {
            let key = SecretKey::from(arr);
            println!("Loaded key from {}", path.display());
            return key;
        }
    }
    let key = SecretKey::generate(&mut rand::rng());
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, key.to_bytes()) {
        eprintln!("Warning: could not save key to {}: {e}", path.display());
    } else {
        println!("Generated new key, saved to {}", path.display());
    }
    key
}
