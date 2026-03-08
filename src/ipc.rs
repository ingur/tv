//! IPC module for tv daemon/client/session communication.
//!
//! Provides:
//! - Socket path utilities
//! - Length-prefixed JSON message framing
//! - Three connection types: sync client, async server, non-blocking session

pub mod client;
pub mod server;
pub mod session;

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Serialize};

/// Maximum IPC message size (16 MB). Prevents OOM from corrupted length prefixes.
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Get the socket path for the tv daemon.
/// Uses /tmp/tv-$UID/tv.sock
pub fn socket_path() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/tv-{}/tv.sock", uid))
}

/// Ensure the socket directory exists with restrictive permissions.
pub fn ensure_socket_dir() -> Result<()> {
    let path = socket_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create socket directory {}", parent.display()))?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to set permissions on {}", parent.display()))?;
    }
    Ok(())
}

/// Encode a message as length-prefixed JSON.
/// Format: [4 bytes: length as u32 big-endian][JSON payload]
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(msg)?;
    let len = json.len() as u32;
    let mut buf = Vec::with_capacity(4 + json.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&json);
    Ok(buf)
}

/// Decode a length-prefixed JSON message from a buffer.
/// Returns Ok(None) if buffer doesn't contain a complete message.
pub fn decode<T: DeserializeOwned>(buf: &[u8]) -> Result<Option<(T, usize)>> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_MESSAGE_SIZE {
        bail!("message too large: {} bytes (max {})", len, MAX_MESSAGE_SIZE);
    }
    let total = 4 + len;
    if buf.len() < total {
        return Ok(None);
    }
    let msg: T = serde_json::from_slice(&buf[4..total])?;
    Ok(Some((msg, total)))
}

/// Sync helper: read a complete framed message.
pub fn read_message<T: DeserializeOwned, R: Read>(reader: &mut R) -> Result<T> {
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .context("failed to read IPC message length")?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_SIZE {
        bail!("message too large: {} bytes (max {})", len, MAX_MESSAGE_SIZE);
    }
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .context("failed to read IPC message payload")?;
    let msg: T = serde_json::from_slice(&payload).context("failed to decode IPC message")?;
    Ok(msg)
}

/// Sync helper: write a complete framed message.
pub fn write_message<T: Serialize, W: Write>(writer: &mut W, msg: &T) -> Result<()> {
    let encoded = encode(msg)?;
    writer
        .write_all(&encoded)
        .context("failed to write IPC message")?;
    writer.flush().context("failed to flush IPC stream")?;
    Ok(())
}
