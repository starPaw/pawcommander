//! Secrets are stored only in the OS credential store, separately from UI settings.
use crate::vfs::Endpoint;
use serde::{Deserialize, Serialize};

const SERVICE: &str = "dev.pawcommander.sftp.v1";
#[derive(Serialize, Deserialize)]
pub struct SavedKey {
    pub private_key: String,
    pub passphrase: String,
}
fn entry(endpoint: &Endpoint) -> Result<keyring::Entry, String> {
    // Structured identity avoids delimiter collisions and isolates users, ports and keys.
    let account = serde_json::to_string(endpoint)
        .map_err(|_| crate::i18n::text("Не удалось определить профиль SFTP"))?;
    keyring::Entry::new(SERVICE, &account).map_err(vault_error)
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
        keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        let endpoint = Endpoint {
            host: "test.invalid".into(),
            port: 2222,
            user: "test".into(),
            key: Some("/test/key".into()),
        };
        let entry = entry(&endpoint).unwrap();
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
        forget_entry(&entry).unwrap();
        assert!(load_entry(&entry).unwrap().is_none());
        entry.set_password("invalid JSON").unwrap();
        assert!(load_entry(&entry).is_err());
    }
}
