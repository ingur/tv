//! Non-blocking IPC connection for sessions (mio-compatible).

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};

use crate::ipc::{decode, encode, socket_path};

const RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Disconnected,
    Connected,
}

/// Non-blocking connection with reconnect support.
pub struct Connection {
    stream: Option<UnixStream>,
    state: State,
    read_buf: Vec<u8>,
    write_queue: VecDeque<Vec<u8>>,
    retry_at: Option<Instant>,
}

impl Connection {
    /// Initial blocking connect. Returns a connected, non-blocking connection.
    /// Used for startup in main.rs before entering the event loop.
    pub fn connect_blocking() -> Result<Self> {
        let path = socket_path();
        let stream = UnixStream::connect(&path).context("failed to connect to daemon socket")?;
        stream
            .set_nonblocking(true)
            .context("failed to set socket to non-blocking")?;

        Ok(Self {
            stream: Some(stream),
            state: State::Connected,
            read_buf: Vec::with_capacity(4096),
            write_queue: VecDeque::new(),
            retry_at: None,
        })
    }

    /// Get raw fd for mio registration.
    pub fn as_raw_fd(&self) -> Option<RawFd> {
        self.stream.as_ref().map(|s| s.as_raw_fd())
    }

    /// Non-blocking reconnect attempt. Returns true if connection was established.
    /// Used in mio event loop after a disconnect.
    pub fn try_reconnect(&mut self) -> Result<bool> {
        if self.state == State::Connected {
            return Ok(false);
        }

        // Check retry timer
        if self
            .retry_at
            .is_some_and(|retry_at| Instant::now() < retry_at)
        {
            return Ok(false);
        }

        let path = socket_path();

        match UnixStream::connect(&path) {
            Ok(stream) => {
                stream
                    .set_nonblocking(true)
                    .context("failed to set reconnected socket to non-blocking")?;
                self.stream = Some(stream);
                self.state = State::Connected;
                self.read_buf.clear();
                self.retry_at = None;
                Ok(true)
            }
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    || e.kind() == io::ErrorKind::ConnectionRefused =>
            {
                self.schedule_retry();
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Check if it's time to retry connection.
    pub fn should_retry(&self) -> bool {
        if self.state != State::Disconnected {
            return false;
        }
        match self.retry_at {
            Some(t) => Instant::now() >= t,
            None => true,
        }
    }

    /// Read available data into internal buffer.
    /// Returns false if connection was closed.
    pub fn read_into_buffer(&mut self) -> Result<bool> {
        let stream = match self.stream.as_mut() {
            Some(s) => s,
            None => return Ok(false),
        };

        let mut tmp = [0u8; 4096];
        loop {
            match stream.read(&mut tmp) {
                Ok(0) => {
                    // Connection closed
                    self.handle_disconnect();
                    return Ok(false);
                }
                Ok(n) => {
                    self.read_buf.extend_from_slice(&tmp[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.handle_disconnect();
                    return Err(e.into());
                }
            }
        }
        Ok(true)
    }

    /// Try to decode a message from the buffer.
    /// Returns None if no complete message is available.
    pub fn try_recv<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        match decode::<T>(&self.read_buf)? {
            Some((msg, consumed)) => {
                self.read_buf.drain(..consumed);
                Ok(Some(msg))
            }
            None => Ok(None),
        }
    }

    /// Queue a message for sending.
    pub fn queue_send<T: Serialize>(&mut self, msg: &T) -> Result<()> {
        let encoded = encode(msg)?;
        self.write_queue.push_back(encoded);
        Ok(())
    }

    /// Check if there are pending writes.
    pub fn has_pending_writes(&self) -> bool {
        !self.write_queue.is_empty()
    }

    /// Flush write queue. Call when socket is writable.
    /// Returns Ok(true) if all data was flushed, Ok(false) if more remains.
    pub fn flush(&mut self) -> Result<bool> {
        let stream = match self.stream.as_mut() {
            Some(s) => s,
            None => return Ok(true),
        };

        while let Some(data) = self.write_queue.front_mut() {
            match stream.write(data) {
                Ok(0) => {
                    // Connection closed
                    self.handle_disconnect();
                    return Ok(true);
                }
                Ok(n) => {
                    if n >= data.len() {
                        self.write_queue.pop_front();
                    } else {
                        data.drain(..n);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.handle_disconnect();
                    return Err(e.into());
                }
            }
        }
        Ok(true)
    }

    /// Handle disconnection (clears state, schedules retry).
    pub fn handle_disconnect(&mut self) {
        self.stream = None;
        self.state = State::Disconnected;
        self.read_buf.clear();
        self.write_queue.clear();
        self.schedule_retry();
    }

    fn schedule_retry(&mut self) {
        self.retry_at = Some(Instant::now() + RETRY_INTERVAL);
    }
}
