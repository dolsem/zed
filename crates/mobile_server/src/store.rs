use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

const STATE_FILE_NAME: &str = "state.json";
const MAX_SUBSCRIPTIONS_PER_DEVICE: usize = 10;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub token_sha256: String,
    /// Seconds since the Unix epoch.
    pub created_at: u64,
    #[serde(default)]
    pub push_subscriptions: Vec<PushSubscription>,
}

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
    #[serde(default)]
    devices: Vec<Device>,
}

pub struct Store {
    path: PathBuf,
    vapid_key: p256::SecretKey,
    devices: Vec<Device>,
}

impl Store {
    pub fn load_or_create(state_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("failed to create state directory {state_dir:?}"))?;
        let path = state_dir.join(STATE_FILE_NAME);

        match std::fs::read(&path) {
            Ok(contents) => match Self::parse(&path, &contents) {
                Ok(store) => return Ok(store),
                Err(error) => {
                    // Starting fresh only costs re-pairing, whereas refusing to
                    // start would leave the user with no way to recover.
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
            devices: Vec::new(),
        };
        store.save()?;
        Ok(store)
    }

    fn parse(path: &Path, contents: &[u8]) -> Result<Self> {
        let state: StateFile = serde_json::from_slice(contents)?;
        let key_bytes = URL_SAFE_NO_PAD
            .decode(state.vapid_private_key.as_bytes())
            .context("invalid VAPID key encoding")?;
        let vapid_key = p256::SecretKey::from_slice(&key_bytes).context("invalid VAPID key")?;
        Ok(Self {
            path: path.to_path_buf(),
            vapid_key,
            devices: state.devices,
        })
    }

    pub fn save(&self) -> Result<()> {
        let state = StateFile {
            vapid_private_key: URL_SAFE_NO_PAD.encode(self.vapid_key.to_bytes()),
            devices: self.devices.clone(),
        };
        let contents = serde_json::to_vec_pretty(&state)?;
        write_atomically(&self.path, &contents)
            .with_context(|| format!("failed to write {:?}", self.path))
    }

    pub fn vapid_key(&self) -> &p256::SecretKey {
        &self.vapid_key
    }

    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    pub fn device_for_token(&self, token: &str) -> Option<&Device> {
        let token_sha256 = sha256_hex(token.as_bytes());
        self.devices
            .iter()
            .find(|device| device.token_sha256 == token_sha256)
    }

    /// Registers a new device, returning it along with the bearer token that
    /// authenticates it. Only the token's hash is kept.
    pub fn add_device(&mut self, name: String) -> Result<(Device, String)> {
        let token = random_base64url(32);
        let device = Device {
            id: random_base64url(16),
            name,
            token_sha256: sha256_hex(token.as_bytes()),
            created_at: unix_timestamp(),
            push_subscriptions: Vec::new(),
        };
        self.devices.push(device.clone());
        if let Err(error) = self.save() {
            self.devices.pop();
            return Err(error);
        }
        Ok((device, token))
    }

    pub fn remove_device(&mut self, device_id: &str) -> Result<()> {
        self.devices.retain(|device| device.id != device_id);
        self.save()
    }

    pub fn remove_all_devices(&mut self) -> Result<()> {
        self.devices.clear();
        self.save()
    }

    pub fn add_push_subscription(
        &mut self,
        device_id: &str,
        subscription: PushSubscription,
    ) -> Result<()> {
        let device = self
            .devices
            .iter_mut()
            .find(|device| device.id == device_id)
            .context("device not found")?;
        device
            .push_subscriptions
            .retain(|existing| existing.endpoint != subscription.endpoint);
        device.push_subscriptions.push(subscription);
        let excess = device
            .push_subscriptions
            .len()
            .saturating_sub(MAX_SUBSCRIPTIONS_PER_DEVICE);
        device.push_subscriptions.drain(..excess);
        self.save()
    }

    /// Returns whether anything was removed.
    pub fn remove_push_subscription(&mut self, device_id: &str, endpoint: &str) -> Result<bool> {
        let Some(device) = self
            .devices
            .iter_mut()
            .find(|device| device.id == device_id)
        else {
            return Ok(false);
        };
        let previous_count = device.push_subscriptions.len();
        device
            .push_subscriptions
            .retain(|subscription| subscription.endpoint != endpoint);
        if device.push_subscriptions.len() == previous_count {
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

pub fn random_base64url(byte_count: usize) -> String {
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

    #[test]
    fn test_store_round_trip() {
        let state_dir =
            std::env::temp_dir().join(format!("mobile_server_store_test_{}", random_base64url(8)));
        let mut store = Store::load_or_create(&state_dir).unwrap();
        let (device, token) = store.add_device("Phone".into()).unwrap();
        store
            .add_push_subscription(
                &device.id,
                PushSubscription {
                    endpoint: "https://push.example.com/a".into(),
                    keys: PushSubscriptionKeys {
                        p256dh: "key".into(),
                        auth: "auth".into(),
                    },
                },
            )
            .unwrap();

        let reloaded = Store::load_or_create(&state_dir).unwrap();
        assert_eq!(
            reloaded.vapid_key().to_bytes(),
            store.vapid_key().to_bytes()
        );
        let found = reloaded.device_for_token(&token).unwrap();
        assert_eq!(found.name, "Phone");
        assert_eq!(found.push_subscriptions.len(), 1);
        assert!(reloaded.device_for_token("wrong").is_none());
        assert!(!reloaded.devices()[0].token_sha256.contains(&token));

        std::fs::remove_dir_all(&state_dir).unwrap();
    }
}
