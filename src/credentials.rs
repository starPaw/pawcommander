//! Secrets are stored only in the OS credential store, separately from UI settings.
use crate::file_error::{CredentialAction, CredentialError, CredentialFailure, SecretFormatError};
use crate::vfs::Endpoint;
use keyring::api::CredentialStoreApi;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

const SERVICE: &str = "dev.pawcommander.sftp.v1";
#[derive(Serialize, Deserialize)]
pub struct SavedKey {
    pub private_key: String,
    pub passphrase: String,
}

impl Drop for SavedKey {
    fn drop(&mut self) {
        self.private_key.zeroize();
        self.passphrase.zeroize();
    }
}

fn entry(endpoint: &Endpoint) -> Result<keyring::Entry, CredentialFailure> {
    let account = account(endpoint).map_err(CredentialFailure::Profile)?;
    system_entry(SERVICE, &account).map_err(CredentialFailure::from)
}

pub(crate) fn system_entry(service: &str, account: &str) -> Result<keyring::Entry, keyring::Error> {
    // Open a store per operation: a temporarily unavailable service must be
    // retryable without restarting the application or changing global state.
    #[cfg(target_os = "macos")]
    let store = apple_native_keyring_store::keychain::Store::new()?;
    #[cfg(target_os = "windows")]
    let store = windows_native_keyring_store::Store::new()?;
    #[cfg(target_os = "linux")]
    let store = zbus_secret_service_keyring_store::Store::new()?;

    store.build(service, account, None)
}
fn account(endpoint: &Endpoint) -> Result<String, serde_json::Error> {
    // Structured identity avoids delimiter collisions and isolates users, ports and keys.
    serde_json::to_string(endpoint)
}
pub(crate) fn load(endpoint: &Endpoint) -> Result<Option<SavedKey>, CredentialError> {
    entry(endpoint)
        .and_then(|entry| load_entry(&entry))
        .map_err(|cause| CredentialError::new(endpoint, CredentialAction::Read, cause))
}
fn load_entry(entry: &keyring::Entry) -> Result<Option<SavedKey>, CredentialFailure> {
    match entry.get_secret() {
        Ok(secret) => decode_saved_key(&Zeroizing::new(secret))
            .map(Some)
            .map_err(|error| CredentialFailure::Malformed(Some(error))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn decode_saved_key(secret: &[u8]) -> Result<SavedKey, SecretFormatError> {
    serde_json::from_slice(secret)
        .or_else(|error| {
            // Previous Windows entries used the password API's UTF-16LE encoding.
            // Decode the same read so a concurrent update cannot mix two values.
            if !secret.starts_with(b"{\0") || !secret.len().is_multiple_of(2) {
                return Err(error);
            }
            let words = Zeroizing::new(
                secret
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_le_bytes(*pair))
                    .collect::<Vec<_>>(),
            );
            let text = Zeroizing::new(String::from_utf16(&words).map_err(|_| error)?);
            serde_json::from_str(&text)
        })
        .map_err(SecretFormatError::from)
}
pub(crate) fn save(endpoint: &Endpoint, secret: &SavedKey) -> Result<(), CredentialError> {
    entry(endpoint)
        .and_then(|entry| save_entry(&entry, secret))
        .map_err(|cause| CredentialError::new(endpoint, CredentialAction::Save, cause))
}
fn save_entry(entry: &keyring::Entry, secret: &SavedKey) -> Result<(), CredentialFailure> {
    // Keep JSON as UTF-8 bytes: Windows doubles ASCII password strings to
    // UTF-16, unnecessarily exceeding its credential-blob limit for RSA keys.
    let mut encoded = Zeroizing::new(Vec::with_capacity(128));
    serde_json::to_writer(&mut *encoded, secret)
        .map_err(|error| CredentialFailure::Prepare(error.into()))?;
    entry.set_secret(&encoded).map_err(CredentialFailure::from)
}
pub(crate) fn forget(endpoint: &Endpoint) -> Result<(), CredentialError> {
    entry(endpoint)
        .and_then(|entry| forget_entry(&entry))
        .map_err(|cause| CredentialError::new(endpoint, CredentialAction::Forget, cause))
}
fn forget_entry(entry: &keyring::Entry) -> Result<(), CredentialFailure> {
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.into()),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    #[test]
    #[ignore = "uses a disposable entry in the real OS credential store"]
    fn os_vault_round_trip() {
        fn check_saved(
            endpoint: &Endpoint,
            expected: &SavedKey,
        ) -> Result<(), Box<dyn std::error::Error>> {
            let restored = load(endpoint)?.ok_or("Secret missing")?;
            if restored.private_key != expected.private_key
                || restored.passphrase != expected.passphrase
            {
                return Err("Saved secret did not round trip".into());
            }
            Ok(())
        }
        let root = tempfile::tempdir().unwrap();
        let endpoint = Endpoint {
            host: "pawcommander-test.invalid".into(),
            port: 2222,
            user: "disposable-test".into(),
            key: Some(root.path().join("generated-test-key")),
            jump: None,
        };
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let small = SavedKey {
                private_key: "disposable test material".into(),
                passphrase: "disposable test phrase".into(),
            };
            save(&endpoint, &small)?;
            check_saved(&endpoint, &small)?;
            #[cfg(target_os = "windows")]
            {
                let previous = serde_json::to_string(&small).unwrap();
                entry(&endpoint)?.set_password(&previous)?;
                check_saved(&endpoint, &small)?;
            }
            // Exercise a key-sized payload as well as the small smoke entry.
            // Some native stores encode password strings differently from bytes.
            let large = SavedKey {
                private_key: "disposable test key material\n".repeat(64),
                passphrase: "одноразовая тестовая фраза".into(),
            };
            save(&endpoint, &large)?;
            check_saved(&endpoint, &large)?;
            #[cfg(target_os = "windows")]
            {
                // The same payload fits as UTF-8 but exceeds the Windows limit
                // when the previous password API expands it to UTF-16.
                let previous = serde_json::to_string(&large).unwrap();
                if !matches!(
                    entry(&endpoint)?.set_password(&previous),
                    Err(keyring::Error::TooLong(..))
                ) {
                    return Err("Expected the previous UTF-16 size failure".into());
                }
                check_saved(&endpoint, &large)?;
                let oversized = SavedKey {
                    private_key: "disposable oversized material".repeat(256),
                    passphrase: String::new(),
                };
                if save(&endpoint, &oversized).is_ok() {
                    return Err("Expected the Windows credential size limit".into());
                }
                check_saved(&endpoint, &large)?;
            }
            let other = Endpoint {
                port: 22,
                ..endpoint.clone()
            };
            if load(&other)?.is_some() {
                return Err("Secret leaked between endpoint identities".into());
            }
            Ok(())
        })();
        let cleanup = forget(&endpoint);
        result.unwrap();
        cleanup.unwrap();
        assert!(load(&endpoint).unwrap().is_none());
    }

    #[test]
    fn previous_windows_utf16_json_remains_readable_and_corruption_is_preserved() {
        let store = keyring::mock::Store::new().unwrap();
        let entry = store.build(SERVICE, "previous-format-test", None).unwrap();
        let previous = SavedKey {
            private_key: "previous key\nwith lines".into(),
            passphrase: "фраза 🔑".into(),
        };
        let mut encoded: Vec<_> = serde_json::to_string(&previous)
            .unwrap()
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        entry.set_secret(&encoded).unwrap();
        let loaded = load_entry(&entry).unwrap().unwrap();
        assert_eq!(loaded.private_key, previous.private_key);
        assert_eq!(loaded.passphrase, previous.passphrase);
        encoded.pop();
        entry.set_secret(&encoded).unwrap();
        assert!(load_entry(&entry).is_err());
        assert_eq!(entry.get_secret().unwrap(), encoded);
        save_entry(&entry, &previous).unwrap();
        let loaded = load_entry(&entry).unwrap().unwrap();
        assert_eq!(loaded.private_key, previous.private_key);
        assert_eq!(loaded.passphrase, previous.passphrase);
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
            jump: None,
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
            jump: None,
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
        #[cfg(target_os = "macos")]
        let unavailable = || {
            keyring::Error::NoStorageAccess(Box::new(security_framework::base::Error::from_code(
                -25308,
            )))
        };
        #[cfg(not(target_os = "macos"))]
        let unavailable = || {
            keyring::Error::NoStorageAccess(Box::new(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            )))
        };
        mock.set_error(unavailable());
        let read = load_entry(&entry).err().expect("failed read");
        mock.set_error(unavailable());
        let save = save_entry(
            &entry,
            &SavedKey {
                private_key: "replacement key".into(),
                passphrase: "replacement phrase".into(),
            },
        )
        .unwrap_err();
        mock.set_error(unavailable());
        let forget = forget_entry(&entry).unwrap_err();
        for failure in [read, save, forget] {
            #[cfg(target_os = "macos")]
            assert_eq!(
                failure
                    .source()
                    .and_then(|error| error.downcast_ref::<security_framework::base::Error>())
                    .map(|status| status.code()),
                Some(-25308)
            );
            #[cfg(not(target_os = "macos"))]
            assert!(matches!(failure, CredentialFailure::StoreUnavailable));
        }
        let restored = load_entry(&entry).unwrap().unwrap();
        assert_eq!(restored.private_key, "saved key");
        assert_eq!(restored.passphrase, "saved phrase");
        forget_entry(&entry).unwrap();
        assert!(load_entry(&entry).unwrap().is_none());
    }

    #[test]
    fn malformed_credentials_never_expose_secret_values_in_diagnostics() {
        let marker = "disposable-secret-that-must-not-appear";
        let store = keyring::mock::Store::new().unwrap();
        let entry = store.build(SERVICE, "malformed-test", None).unwrap();
        let malformed = format!(r#"{{"private_key":["{marker}"],"passphrase":"{marker}"}}"#);
        let quoted = format!(r#""{marker}""#);
        let endpoint = Endpoint {
            host: "test.invalid".into(),
            user: "test".into(),
            port: 2222,
            key: Some("/test/key".into()),
            jump: None,
        };
        for record in [malformed.into_bytes(), quoted.into_bytes()] {
            entry.set_secret(&record).unwrap();
            let failure = load_entry(&entry).err().expect("malformed secret");
            assert!(matches!(failure, CredentialFailure::Malformed(Some(_))));
            assert!(failure.source().is_some());
            let error: crate::operations::TransferError =
                CredentialError::new(&endpoint, CredentialAction::Read, failure).into();
            let message = crate::i18n::file_error(&error);
            assert!(message.contains("test@test.invalid · 2222"));
            assert!(message.contains("/test/key"));
            assert!(!message.contains(marker));
            let mut cause: Option<&dyn Error> = Some(&error);
            while let Some(error) = cause {
                assert!(!error.to_string().contains(marker));
                assert!(!format!("{error:?}").contains(marker));
                cause = error.source();
            }
            assert_eq!(entry.get_secret().unwrap(), record);
        }
    }
}
