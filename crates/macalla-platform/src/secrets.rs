// SPDX-License-Identifier: MIT
//! A secret stored under an account name -- an API key, kept in the OS
//! keychain rather than a file.
//!
//! Cloud engine keys live in the OS keychain and nowhere else. They live
//! behind `macalla-platform`, which already owns the per-OS details, and an
//! adapter takes the key as a value and never reads the keychain itself --
//! this module is where that reading and writing happens, once.
//!
//! Follows [`crate::paste`]'s shape: a small trait for the capability, a real
//! implementation behind it, and a public in-memory one so other crates'
//! tests never need a real keychain. The real one uses `keyring-core` with the
//! platform's own store: the login keychain on macOS, the Credential Manager on
//! Windows. **Linux has no store yet** (parked on 2026-09-11): the Secret
//! Service store pulls in a D-Bus stack and an async runtime, and [`SystemSecrets`]
//! there reports the store as unavailable, so a key comes from the environment.
//!
//! **No secret ever appears in an error, in `Debug`, or in `Display`.** Every
//! error variant here carries only the account name and a plain-English
//! reason from the store, never the value that was stored.

use std::collections::HashMap;
use std::sync::Mutex;

/// The service name under which every secret is stored. One name, shared by
/// the CLI and the desktop app, so a key entered in one is usable from the
/// other.
pub const SERVICE: &str = "dev.macalla";

/// A place secrets are stored and retrieved by account name.
pub trait SecretStore {
    /// The secret under `account`, or `None` if there is no entry.
    fn get(&self, account: &str) -> crate::Result<Option<String>>;

    /// Store `secret` under `account`, replacing whatever was there.
    fn set(&self, account: &str, secret: &str) -> crate::Result<()>;

    /// Remove the entry for `account`.
    ///
    /// Deleting an entry that is not there is `Ok(())`: the caller wants the
    /// secret gone, and it already is, so a missing entry is not a reason to
    /// fail.
    fn delete(&self, account: &str) -> crate::Result<()>;
}

/// The OS credential store: the login keychain on macOS, the Credential
/// Manager on Windows, and none yet on Linux (see the module note).
///
/// Cheap to construct -- the store is opened once per process, on first use,
/// so there is nothing to fail here. Where there is no store, every call
/// reports [`Error::SecretStoreUnavailable`](crate::Error::SecretStoreUnavailable).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemSecrets;

impl SystemSecrets {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl SystemSecrets {
    fn entry(account: &str) -> crate::Result<keyring_core::Entry> {
        store_opened()
            .as_ref()
            .map_err(|reason| crate::Error::SecretStoreUnavailable(reason.clone()))?;
        keyring_core::Entry::new(SERVICE, account).map_err(|e| classify(account, e))
    }
}

/// Opens the platform's store and makes it `keyring-core`'s default, once per
/// process; later calls get the first attempt's result. The same thing the
/// `keyring` crate's v1 wrapper does, for only the two stores used here.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn store_opened() -> &'static std::result::Result<(), String> {
    use std::sync::LazyLock;
    static OPENED: LazyLock<std::result::Result<(), String>> = LazyLock::new(|| {
        #[cfg(target_os = "macos")]
        let store = apple_native_keyring_store::keychain::Store::new();
        #[cfg(target_os = "windows")]
        let store = windows_native_keyring_store::Store::new();
        match store {
            Ok(store) => {
                keyring_core::set_default_store(store);
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    });
    &OPENED
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl SecretStore for SystemSecrets {
    fn get(&self, account: &str) -> crate::Result<Option<String>> {
        let entry = Self::entry(account)?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(classify(account, e)),
        }
    }

    fn set(&self, account: &str, secret: &str) -> crate::Result<()> {
        let entry = Self::entry(account)?;
        entry.set_password(secret).map_err(|e| classify(account, e))
    }

    fn delete(&self, account: &str) -> crate::Result<()> {
        let entry = Self::entry(account)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(classify(account, e)),
        }
    }
}

/// What a platform without a store says, for every call.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const NO_STORE: &str =
    "Macalla keeps no keys in a keychain on this platform yet; give the key in an environment variable";

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl SecretStore for SystemSecrets {
    fn get(&self, _account: &str) -> crate::Result<Option<String>> {
        Err(crate::Error::SecretStoreUnavailable(NO_STORE.into()))
    }

    fn set(&self, _account: &str, _secret: &str) -> crate::Result<()> {
        Err(crate::Error::SecretStoreUnavailable(NO_STORE.into()))
    }

    fn delete(&self, _account: &str) -> crate::Result<()> {
        Err(crate::Error::SecretStoreUnavailable(NO_STORE.into()))
    }
}

/// Sort a store error into the two shapes the rest of the app can act on.
///
/// `.to_string()` only -- never `{e:?}`. `Error::BadEncoding` carries the raw,
/// possibly-secret bytes it failed to decode as UTF-8 in its `Debug` output;
/// its `Display` does not.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn classify(account: &str, e: keyring_core::Error) -> crate::Error {
    match e {
        keyring_core::Error::NoStorageAccess(_) => {
            crate::Error::SecretStoreUnavailable(e.to_string())
        }
        keyring_core::Error::NoDefaultStore => crate::Error::SecretStoreUnavailable(
            "the platform's credential store could not be opened".into(),
        ),
        other => crate::Error::Secret {
            account: account.to_string(),
            reason: other.to_string(),
        },
    }
}

/// An in-memory [`SecretStore`], for tests. Public so `macalla-session` and
/// anything else that stores secrets through this trait can test against it
/// without a real keychain -- CI also runs headless on Linux, which has no
/// store here.
#[derive(Default)]
pub struct MemoryStore {
    entries: Mutex<HashMap<String, String>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Hand-written, not derived: `std::sync::Mutex<T>`'s `Debug` impl prints the
/// guarded value when the lock is uncontended, which would print every
/// stored secret verbatim. Show which accounts have an entry, never the
/// values -- the same redaction discipline as [`classify`]'s use of
/// `Display`-only on `keyring::Error`.
impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let accounts: Vec<String> = match self.entries.lock() {
            Ok(guard) => guard.keys().cloned().collect(),
            Err(poisoned) => poisoned.get_ref().keys().cloned().collect(),
        };
        f.debug_struct("MemoryStore")
            .field("accounts", &accounts)
            .finish()
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, account: &str) -> crate::Result<Option<String>> {
        Ok(self.entries.lock().unwrap().get(account).cloned())
    }

    fn set(&self, account: &str, secret: &str) -> crate::Result<()> {
        self.entries
            .lock()
            .unwrap()
            .insert(account.to_string(), secret.to_string());
        Ok(())
    }

    fn delete(&self, account: &str) -> crate::Result<()> {
        self.entries.lock().unwrap().remove(account);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_entry_is_ok_none_not_an_error() {
        let store = MemoryStore::new();
        assert_eq!(store.get("openai").unwrap(), None);
    }

    #[test]
    fn a_stored_secret_is_read_back() {
        let store = MemoryStore::new();
        store.set("openai", "sk-test-key").unwrap();
        assert_eq!(store.get("openai").unwrap().as_deref(), Some("sk-test-key"));
    }

    #[test]
    fn setting_again_replaces_the_old_value() {
        let store = MemoryStore::new();
        store.set("openai", "first").unwrap();
        store.set("openai", "second").unwrap();
        assert_eq!(store.get("openai").unwrap().as_deref(), Some("second"));
    }

    #[test]
    fn deleting_removes_it() {
        let store = MemoryStore::new();
        store.set("openai", "sk-test-key").unwrap();
        store.delete("openai").unwrap();
        assert_eq!(store.get("openai").unwrap(), None);
    }

    #[test]
    fn deleting_a_missing_entry_is_ok() {
        let store = MemoryStore::new();
        assert!(store.delete("never-stored").is_ok());
    }

    #[test]
    fn debug_output_never_contains_the_secret_value() {
        let store = MemoryStore::new();
        store.set("openai", "sk-super-secret-value").unwrap();
        let debug = format!("{store:?}");
        assert!(
            !debug.contains("sk-super-secret-value"),
            "Debug output leaked a stored secret: {debug}"
        );
        // The account name is fine to show -- it is not the secret.
        assert!(debug.contains("openai"));
    }

    #[test]
    fn accounts_do_not_leak_into_each_other() {
        let store = MemoryStore::new();
        store.set("openai", "openai-key").unwrap();
        store.set("anthropic", "anthropic-key").unwrap();
        assert_eq!(store.get("openai").unwrap().as_deref(), Some("openai-key"));
        assert_eq!(
            store.get("anthropic").unwrap().as_deref(),
            Some("anthropic-key")
        );
        store.delete("openai").unwrap();
        assert_eq!(store.get("openai").unwrap(), None);
        assert_eq!(
            store.get("anthropic").unwrap().as_deref(),
            Some("anthropic-key")
        );
    }

    /// Where there is no store, every call says so rather than failing
    /// quietly or pretending to have stored something.
    #[test]
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn without_a_store_every_call_says_so() {
        let store = SystemSecrets::new();
        assert!(matches!(
            store.get("openai"),
            Err(crate::Error::SecretStoreUnavailable(_))
        ));
        assert!(matches!(
            store.set("openai", "x"),
            Err(crate::Error::SecretStoreUnavailable(_))
        ));
        assert!(matches!(
            store.delete("openai"),
            Err(crate::Error::SecretStoreUnavailable(_))
        ));
    }

    /// Needs the real keychain or Credential Manager, so it never runs in CI.
    ///
    /// ```text
    /// cargo test -p macalla-platform --lib -- --ignored --nocapture round_trips_through_the_real_keychain
    /// ```
    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[ignore = "needs the real OS keychain or Credential Manager"]
    fn round_trips_through_the_real_keychain() {
        let store = SystemSecrets::new();
        let account = "macalla-secrets-test";
        // Leave no trace, whatever happens.
        let _ = store.delete(account);

        assert_eq!(store.get(account).unwrap(), None);
        store.set(account, "round-trip-value").unwrap();
        assert_eq!(
            store.get(account).unwrap().as_deref(),
            Some("round-trip-value")
        );
        store.delete(account).unwrap();
        assert_eq!(store.get(account).unwrap(), None);
    }
}
