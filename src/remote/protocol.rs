//! Wire format shared by the Mac app and the iOS client.
//!
//! One reliable iroh stream carries everything: channel 0 is JSON control,
//! other channels are terminal traffic. Terminal payloads keep libghostty's
//! byte stream rather than a rendered grid, so the phone can use its own VT.

use std::io;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const ALPN: &[u8] = b"kumu/remote/1";
pub const PROTOCOL: u64 = 1;
pub const LINK_PREFIX: &str = "kumu://pair/";
pub const MAX_CONTROL: usize = 256 * 1024;
pub const MAX_BINARY: usize = 2 * 1024 * 1024;

pub const CONTROL_CHANNEL: u32 = 0;

/// Terminal frame kinds.
pub const KIND_DATA: u8 = 0;
pub const KIND_CREDIT: u8 = 1;
pub const KIND_CLOSE: u8 = 2;
pub const KIND_RESIZE: u8 = 3;
pub const KIND_REPLAY: u8 = 4;

/// The link shown in the Mac's Remote settings and pasted into the phone.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingLink {
    pub version: u64,
    pub address: iroh::EndpointAddr,
    /// Account-free address lookup, so reconnects survive a network change.
    #[serde(default)]
    pub discovery: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl PairingLink {
    #[allow(dead_code)]
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        if text.len() > 16 * 1024 {
            bail!("connection link is too large");
        }
        let link: Self = if let Some(encoded) = text.strip_prefix(LINK_PREFIX) {
            serde_json::from_slice(
                &URL_SAFE_NO_PAD
                    .decode(encoded)
                    .context("invalid connection link")?,
            )?
        } else {
            serde_json::from_str(text).context("paste the kumu://pair/ link from the Mac")?
        };
        if link.version != PROTOCOL || link.address.is_empty() {
            bail!("unsupported or incomplete connection link");
        }
        if link.address.addrs.len() > 32 || link.code.as_ref().is_some_and(|code| code.len() > 32) {
            bail!("invalid connection link");
        }
        for relay in link.address.relay_urls() {
            if relay.scheme() != "https"
                || relay.host_str().is_none()
                || relay.username() != ""
                || relay.password().is_some()
            {
                bail!("relay URLs must use HTTPS without credentials");
            }
        }
        Ok(link)
    }

    /// Encode with at most one relay and two IPs so the link stays QR-sized.
    pub fn encode(&self) -> Result<String> {
        let mut link = self.clone();
        let mut ips = link.address.ip_addrs().copied().collect::<Vec<_>>();
        ips.sort_by_key(|addr| match addr.ip() {
            std::net::IpAddr::V4(ip) if ip.is_private() || ip.is_loopback() => 0,
            std::net::IpAddr::V6(_) => 1,
            _ => 2,
        });
        link.address.addrs = self
            .address
            .relay_urls()
            .take(1)
            .cloned()
            .map(iroh::TransportAddr::Relay)
            .chain(ips.into_iter().take(2).map(iroh::TransportAddr::Ip))
            .collect();
        Ok(format!(
            "{LINK_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&link)?)
        ))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Message {
    Text(String),
    /// A complete terminal frame, including its 9-byte header.
    Binary(Vec<u8>),
}

pub fn frame(channel: u32, kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(9 + payload.len());
    bytes.extend_from_slice(&channel.to_le_bytes());
    bytes.push(kind);
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

pub async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Message>> {
    let mut header = [0u8; 9];
    if reader.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..]).await?;
    let channel = u32::from_le_bytes(header[..4].try_into().expect("four bytes"));
    let length = u32::from_le_bytes(header[5..].try_into().expect("four bytes")) as usize;
    let limit = if channel == CONTROL_CHANNEL {
        MAX_CONTROL
    } else {
        MAX_BINARY.saturating_sub(9)
    };
    if length > limit || (channel == CONTROL_CHANNEL && header[4] != 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid frame header or length",
        ));
    }
    let mut bytes = vec![0; 9 + length];
    bytes[..9].copy_from_slice(&header);
    reader.read_exact(&mut bytes[9..]).await?;
    if channel == CONTROL_CHANNEL {
        let text = String::from_utf8(bytes.split_off(9))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(Some(Message::Text(text)))
    } else {
        Ok(Some(Message::Binary(bytes)))
    }
}

pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &Message,
) -> io::Result<()> {
    match message {
        Message::Text(text) if text.len() <= MAX_CONTROL => {
            writer
                .write_all(&frame(CONTROL_CHANNEL, 0, text.as_bytes()))
                .await
        }
        Message::Binary(bytes) if bytes.len() >= 9 && bytes.len() <= MAX_BINARY => {
            let channel = u32::from_le_bytes(bytes[..4].try_into().expect("four bytes"));
            let length = u32::from_le_bytes(bytes[5..9].try_into().expect("four bytes")) as usize;
            if channel == CONTROL_CHANNEL || length != bytes.len() - 9 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid terminal frame",
                ));
            }
            writer.write_all(bytes).await
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message exceeds protocol limit",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn fragmented_messages_keep_their_boundaries() {
        let messages = vec![
            Message::Text("{\"hello\":\"中文\"}".into()),
            Message::Binary(frame(7, KIND_DATA, b"\x1b[31mterminal")),
            Message::Binary(frame(7, KIND_CLOSE, b"")),
        ];
        let mut bytes = Vec::new();
        for message in &messages {
            write_message(&mut bytes, message).await.unwrap();
        }
        let (mut send, mut recv) = duplex(3);
        tokio::spawn(async move {
            send.write_all(&bytes).await.unwrap();
        });
        for expected in messages {
            assert_eq!(read_message(&mut recv).await.unwrap().unwrap(), expected);
        }
        assert!(read_message(&mut recv).await.unwrap().is_none());
    }

    #[test]
    fn a_terminal_frame_rejects_a_mismatched_length() {
        let mut bytes = frame(3, KIND_DATA, b"abc");
        bytes[5] = 9;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(async {
            let (mut send, mut recv) = duplex(64);
            send.write_all(&bytes).await.unwrap();
            drop(send);
            read_message(&mut recv).await
        });
        assert!(result.is_err());
    }
}
