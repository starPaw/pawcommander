//! Secrets are stored only in the OS credential store, separately from UI settings.
use crate::vfs::Endpoint;
use keyring::api::CredentialStoreApi;
use serde::{Deserialize, Serialize};

const SERVICE: &str = "dev.pawcommander.sftp.v1";
#[derive(Serialize, Deserialize)]
pub struct SavedKey {
    pub private_key: String,
    pub passphrase: String,
}
fn entry(endpoint: &Endpoint) -> Result<keyring::Entry, String> {
    let account = account(endpoint)?;
    // Open a store per operation: a temporarily unavailable service must be
    // retryable without restarting the application or changing global state.
    #[cfg(target_os = "macos")]
    let store = apple_native_keyring_store::keychain::Store::new().map_err(vault_error)?;
    #[cfg(target_os = "windows")]
    let store = windows_native_keyring_store::Store::new().map_err(vault_error)?;
    #[cfg(target_os = "linux")]
    let store = zbus_secret_service_keyring_store::Store::new().map_err(vault_error)?;

    store.build(SERVICE, &account, None).map_err(vault_error)
}
fn account(endpoint: &Endpoint) -> Result<String, String> {
    // Structured identity avoids delimiter collisions and isolates users, ports and keys.
    serde_json::to_string(endpoint)
        .map_err(|_| crate::i18n::text("Не удалось определить профиль SFTP").into())
}
fn vault_error(error: keyring::Error) -> String {
    crate::tr_format!("Системное хранилище секретов недоступно: {error}. Ключ и фраза не записываются в настройки.", "System credential store unavailable: {error}. The key and passphrase are not written to settings.")
}
pub fn load(endpoint: &Endpoint) -> Result<Option<SavedKey>, String> {
    load_entry(&entry(endpoint)?)
}
fn load_entry(entry: &keyring::Entry) -> Result<Option<SavedKey>, String> {
    match entry.get_password() {
        Ok(secret) => serde_json::from_str(&secret).map(Some).map_err(|_| {
            crate::i18n::text("Сохранённый ключ повреждён; удалите его и сохраните заново").into()
        }),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(vault_error(error)),
    }
}
pub fn save(endpoint: &Endpoint, secret: &SavedKey) -> Result<(), String> {
    save_entry(&entry(endpoint)?, secret)
}
fn save_entry(entry: &keyring::Entry, secret: &SavedKey) -> Result<(), String> {
    let encoded = serde_json::to_string(secret)
        .map_err(|_| crate::i18n::text("Не удалось подготовить ключ к сохранению"))?;
    entry.set_password(&encoded).map_err(vault_error)
}
pub fn forget(endpoint: &Endpoint) -> Result<(), String> {
    forget_entry(&entry(endpoint)?)
}
fn forget_entry(entry: &keyring::Entry) -> Result<(), String> {
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(vault_error(error)),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "uses a disposable entry in the real OS credential store"]
    fn os_vault_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let endpoint = Endpoint {
            host: "pawcommander-test.invalid".into(),
            port: 2222,
            user: "disposable-test".into(),
            key: Some(root.path().join("generated-test-key")),
        };
        let result = (|| -> Result<(), String> {
            save(
                &endpoint,
                &SavedKey {
                    private_key: "disposable test material".into(),
                    passphrase: "disposable test phrase".into(),
                },
            )?;
            let restored = load(&endpoint)?.ok_or("Secret missing")?;
            assert_eq!(restored.private_key, "disposable test material");
            assert_eq!(restored.passphrase, "disposable test phrase");
            let other = Endpoint {
                port: 22,
                ..endpoint.clone()
            };
            assert!(load(&other)?.is_none());
            Ok(())
        })();
        let cleanup = forget(&endpoint);
        result.unwrap();
        cleanup.unwrap();
        assert!(load(&endpoint).unwrap().is_none());
    }
    #[test]
    fn vault_round_trip_is_scoped_and_can_be_forgotten() {
        // Keep the mock local: never replace the global store used by native tests.
        let store = keyring::mock::Store::new().unwrap();
        let endpoint = Endpoint {
            host: "test.invalid".into(),
            port: 2222,
            user: "test".into(),
            key: Some("/test/key".into()),
        };
        let entry = store
            .build(SERVICE, &account(&endpoint).unwrap(), None)
            .unwrap();
        assert!(load_entry(&entry).unwrap().is_none());
        save_entry(
            &entry,
            &SavedKey {
                private_key: "disposable test key".into(),
                passphrase: "test phrase".into(),
            },
        )
        .unwrap();
        let loaded = load_entry(&entry).unwrap().unwrap();
        assert_eq!(loaded.private_key, "disposable test key");
        assert_eq!(loaded.passphrase, "test phrase");
        let other = Endpoint {
            port: 22,
            ..endpoint.clone()
        };
        let other = store
            .build(SERVICE, &account(&other).unwrap(), None)
            .unwrap();
        assert!(load_entry(&other).unwrap().is_none());
        forget_entry(&entry).unwrap();
        assert!(load_entry(&entry).unwrap().is_none());
        entry.set_password("invalid JSON").unwrap();
        assert!(load_entry(&entry).is_err());
    }

    #[test]
    fn endpoint_fields_identify_independent_credentials() {
        let store = keyring::mock::Store::new().unwrap();
        let endpoint = Endpoint {
            host: "test.invalid".into(),
            port: 2222,
            user: "test".into(),
            key: Some("/test/key".into()),
        };
        let current = store
            .build(SERVICE, &account(&endpoint).unwrap(), None)
            .unwrap();
        save_entry(
            &current,
            &SavedKey {
                private_key: "saved key".into(),
                passphrase: "saved phrase".into(),
            },
        )
        .unwrap();
        let restored = load_entry(&current).unwrap().unwrap();
        assert_eq!(restored.private_key, "saved key");
        assert_eq!(restored.passphrase, "saved phrase");
        for other in [
            Endpoint {
                host: "other.invalid".into(),
                ..endpoint.clone()
            },
            Endpoint {
                port: 22,
                ..endpoint.clone()
            },
            Endpoint {
                user: "other".into(),
                ..endpoint.clone()
            },
            Endpoint {
                key: Some("/other/key".into()),
                ..endpoint
            },
        ] {
            let other = store
                .build(SERVICE, &account(&other).unwrap(), None)
                .unwrap();
            assert!(load_entry(&other).unwrap().is_none());
        }
    }

    #[test]
    fn temporary_vault_failure_preserves_the_secret_and_allows_retry() {
        let store = keyring::mock::Store::new().unwrap();
        let entry = store.build(SERVICE, "disposable-test", None).unwrap();
        save_entry(
            &entry,
            &SavedKey {
                private_key: "saved key".into(),
                passphrase: "saved phrase".into(),
            },
        )
        .unwrap();
        let mock = entry
            .as_any()
            .downcast_ref::<keyring::mock::Cred>()
            .unwrap();
        let unavailable = || {
            keyring::Error::NoStorageAccess(Box::new(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "test vault locked",
            )))
        };
        mock.set_error(unavailable());
        assert!(load_entry(&entry).is_err());
        mock.set_error(unavailable());
        assert!(forget_entry(&entry).is_err());
        let restored = load_entry(&entry).unwrap().unwrap();
        assert_eq!(restored.private_key, "saved key");
        assert_eq!(restored.passphrase, "saved phrase");
        forget_entry(&entry).unwrap();
        assert!(load_entry(&entry).unwrap().is_none());
    }
}
