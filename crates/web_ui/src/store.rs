use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

const STATE_FILE_NAME: &str = "state.json";
const MAX_SUBSCRIPTIONS: usize = 50;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PushSubscription {
    pub endpoint: String,
    pub keys: PushSubscriptionKeys,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PushSubscriptionKeys {
    pub p256dh: String,
    pub auth: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct StateFile {
    /// Base64url-encoded P-256 private scalar.
    vapid_private_key: String,
    /// Hash of the API secret the subscriptions were made with.
    #[serde(default)]
    secret_sha256: Option<String>,
    #[serde(default)]
    push_subscriptions: Vec<PushSubscription>,
}

pub struct Store {
    path: PathBuf,
    vapid_key: p256::SecretKey,
    secret_sha256: String,
    push_subscriptions: Vec<PushSubscription>,
}

impl Store {
    /// Subscriptions made with a different API secret are dropped, so that
    /// changing the secret also stops notifications to devices that had it.
    pub fn load_or_create(state_dir: &Path, secret: &str) -> Result<Self> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("failed to create state directory {state_dir:?}"))?;
        let path = state_dir.join(STATE_FILE_NAME);
        let secret_sha256 = sha256_hex(secret.as_bytes());

        match std::fs::read(&path) {
            Ok(contents) => match Self::parse(&path, &contents, &secret_sha256) {
                Ok(store) => {
                    store.save()?;
                    return Ok(store);
                }
                Err(error) => {
                    // Starting fresh only costs re-subscribing to
                    // notifications, whereas refusing to start would leave the
                    // user with no way to recover.
                    let backup_path = state_dir.join(format!("{STATE_FILE_NAME}.corrupt"));
                    log::error!(
                        "failed to parse {path:?}, moving it to {backup_path:?} and starting over: {error:#}"
                    );
                    std::fs::rename(&path, &backup_path).with_context(|| {
                        format!("failed to move corrupt state file to {backup_path:?}")
                    })?;
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {path:?}"));
            }
        }

        let store = Self {
            path,
            vapid_key: generate_secret_key(),
            secret_sha256,
            push_subscriptions: Vec::new(),
        };
        store.save()?;
        Ok(store)
    }

    fn parse(path: &Path, contents: &[u8], secret_sha256: &str) -> Result<Self> {
        let state: StateFile = serde_json::from_slice(contents)?;
        let key_bytes = URL_SAFE_NO_PAD
            .decode(state.vapid_private_key.as_bytes())
            .context("invalid VAPID key encoding")?;
        let vapid_key = p256::SecretKey::from_slice(&key_bytes).context("invalid VAPID key")?;
        let push_subscriptions = if state.secret_sha256.as_deref() == Some(secret_sha256) {
            state.push_subscriptions
        } else {
            if !state.push_subscriptions.is_empty() {
                log::info!("the API secret changed, dropping push subscriptions");
            }
            Vec::new()
        };
        Ok(Self {
            path: path.to_path_buf(),
            vapid_key,
            secret_sha256: secret_sha256.to_string(),
            push_subscriptions,
        })
    }

    pub fn save(&self) -> Result<()> {
        let state = StateFile {
            vapid_private_key: URL_SAFE_NO_PAD.encode(self.vapid_key.to_bytes()),
            secret_sha256: Some(self.secret_sha256.clone()),
            push_subscriptions: self.push_subscriptions.clone(),
        };
        let contents = serde_json::to_vec_pretty(&state)?;
        write_atomically(&self.path, &contents)
            .with_context(|| format!("failed to write {:?}", self.path))
    }

    pub fn vapid_key(&self) -> &p256::SecretKey {
        &self.vapid_key
    }

    pub fn push_subscriptions(&self) -> &[PushSubscription] {
        &self.push_subscriptions
    }

    pub fn add_push_subscription(&mut self, subscription: PushSubscription) -> Result<()> {
        self.push_subscriptions
            .retain(|existing| existing.endpoint != subscription.endpoint);
        self.push_subscriptions.push(subscription);
        let excess = self
            .push_subscriptions
            .len()
            .saturating_sub(MAX_SUBSCRIPTIONS);
        self.push_subscriptions.drain(..excess);
        self.save()
    }

    /// Returns whether anything was removed.
    pub fn remove_push_subscription(&mut self, endpoint: &str) -> Result<bool> {
        let previous_count = self.push_subscriptions.len();
        self.push_subscriptions
            .retain(|subscription| subscription.endpoint != endpoint);
        if self.push_subscriptions.len() == previous_count {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }
}

fn write_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write as _;

    let mut temporary_path = path.as_os_str().to_owned();
    temporary_path.push(".tmp");
    let temporary_path = PathBuf::from(temporary_path);

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary_path)?;
    #[cfg(unix)]
    {
        // `mode` only applies when the file is created, so tighten the
        // permissions of a leftover temporary file too.
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary_path, path)?;
    Ok(())
}

pub fn generate_secret_key() -> p256::SecretKey {
    loop {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        // Fails only for zero or values above the curve order, which is
        // astronomically unlikely, so simply draw again.
        if let Ok(secret_key) = p256::SecretKey::from_slice(&bytes) {
            return secret_key;
        }
    }
}

#[cfg(test)]
fn random_base64url(byte_count: usize) -> String {
    let mut bytes = vec![0u8; byte_count];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subscription(endpoint: &str) -> PushSubscription {
        PushSubscription {
            endpoint: endpoint.into(),
            keys: PushSubscriptionKeys {
                p256dh: "key".into(),
                auth: "auth".into(),
            },
        }
    }

    #[test]
    fn test_store_round_trip() {
        let state_dir =
            std::env::temp_dir().join(format!("web_ui_store_test_{}", random_base64url(8)));
        let mut store = Store::load_or_create(&state_dir, "secret").unwrap();
        store
            .add_push_subscription(subscription("https://push.example.com/a"))
            .unwrap();

        let reloaded = Store::load_or_create(&state_dir, "secret").unwrap();
        assert_eq!(
            reloaded.vapid_key().to_bytes(),
            store.vapid_key().to_bytes()
        );
        assert_eq!(reloaded.push_subscriptions().len(), 1);
        let contents = std::fs::read_to_string(state_dir.join(STATE_FILE_NAME)).unwrap();
        assert!(!contents.contains("\"secret\""));

        let rotated = Store::load_or_create(&state_dir, "new secret").unwrap();
        assert_eq!(rotated.vapid_key().to_bytes(), store.vapid_key().to_bytes());
        assert!(rotated.push_subscriptions().is_empty());

        std::fs::remove_dir_all(&state_dir).unwrap();
    }
}
