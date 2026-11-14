//! Pairing, device tokens and revocation.
//!
//! A pairing code is short lived and one-use. The token returned to a phone is
//! random; only its SHA-256 hash is stored. Every request after pairing is
//! authenticated by that token over the encrypted iroh connection, and a
//! revoked device cannot reconnect.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::broadcast;

const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const PAIR_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_ATTEMPTS: u32 = 5;
const MAX_NAME: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub token_sha256: String,
    #[serde(default)]
    pub endpoint_id: Option<String>,
    pub created_ms: u64,
    pub last_seen_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DevicesFile {
    #[serde(default)]
    bridge_id: String,
    devices: Vec<Device>,
}

pub struct Auth {
    file: PathBuf,
    devices: Mutex<DevicesFile>,
    pairing: Mutex<Pairing>,
    revocations: broadcast::Sender<String>,
    /// Fires whenever the pairing code rotates, so the desktop stops showing
    /// a code a phone has already used.
    pairing_changes: broadcast::Sender<()>,
}

struct Pairing {
    code: String,
    expires: SystemTime,
    attempts: u32,
}

#[derive(Debug)]
pub struct Paired {
    pub token: String,
    pub device: Device,
}

impl Auth {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        restrict_dir(dir)?;
        let file = dir.join("devices.json");
        let mut devices: DevicesFile = if file.exists() {
            serde_json::from_slice(&std::fs::read(&file)?)
                .with_context(|| format!("invalid {}", file.display()))?
        } else {
            DevicesFile::default()
        };
        if devices.bridge_id.is_empty() {
            devices.bridge_id = random_id();
            write_private(&file, &serde_json::to_vec_pretty(&devices)?)?;
        }
        let (revocations, _) = broadcast::channel(64);
        let (pairing_changes, _) = broadcast::channel(8);
        let auth = Self {
            file,
            devices: Mutex::new(devices),
            pairing: Mutex::new(Pairing {
                code: String::new(),
                expires: UNIX_EPOCH,
                attempts: 0,
            }),
            revocations,
            pairing_changes,
        };
        auth.rotate_code()?;
        Ok(auth)
    }

    pub fn bridge_id(&self) -> String {
        self.devices
            .lock()
            .expect("devices mutex poisoned")
            .bridge_id
            .clone()
    }

    /// The code to show. A code whose time is up is replaced here, so the
    /// desktop never offers a code that already expired.
    pub fn code(&self) -> String {
        let mut pairing = self.pairing.lock().expect("pairing mutex poisoned");
        if pairing.code.is_empty() {
            return String::new();
        }
        let expired = SystemTime::now() > pairing.expires;
        if expired {
            reset_pairing(&mut pairing);
        }
        let code = pairing.code.clone();
        drop(pairing);
        if expired {
            let _ = self.pairing_changes.send(());
        }
        code
    }

    /// When the current code stops working, for the Mac's pairing card.
    pub fn pairing_expires_ms(&self) -> u64 {
        ms(self.pairing.lock().expect("pairing mutex poisoned").expires)
    }

    /// A fresh one-use code. Called at startup and from the Mac settings UI.
    pub fn rotate_code(&self) -> Result<String> {
        let mut pairing = self.pairing.lock().expect("pairing mutex poisoned");
        reset_pairing(&mut pairing);
        let code = pairing.code.clone();
        drop(pairing);
        let _ = self.pairing_changes.send(());
        Ok(code)
    }

    /// Notified after every rotation, including the one a successful pair does.
    pub fn subscribe_pairing(&self) -> broadcast::Receiver<()> {
        self.pairing_changes.subscribe()
    }

    pub fn pair(&self, presented: &str, name: &str, endpoint_id: &str) -> Result<Paired> {
        let name = clean_name(name)?;
        let mut pairing = self.pairing.lock().expect("pairing mutex poisoned");
        if pairing.code.is_empty() || SystemTime::now() > pairing.expires {
            // A fresh code is shown right behind this failure; leaving the
            // expired one in place would make pairing impossible until a
            // restart.
            reset_pairing(&mut pairing);
            drop(pairing);
            let _ = self.pairing_changes.send(());
            bail!("pairing code expired");
        }
        if pairing.attempts >= MAX_ATTEMPTS {
            reset_pairing(&mut pairing);
            drop(pairing);
            let _ = self.pairing_changes.send(());
            bail!("too many pairing attempts");
        }
        let expected = normalize_code(&pairing.code).into_bytes();
        let got = normalize_code(presented).into_bytes();
        if expected.len() != got.len() || expected.ct_eq(&got).unwrap_u8() != 1 {
            pairing.attempts += 1;
            // The last wrong guess spends the code: rotate rather than leave a
            // code that can no longer pair.
            let spent = pairing.attempts >= MAX_ATTEMPTS;
            if spent {
                reset_pairing(&mut pairing);
            }
            drop(pairing);
            if spent {
                let _ = self.pairing_changes.send(());
            }
            bail!("wrong pairing code");
        }
        // A code is one-use: rotate immediately after a successful pair so the
        // Mac can show the next device's code without a restart.
        reset_pairing(&mut pairing);
        drop(pairing);
        let _ = self.pairing_changes.send(());

        let token = random_token();
        let device = Device {
            id: random_id(),
            name,
            token_sha256: hash_token(&token),
            endpoint_id: Some(endpoint_id.to_owned()),
            created_ms: now_ms(),
            last_seen_ms: now_ms(),
        };
        {
            let mut devices = self.devices.lock().expect("devices mutex poisoned");
            devices.devices.push(device.clone());
            if let Err(error) = write_private(&self.file, &serde_json::to_vec_pretty(&*devices)?) {
                devices.devices.pop();
                return Err(error);
            }
        }
        Ok(Paired { token, device })
    }

    pub fn authenticate(&self, token: &str, endpoint_id: &str) -> Option<Device> {
        let hash = hash_token(token);
        let mut devices = self.devices.lock().expect("devices mutex poisoned");
        let device = devices
            .devices
            .iter()
            .find(|device| {
                device.endpoint_id.as_deref() == Some(endpoint_id)
                    && device
                        .token_sha256
                        .as_bytes()
                        .ct_eq(hash.as_bytes())
                        .unwrap_u8()
                        == 1
            })
            .cloned()?;
        let now = now_ms();
        if let Some(record) = devices
            .devices
            .iter_mut()
            .find(|record| record.id == device.id)
        {
            record.last_seen_ms = now;
        }
        let _ = write_private(&self.file, &serde_json::to_vec_pretty(&*devices).ok()?);
        Some(device)
    }

    /// Every paired device. Kept for the Mac settings list, which does not
    /// exist yet, so nothing calls this today.
    #[allow(dead_code)]
    pub fn devices(&self) -> Vec<Device> {
        self.devices
            .lock()
            .expect("devices mutex poisoned")
            .devices
            .clone()
    }

    /// Drop a paired device and cut off its reconnects. Only tests call this
    /// until the settings list can revoke a device.
    #[allow(dead_code)]
    pub fn revoke(&self, id: &str) -> Result<()> {
        let mut devices = self.devices.lock().expect("devices mutex poisoned");
        let before = devices.devices.len();
        devices.devices.retain(|device| device.id != id);
        if devices.devices.len() == before {
            bail!("unknown device");
        }
        write_private(&self.file, &serde_json::to_vec_pretty(&*devices)?)?;
        let _ = self.revocations.send(id.to_owned());
        Ok(())
    }

    pub fn subscribe_revocations(&self) -> broadcast::Receiver<String> {
        self.revocations.subscribe()
    }

    /// True when this device still exists; a missed revocation event must not
    /// leave a stale connection authorized.
    pub fn is_current(&self, id: &str) -> bool {
        self.devices
            .lock()
            .expect("devices mutex poisoned")
            .devices
            .iter()
            .any(|device| device.id == id)
    }
}

fn clean_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name.len() > MAX_NAME || name.chars().any(char::is_control) {
        bail!("invalid device name");
    }
    Ok(name.to_owned())
}

fn reset_pairing(pairing: &mut Pairing) {
    let mut code = String::with_capacity(9);
    let mut rng = rand::rng();
    let mut bytes = [0u8; 8];
    rng.fill_bytes(&mut bytes);
    for (index, byte) in bytes.iter().enumerate() {
        if index == 4 {
            code.push('-');
        }
        code.push(CODE_ALPHABET[(*byte as usize) % CODE_ALPHABET.len()] as char);
    }
    pairing.code = code;
    pairing.expires = SystemTime::now() + PAIR_TTL;
    pairing.attempts = 0;
}

fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_uppercase)
        .collect()
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn random_id() -> String {
    let mut bytes = [0u8; 12];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn hash_token(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn restrict_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    use std::io::Write as _;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kumu-remote-auth-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_code_is_one_use_and_bound_to_the_peer() {
        let dir = temp_dir("pair");
        let auth = Auth::open(&dir).unwrap();
        let code = auth.code();
        let paired = auth.pair(&code, "iPhone", "peer-a").unwrap();
        assert_ne!(auth.code(), code, "a successful pair rotates the code");
        assert!(auth.pair(&code, "iPhone", "peer-a").is_err());
        assert!(auth.authenticate(&paired.token, "peer-b").is_none());
        assert_eq!(
            auth.authenticate(&paired.token, "peer-a").unwrap().id,
            paired.device.id
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_revoked_device_cannot_authenticate() {
        let dir = temp_dir("revoke");
        let auth = Auth::open(&dir).unwrap();
        let code = auth.code();
        let paired = auth.pair(&code, "iPhone", "peer").unwrap();
        auth.revoke(&paired.device.id).unwrap();
        assert!(auth.authenticate(&paired.token, "peer").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_expired_code_is_replaced_instead_of_staying_dead() {
        let dir = temp_dir("expire");
        let auth = Auth::open(&dir).unwrap();
        let stale = auth.code();
        auth.pairing.lock().unwrap().expires = SystemTime::now() - Duration::from_secs(1);
        let fresh = auth.code();
        assert_ne!(fresh, stale, "reading an expired code replaces it");
        assert!(auth.pair(&stale, "iPhone", "peer").is_err());
        let fresh = auth.code();
        assert!(auth.pair(&fresh, "iPhone", "peer").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_last_wrong_guess_replaces_the_code() {
        let dir = temp_dir("attempts");
        let auth = Auth::open(&dir).unwrap();
        let stale = auth.code();
        for _ in 0..MAX_ATTEMPTS {
            assert!(auth.pair("WRONG", "iPhone", "peer").is_err());
        }
        let fresh = auth.code();
        assert_ne!(fresh, stale, "a spent code is replaced");
        assert!(auth.pair(&stale, "iPhone", "peer").is_err());
        assert!(auth.pair(&fresh, "iPhone", "peer").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
