//! Async (tokio) IPC server for the daemon.

use std::os::unix::io::AsRawFd;

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::auth;
use crate::ipc::{encode, ensure_socket_dir, socket_path};

/// Async listener for incoming connections.
pub struct Listener {
    inner: UnixListener,
}

impl Listener {
    /// Bind to the socket path, removing any stale socket file.
    pub async fn bind() -> Result<Self> {
        let path = socket_path();
        ensure_socket_dir()?;

        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }

        let inner = UnixListener::bind(&path)
            .with_context(|| format!("failed to bind daemon socket at {}", path.display()))?;
        Ok(Self { inner })
    }

    /// Accept a new connection.
    pub async fn accept(&self) -> Result<Connection> {
        let (stream, _) = self.inner.accept().await.context("failed to accept connection")?;
        let peer_pid = auth::get_peer_pid(stream.as_raw_fd())
            .context("failed to get peer credentials")?;
        Ok(Connection {
            stream,
            peer_pid,
            buf: Vec::with_capacity(4096),
        })
    }
}

/// Async connection for the server.
pub struct Connection {
    stream: UnixStream,
    peer_pid: u32,
    buf: Vec<u8>,
}

impl Connection {
    /// Get the peer PID.
    pub fn peer_pid(&self) -> u32 {
        self.peer_pid
    }

    /// Send a message.
    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<()> {
        let encoded = encode(msg)?;
        self.stream
            .write_all(&encoded)
            .await
            .context("failed to write to connection")?;
        self.stream
            .flush()
            .await
            .context("failed to flush connection")?;
        Ok(())
    }

    /// Receive a message.
    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<T> {
        // Read length prefix
        let mut len_buf = [0u8; 4];
        self.stream
            .read_exact(&mut len_buf)
            .await
            .context("failed to read message length from connection")?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > crate::ipc::MAX_MESSAGE_SIZE {
            anyhow::bail!("message too large: {} bytes", len);
        }

        // Read payload
        self.buf.clear();
        self.buf.resize(len, 0);
        self.stream
            .read_exact(&mut self.buf)
            .await
            .context("failed to read message payload from connection")?;

        let msg: T =
            serde_json::from_slice(&self.buf).context("failed to decode message from connection")?;
        Ok(msg)
    }

    /// Wait for the connection to be closed by the peer.
    /// Returns when the socket is closed or an error occurs.
    pub async fn closed(&mut self) {
        let mut buf = [0u8; 1];
        // read() returns Ok(0) on clean close, Err on error - either means disconnected
        let _ = self.stream.read(&mut buf).await;
    }
}
