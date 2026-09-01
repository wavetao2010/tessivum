//! Rust-owned pairing and durable remote device sessions.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{broadcast, Mutex as AsyncMutex},
};
use uuid::Uuid;

const STORE_VERSION: u32 = 1;
const MAX_DEVICES: usize = 256;
const MAX_STORE_BYTES: u64 = 1024 * 1024;
const ACTIVITY_PERSIST_INTERVAL: u64 = 60_000;

#[derive(Clone, Debug)]
pub struct RemoteAccessConfig {
    pub enabled: bool,
    pub trusted_tunnel: bool,
    pub pairing_ttl: Duration,
    pub session_ttl: Duration,
}

impl Default for RemoteAccessConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            trusted_tunnel: false,
            pairing_ttl: Duration::from_secs(5 * 60),
            session_ttl: Duration::from_secs(30 * 24 * 60 * 60),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDeviceDescriptor {
    pub device_id: Uuid,
    pub name: String,
    pub created_at: u64,
    pub last_activity_at: u64,
    pub expires_at: u64,
    pub status: RemoteDeviceStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteDeviceStatus {
    Active,
    Expired,
    Revoked,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteAccessDescription {
    pub enabled: bool,
    pub trusted_tunnel: bool,
    pub revision: u64,
    pub devices: Vec<RemoteDeviceDescriptor>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingToken {
    pub token: String,
    pub expires_at: u64,
}

pub struct IssuedRemoteSession {
    pub device: RemoteDeviceDescriptor,
    pub session_secret: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteDeviceContext {
    pub device_id: Uuid,
    pub expires_at: u64,
}

#[derive(Debug, Error)]
pub enum RemoteAccessError {
    #[error("remote access is disabled")]
    Disabled,
    #[error("remote access requires a trusted TLS tunnel")]
    TlsRequired,
    #[error("remote authentication is required")]
    AuthRequired,
    #[error("remote device session expired")]
    SessionExpired,
    #[error("remote device session was revoked")]
    SessionRevoked,
    #[error("remote device was not found")]
    DeviceNotFound,
    #[error("invalid remote access request: {0}")]
    Invalid(String),
    #[error("remote access persistence failed: {0}")]
    Persistence(String),
    #[error("remote access state is corrupt: {0}")]
    Corrupt(String),
}

impl RemoteAccessError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Disabled => "REMOTE_ACCESS_DISABLED",
            Self::TlsRequired => "REMOTE_TLS_REQUIRED",
            Self::AuthRequired => "REMOTE_AUTH_REQUIRED",
            Self::SessionExpired => "REMOTE_SESSION_EXPIRED",
            Self::SessionRevoked => "REMOTE_SESSION_REVOKED",
            Self::DeviceNotFound => "REMOTE_DEVICE_NOT_FOUND",
            Self::Invalid(_) => "INVALID_REMOTE_ACCESS_REQUEST",
            Self::Persistence(_) | Self::Corrupt(_) => "REMOTE_STATE_UNAVAILABLE",
        }
    }
}

#[derive(Clone)]
pub struct RemoteAccess {
    inner: Arc<RemoteAccessInner>,
}

struct RemoteAccessInner {
    path: PathBuf,
    config: RemoteAccessConfig,
    state: AsyncMutex<RemoteState>,
    disconnects: Mutex<Option<broadcast::Sender<()>>>,
}

#[derive(Clone, Default)]
struct RemoteState {
    revision: u64,
    devices: BTreeMap<Uuid, StoredDevice>,
    pairing: Option<StoredPairing>,
}

#[derive(Clone)]
struct StoredPairing {
    token_hash: [u8; 32],
    expires_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredDevice {
    device_id: Uuid,
    name: String,
    secret_hash: String,
    created_at: u64,
    last_activity_at: u64,
    expires_at: u64,
    revoked_at: Option<u64>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredDocument {
    version: u32,
    revision: u64,
    devices: Vec<StoredDevice>,
}

impl RemoteAccess {
    pub async fn open(
        path: impl Into<PathBuf>,
        config: RemoteAccessConfig,
    ) -> Result<Self, RemoteAccessError> {
        if config.enabled && !config.trusted_tunnel {
            return Err(RemoteAccessError::TlsRequired);
        }
        if config.pairing_ttl.is_zero() || config.session_ttl.is_zero() {
            return Err(RemoteAccessError::Invalid(
                "pairing and session TTLs must be positive".into(),
            ));
        }
        let path = path.into();
        let state = load_state(&path).await?;
        let access = Self {
            inner: Arc::new(RemoteAccessInner {
                path,
                config,
                state: AsyncMutex::new(state),
                disconnects: Mutex::new(None),
            }),
        };
        let expirations = if access.enabled() {
            let state = access.inner.state.lock().await;
            state
                .devices
                .values()
                .filter(|device| device.revoked_at.is_none())
                .map(|device| device.expires_at)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        for expires_at in expirations {
            access.schedule_expiry(expires_at);
        }
        Ok(access)
    }

    pub fn enabled(&self) -> bool {
        self.inner.config.enabled
    }

    pub fn trusted_tunnel(&self) -> bool {
        self.inner.config.trusted_tunnel
    }

    pub fn attach_disconnects(&self, sender: broadcast::Sender<()>) {
        *lock(&self.inner.disconnects) = Some(sender);
    }

    pub async fn describe(&self, only: Option<Uuid>) -> RemoteAccessDescription {
        let now = now_millis();
        let state = self.inner.state.lock().await;
        RemoteAccessDescription {
            enabled: self.enabled(),
            trusted_tunnel: self.trusted_tunnel(),
            revision: state.revision,
            devices: state
                .devices
                .values()
                .filter(|device| only.is_none_or(|device_id| device.device_id == device_id))
                .map(|device| descriptor(device, now))
                .collect(),
        }
    }

    pub async fn issue_pairing(&self) -> Result<PairingToken, RemoteAccessError> {
        self.require_enabled()?;
        let now = now_millis();
        let expires_at = add_duration(now, self.inner.config.pairing_ttl)?;
        let token = random_secret("tvp");
        self.inner.state.lock().await.pairing = Some(StoredPairing {
            token_hash: hash_secret(&token),
            expires_at,
        });
        Ok(PairingToken { token, expires_at })
    }

    pub async fn exchange_pairing(
        &self,
        token: &str,
        device_name: &str,
    ) -> Result<IssuedRemoteSession, RemoteAccessError> {
        self.require_enabled()?;
        validate_secret(token)?;
        validate_device_name(device_name)?;
        let now = now_millis();
        let mut state = self.inner.state.lock().await;
        let pairing = state
            .pairing
            .as_ref()
            .ok_or(RemoteAccessError::AuthRequired)?;
        if pairing.expires_at <= now {
            state.pairing = None;
            return Err(RemoteAccessError::AuthRequired);
        }
        if !constant_time_eq(&pairing.token_hash, &hash_secret(token)) {
            return Err(RemoteAccessError::AuthRequired);
        }

        let mut candidate = state.clone();
        prune_inactive_devices(&mut candidate, now);
        if candidate.devices.len() >= MAX_DEVICES {
            return Err(RemoteAccessError::Invalid(
                "remote device limit is reached".into(),
            ));
        }
        let session_secret = random_secret("tvs");
        let expires_at = add_duration(now, self.inner.config.session_ttl)?;
        let stored = StoredDevice {
            device_id: Uuid::new_v4(),
            name: device_name.into(),
            secret_hash: hex_hash(&hash_secret(&session_secret)),
            created_at: now,
            last_activity_at: now,
            expires_at,
            revoked_at: None,
        };
        candidate.pairing = None;
        candidate.revision = candidate.revision.saturating_add(1);
        candidate.devices.insert(stored.device_id, stored.clone());
        persist(&self.inner.path, &candidate).await?;
        *state = candidate;
        drop(state);
        self.schedule_expiry(expires_at);
        Ok(IssuedRemoteSession {
            device: descriptor(&stored, now),
            session_secret,
        })
    }

    pub async fn authenticate(
        &self,
        session_secret: &str,
    ) -> Result<RemoteDeviceContext, RemoteAccessError> {
        self.require_enabled()?;
        validate_secret(session_secret)?;
        let supplied = hash_secret(session_secret);
        let now = now_millis();
        let mut state = self.inner.state.lock().await;
        let Some(device_id) = state.devices.values().find_map(|device| {
            decode_hash(&device.secret_hash)
                .filter(|stored| constant_time_eq(stored, &supplied))
                .map(|_| device.device_id)
        }) else {
            return Err(RemoteAccessError::AuthRequired);
        };
        let device = state
            .devices
            .get(&device_id)
            .expect("selected remote device exists");
        if device.revoked_at.is_some() {
            return Err(RemoteAccessError::SessionRevoked);
        }
        if device.expires_at <= now {
            return Err(RemoteAccessError::SessionExpired);
        }
        let expires_at = device.expires_at;
        if now.saturating_sub(device.last_activity_at) >= ACTIVITY_PERSIST_INTERVAL {
            let mut candidate = state.clone();
            candidate
                .devices
                .get_mut(&device_id)
                .expect("selected remote device exists")
                .last_activity_at = now;
            candidate.revision = candidate.revision.saturating_add(1);
            persist(&self.inner.path, &candidate).await?;
            *state = candidate;
        }
        Ok(RemoteDeviceContext {
            device_id,
            expires_at,
        })
    }

    pub async fn check_device(&self, device_id: Uuid) -> Result<(), RemoteAccessError> {
        self.require_enabled()?;
        let now = now_millis();
        let state = self.inner.state.lock().await;
        let device = state
            .devices
            .get(&device_id)
            .ok_or(RemoteAccessError::AuthRequired)?;
        if device.revoked_at.is_some() {
            Err(RemoteAccessError::SessionRevoked)
        } else if device.expires_at <= now {
            Err(RemoteAccessError::SessionExpired)
        } else {
            Ok(())
        }
    }

    pub async fn revoke(
        &self,
        device_id: Uuid,
    ) -> Result<RemoteDeviceDescriptor, RemoteAccessError> {
        self.require_enabled()?;
        let now = now_millis();
        let mut state = self.inner.state.lock().await;
        let device = state
            .devices
            .get(&device_id)
            .ok_or(RemoteAccessError::DeviceNotFound)?;
        if device.revoked_at.is_some() {
            return Ok(descriptor(device, now));
        }
        let mut candidate = state.clone();
        let revoked = candidate
            .devices
            .get_mut(&device_id)
            .expect("selected remote device exists");
        revoked.revoked_at = Some(now);
        let output = descriptor(revoked, now);
        candidate.revision = candidate.revision.saturating_add(1);
        persist(&self.inner.path, &candidate).await?;
        *state = candidate;
        drop(state);
        self.disconnect_live();
        Ok(output)
    }

    pub async fn revoke_all(&self) -> Result<u64, RemoteAccessError> {
        self.require_enabled()?;
        let now = now_millis();
        let mut state = self.inner.state.lock().await;
        let mut candidate = state.clone();
        let mut revoked = 0_u64;
        for device in candidate.devices.values_mut() {
            if device.revoked_at.is_none() {
                device.revoked_at = Some(now);
                revoked = revoked.saturating_add(1);
            }
        }
        if revoked == 0 {
            return Ok(0);
        }
        candidate.revision = candidate.revision.saturating_add(1);
        persist(&self.inner.path, &candidate).await?;
        *state = candidate;
        drop(state);
        self.disconnect_live();
        Ok(revoked)
    }

    fn require_enabled(&self) -> Result<(), RemoteAccessError> {
        if self.enabled() {
            Ok(())
        } else {
            Err(RemoteAccessError::Disabled)
        }
    }

    fn disconnect_live(&self) {
        // ponytail: revocation closes all live streams; use per-device channels only if device churn makes reconnects material.
        if let Some(sender) = lock(&self.inner.disconnects).as_ref() {
            let _ = sender.send(());
        }
    }

    fn schedule_expiry(&self, expires_at: u64) {
        let disconnects = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let delay = expires_at.saturating_sub(now_millis());
            tokio::time::sleep(Duration::from_millis(delay)).await;
            if let Some(sender) = lock(&disconnects.disconnects).as_ref() {
                let _ = sender.send(());
            }
        });
    }
}

fn validate_device_name(name: &str) -> Result<(), RemoteAccessError> {
    if name.trim() != name
        || name.is_empty()
        || name.len() > 128
        || name.chars().any(char::is_control)
    {
        return Err(RemoteAccessError::Invalid(
            "deviceName must be 1-128 printable characters without surrounding whitespace".into(),
        ));
    }
    Ok(())
}

fn validate_secret(secret: &str) -> Result<(), RemoteAccessError> {
    if secret.len() < 16 || secret.len() > 256 || secret.chars().any(char::is_control) {
        return Err(RemoteAccessError::AuthRequired);
    }
    Ok(())
}

fn descriptor(device: &StoredDevice, now: u64) -> RemoteDeviceDescriptor {
    RemoteDeviceDescriptor {
        device_id: device.device_id,
        name: device.name.clone(),
        created_at: device.created_at,
        last_activity_at: device.last_activity_at,
        expires_at: device.expires_at,
        status: if device.revoked_at.is_some() {
            RemoteDeviceStatus::Revoked
        } else if device.expires_at <= now {
            RemoteDeviceStatus::Expired
        } else {
            RemoteDeviceStatus::Active
        },
    }
}

fn prune_inactive_devices(state: &mut RemoteState, now: u64) {
    let remove_count = state
        .devices
        .len()
        .saturating_add(1)
        .saturating_sub(MAX_DEVICES);
    if remove_count == 0 {
        return;
    }
    let mut inactive = state
        .devices
        .values()
        .filter(|device| device.revoked_at.is_some() || device.expires_at <= now)
        .map(|device| (device.created_at, device.device_id))
        .collect::<Vec<_>>();
    inactive.sort_unstable();
    for (_, device_id) in inactive.into_iter().take(remove_count) {
        state.devices.remove(&device_id);
    }
}

fn random_secret(prefix: &str) -> String {
    format!(
        "{prefix}_{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    )
}

fn hash_secret(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

fn hex_hash(hash: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(64);
    for byte in hash {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn decode_hash(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut output = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        output[index] = (hex_digit(pair[0])? << 4) | hex_digit(pair[1])?;
    }
    Some(output)
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn add_duration(now: u64, duration: Duration) -> Result<u64, RemoteAccessError> {
    let millis = duration.as_millis().min(u64::MAX as u128) as u64;
    now.checked_add(millis)
        .ok_or_else(|| RemoteAccessError::Invalid("TTL exceeds the supported clock range".into()))
}

async fn load_state(path: &Path) -> Result<RemoteState, RemoteAccessError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match options.open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(RemoteState::default()),
        Err(error) => {
            return Err(RemoteAccessError::Persistence(format!(
                "open {}: {error}",
                path.display()
            )))
        }
    };
    let metadata = file.metadata().await.map_err(|error| {
        RemoteAccessError::Persistence(format!("inspect {}: {error}", path.display()))
    })?;
    if !metadata.is_file() || metadata.len() > MAX_STORE_BYTES {
        return Err(RemoteAccessError::Corrupt(
            "remote access store must be a bounded regular file".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o7777 != 0o600
        {
            return Err(RemoteAccessError::Corrupt(
                "remote access store must be effective-user-owned mode 0600".into(),
            ));
        }
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes).await.map_err(|error| {
        RemoteAccessError::Persistence(format!("read {}: {error}", path.display()))
    })?;
    let document: StoredDocument = serde_json::from_slice(&bytes)
        .map_err(|error| RemoteAccessError::Corrupt(format!("decode store: {error}")))?;
    if document.version != STORE_VERSION || document.devices.len() > MAX_DEVICES {
        return Err(RemoteAccessError::Corrupt(
            "unsupported store version or device count".into(),
        ));
    }
    let mut devices = BTreeMap::new();
    for device in document.devices {
        validate_device_name(&device.name).map_err(|error| {
            RemoteAccessError::Corrupt(format!("invalid remote device name: {error}"))
        })?;
        if decode_hash(&device.secret_hash).is_none()
            || device.created_at > device.last_activity_at
            || device.last_activity_at > device.expires_at
            || devices.insert(device.device_id, device).is_some()
        {
            return Err(RemoteAccessError::Corrupt(
                "invalid or duplicate remote device record".into(),
            ));
        }
    }
    Ok(RemoteState {
        revision: document.revision,
        devices,
        pairing: None,
    })
}

async fn persist(path: &Path, state: &RemoteState) -> Result<(), RemoteAccessError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).await.map_err(|error| {
        RemoteAccessError::Persistence(format!("create {}: {error}", parent.display()))
    })?;
    let document = StoredDocument {
        version: STORE_VERSION,
        revision: state.revision,
        devices: state.devices.values().cloned().collect(),
    };
    let bytes = serde_json::to_vec(&document)
        .map_err(|error| RemoteAccessError::Persistence(format!("encode store: {error}")))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("remote-access.json");
    let temporary = parent.join(format!(".{name}-{}.tmp", Uuid::new_v4()));
    let result = async {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).await.map_err(|error| {
            RemoteAccessError::Persistence(format!("create {}: {error}", temporary.display()))
        })?;
        file.write_all(&bytes).await.map_err(|error| {
            RemoteAccessError::Persistence(format!("write {}: {error}", temporary.display()))
        })?;
        file.sync_all().await.map_err(|error| {
            RemoteAccessError::Persistence(format!("sync {}: {error}", temporary.display()))
        })?;
        drop(file);
        replace_file(&temporary, path).await?;
        #[cfg(unix)]
        {
            let directory = fs::File::open(parent).await.map_err(|error| {
                RemoteAccessError::Persistence(format!("open {}: {error}", parent.display()))
            })?;
            directory.sync_all().await.map_err(|error| {
                RemoteAccessError::Persistence(format!("sync {}: {error}", parent.display()))
            })?;
        }
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&temporary).await;
    }
    result
}

#[cfg(not(windows))]
async fn replace_file(source: &Path, target: &Path) -> Result<(), RemoteAccessError> {
    fs::rename(source, target).await.map_err(|error| {
        RemoteAccessError::Persistence(format!("replace {}: {error}", target.display()))
    })
}

#[cfg(windows)]
async fn replace_file(source: &Path, target: &Path) -> Result<(), RemoteAccessError> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    #[link(name = "Kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }
    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(RemoteAccessError::Persistence(format!(
            "replace remote access store: {}",
            io::Error::last_os_error()
        )))
    } else {
        Ok(())
    }
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_store() -> PathBuf {
        std::env::temp_dir().join(format!("tessivum-remote-access-{}.json", Uuid::new_v4()))
    }

    #[test]
    fn full_device_store_prunes_oldest_inactive_record() {
        let mut state = RemoteState::default();
        for index in 0..MAX_DEVICES {
            let device_id = Uuid::from_u128(index as u128 + 1);
            state.devices.insert(
                device_id,
                StoredDevice {
                    device_id,
                    name: format!("device-{index}"),
                    secret_hash: "00".repeat(32),
                    created_at: index as u64,
                    last_activity_at: index as u64,
                    expires_at: u64::MAX,
                    revoked_at: (index < 2).then_some(index as u64),
                },
            );
        }

        prune_inactive_devices(&mut state, 0);

        assert_eq!(state.devices.len(), MAX_DEVICES - 1);
        assert!(!state.devices.contains_key(&Uuid::from_u128(1)));
        assert!(state.devices.contains_key(&Uuid::from_u128(2)));
    }

    #[tokio::test]
    async fn pairing_sessions_are_single_use_redacted_persistent_and_revocable() {
        let path = temporary_store();
        let config = RemoteAccessConfig {
            enabled: true,
            trusted_tunnel: true,
            pairing_ttl: Duration::from_secs(1),
            session_ttl: Duration::from_secs(1),
        };
        let access = RemoteAccess::open(&path, config.clone()).await.unwrap();
        let pairing = access.issue_pairing().await.unwrap();
        let (phone, other) = tokio::join!(
            access.exchange_pairing(&pairing.token, "Phone"),
            access.exchange_pairing(&pairing.token, "Other")
        );
        let issued = match (phone, other) {
            (Ok(issued), Err(RemoteAccessError::AuthRequired))
            | (Err(RemoteAccessError::AuthRequired), Ok(issued)) => issued,
            _ => panic!("exactly one concurrent pairing exchange must succeed"),
        };
        assert_eq!(
            access
                .authenticate(&issued.session_secret)
                .await
                .unwrap()
                .device_id,
            issued.device.device_id
        );
        let encoded = serde_json::to_string(&access.describe(None).await).unwrap();
        assert!(!encoded.contains(&pairing.token));
        assert!(!encoded.contains(&issued.session_secret));
        drop(access);

        let reopened = RemoteAccess::open(&path, config).await.unwrap();
        assert_eq!(
            reopened
                .authenticate(&issued.session_secret)
                .await
                .unwrap()
                .device_id,
            issued.device.device_id
        );
        reopened.revoke(issued.device.device_id).await.unwrap();
        assert!(matches!(
            reopened.authenticate(&issued.session_secret).await,
            Err(RemoteAccessError::SessionRevoked)
        ));
        drop(reopened);

        let reopened = RemoteAccess::open(
            &path,
            RemoteAccessConfig {
                enabled: true,
                trusted_tunnel: true,
                pairing_ttl: Duration::from_millis(5),
                session_ttl: Duration::from_millis(5),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            reopened.authenticate(&issued.session_secret).await,
            Err(RemoteAccessError::SessionRevoked)
        ));
        let expiry_path = temporary_store();
        let expiring = RemoteAccess::open(
            &expiry_path,
            RemoteAccessConfig {
                enabled: true,
                trusted_tunnel: true,
                pairing_ttl: Duration::from_millis(20),
                session_ttl: Duration::from_millis(5),
            },
        )
        .await
        .unwrap();
        let token = expiring.issue_pairing().await.unwrap();
        let short = expiring
            .exchange_pairing(&token.token, "Short lived")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(matches!(
            expiring.authenticate(&short.session_secret).await,
            Err(RemoteAccessError::SessionExpired)
        ));
        let expired_pairing = expiring.issue_pairing().await.unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(matches!(
            expiring
                .exchange_pairing(&expired_pairing.token, "Too late")
                .await,
            Err(RemoteAccessError::AuthRequired)
        ));
        std::fs::remove_file(expiry_path).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(reopened);
        std::fs::write(&path, b"{}").unwrap();
        assert!(matches!(
            RemoteAccess::open(&path, RemoteAccessConfig::default()).await,
            Err(RemoteAccessError::Corrupt(_))
        ));
        std::fs::remove_file(path).unwrap();
    }
}
