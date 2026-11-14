//! Remote control for the iOS client.
//!
//! The Mac owns the terminals; the phone gets workspace facts as JSON and the
//! focused terminal as raw PTY bytes. iroh supplies the encrypted transport,
//! and a one-use pairing code authorizes the device.

mod auth;
mod protocol;
mod server;
mod state;

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::channel::mpsc::UnboundedSender;
use iroh::endpoint::presets;
use iroh::{Endpoint, RelayMap, RelayMode, SecretKey};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

pub use auth::Auth;
pub use protocol::{ALPN, PROTOCOL, PairingLink};
pub use state::{AgentInfo, ComputerInfo, PaneInfo, PaneStatus, ProjectInfo, RemoteState};

use server::RemoteServer;

/// The largest grid a phone may ask for, on attach and on resize. Bounds what
/// one device can make libghostty allocate.
pub const MAX_GRID: u16 = 1000;

/// How the phone controls one pane. v1 always means the phone owns the size.
pub struct AttachHandle {
    pub tap: u64,
    pub cols: u16,
    pub rows: u16,
    pub title: String,
    pub status: PaneStatus,
    /// ANSI screen and mode sequence, produced after the resize.
    pub replay: Vec<u8>,
    /// Live bytes after the replay. Bounded; a slow consumer closes the tap.
    pub output: futures::channel::mpsc::Receiver<Vec<u8>>,
}

#[derive(Debug)]
pub struct RemoteError {
    pub kind: String,
    pub message: String,
}

impl RemoteError {
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for RemoteError {}

/// Where a remote action goes on the desktop. No variant carries a command or
/// a filesystem path.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Action {
    NewTerminal {
        #[serde(default)]
        project: Option<i64>,
    },
    NewAgent {
        #[serde(default)]
        project: Option<i64>,
        agent: String,
    },
    ClosePane {
        pane: u64,
    },
    ShowOnDesktop {
        pane: u64,
    },
}

pub enum Command {
    Attach {
        pane: u64,
        cols: u16,
        rows: u16,
        reply: oneshot::Sender<Result<AttachHandle, RemoteError>>,
    },
    Detach {
        pane: u64,
        tap: u64,
        reply: Option<oneshot::Sender<()>>,
    },
    Input {
        pane: u64,
        tap: u64,
        bytes: Vec<u8>,
    },
    Resize {
        pane: u64,
        tap: u64,
        cols: u16,
        rows: u16,
    },
    Action {
        action: Action,
        reply: oneshot::Sender<Result<(), RemoteError>>,
    },
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum RelayChoice {
    Default,
    Disabled,
    RelayOnly,
    Custom(Vec<String>),
}

#[derive(Clone, Debug)]
pub struct RemoteConfig {
    pub name: String,
    pub state_dir: PathBuf,
    pub relay: RelayChoice,
    pub discovery: bool,
    pub max_connections: usize,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            name: hostname(),
            state_dir: default_state_dir(),
            relay: RelayChoice::Default,
            discovery: true,
            max_connections: 8,
        }
    }
}

pub struct RemoteHandle {
    /// The address is enough to rebuild a link when the pairing code rotates.
    address: iroh::EndpointAddr,
    discovery: bool,
    pub auth: Arc<Auth>,
    stop: Option<oneshot::Sender<()>>,
}

impl RemoteHandle {
    /// The current pairing link. A code rotates after every successful pair,
    /// so the link is generated from the address on demand rather than stored.
    pub fn pairing_link(&self) -> Option<String> {
        let code = self.auth.code();
        if code.is_empty() {
            return None;
        }
        PairingLink {
            version: PROTOCOL,
            address: self.address.clone(),
            discovery: self.discovery,
            code: Some(code),
        }
        .encode()
        .ok()
    }

    pub fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

impl Drop for RemoteHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start the iroh endpoint and its accept loop on a thread of its own. The
/// returned handle keeps the service alive; dropping it asks the loop to stop.
pub fn start(
    config: RemoteConfig,
    state: Arc<RwLock<RemoteState>>,
    commands: UnboundedSender<Command>,
) -> Result<RemoteHandle> {
    std::fs::create_dir_all(&config.state_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config.state_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let auth = Arc::new(Auth::open(&config.state_dir)?);
    let identity = load_identity(&config.state_dir)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("kumu-remote")
        .build()
        .context("cannot start the remote runtime")?;

    let mut builder = if config.discovery {
        Endpoint::builder(presets::N0)
    } else {
        Endpoint::builder(presets::Minimal)
    };
    let relay = match &config.relay {
        RelayChoice::Default => RelayMode::Default,
        RelayChoice::Disabled => RelayMode::Disabled,
        RelayChoice::RelayOnly => RelayMode::Default,
        RelayChoice::Custom(urls) => {
            if urls.is_empty() {
                bail!("custom relay list is empty");
            }
            let parsed = urls
                .iter()
                .map(|url| url.parse::<iroh::RelayUrl>())
                .collect::<Result<Vec<_>, _>>()
                .context("invalid relay URL")?;
            RelayMode::Custom(RelayMap::from_iter(parsed))
        }
    };
    builder = builder
        .secret_key(identity)
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(relay);
    if matches!(config.relay, RelayChoice::RelayOnly) {
        builder = builder.clear_ip_transports();
    }
    let endpoint = runtime
        .block_on(async {
            let endpoint = builder.bind().await?;
            for _ in 0..20 {
                if !endpoint.addr().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            // A link without a relay only works on the same network. Give the
            // relay a moment to appear before the pairing link is built.
            if !matches!(config.relay, RelayChoice::Disabled) {
                let _ = tokio::time::timeout(Duration::from_secs(5), endpoint.online()).await;
            }
            Ok::<_, anyhow::Error>(endpoint)
        })
        .context("cannot start the remote endpoint")?;
    let address = endpoint.addr();

    let (stop_tx, stop_rx) = oneshot::channel();
    let server = Arc::new(RemoteServer {
        endpoint,
        name: config.name.clone(),
        auth: auth.clone(),
        state,
        commands,
        max_connections: config.max_connections,
    });
    std::thread::Builder::new()
        .name("kumu-remote".into())
        .spawn(move || {
            runtime.block_on(async move {
                tokio::select! {
                    result = server.clone().serve() => {
                        if let Err(error) = result {
                            eprintln!("[remote] server stopped: {error:#}");
                        }
                    }
                    _ = stop_rx => {
                        server.endpoint.close().await;
                    }
                }
            });
        })
        .context("cannot spawn the remote thread")?;

    Ok(RemoteHandle {
        address,
        discovery: config.discovery,
        auth,
        stop: Some(stop_tx),
    })
}

pub fn hostname() -> String {
    std::env::var("KUMU_HOSTNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "Mac".to_owned())
}

pub fn default_state_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".kumu")
        .join("remote")
}

fn load_identity(dir: &Path) -> Result<SecretKey> {
    let path = dir.join("identity");
    if !path.exists() {
        let key = SecretKey::generate();
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write as _;
        let mut file = options.open(&path)?;
        file.write_all(&key.to_bytes())?;
        file.sync_all()?;
        return Ok(key);
    }
    let bytes = std::fs::read(&path)?;
    if bytes.len() != 32 {
        bail!("invalid remote identity at {}", path.display());
    }
    Ok(SecretKey::from_bytes(
        bytes.as_slice().try_into().expect("32 bytes"),
    ))
}
