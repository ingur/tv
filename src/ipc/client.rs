//! Synchronous IPC client for one-shot request/response.

use std::os::unix::net::UnixStream;

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};

use crate::ipc::{read_message, socket_path, write_message};

/// Synchronous connection for clients.
pub struct Connection {
    stream: UnixStream,
}

impl Connection {
    /// Connect to the daemon.
    pub fn connect() -> Result<Self> {
        let stream =
            UnixStream::connect(socket_path()).context("failed to connect to daemon socket")?;
        Ok(Self { stream })
    }

    /// Send a message.
    pub fn send<T: Serialize>(&mut self, msg: &T) -> Result<()> {
        write_message(&mut self.stream, msg)
    }

    /// Receive a message.
    pub fn recv<T: DeserializeOwned>(&mut self) -> Result<T> {
        read_message(&mut self.stream)
    }
}
