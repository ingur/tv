//! tv - Terminal session manager

mod auth;
mod cli;
mod client;
mod config;
mod ipc;
mod keys;
mod server;
mod session;
mod types;

use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

use cli::{Cli, DaemonAction};

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Load config early to fail fast on malformed config
    let config = config::Config::load()?;

    match cli.command {
        // No subcommand — start new session
        None => {
            let conn = get_session_connection()?;
            session::run(conn, &config, cli.tag.map(|t| t.0))
        }

        // Daemon management (special cases)
        Some(cli::Commands::Daemon { action }) => match action {
            DaemonAction::Start => {
                if is_daemon_running() {
                    anyhow::bail!("daemon already running");
                }
                server::run(&config)
            }
            DaemonAction::Stop => {
                let conn = ipc::client::Connection::connect()
                    .context("daemon not running")?;
                client::run_request(conn, types::Request::Stop, config.output_format, &config)
            }
            DaemonAction::Restart => {
                // Stop existing daemon if running
                if let Ok(conn) = ipc::client::Connection::connect() {
                    let _ = client::run_request(conn, types::Request::Stop, config.output_format, &config);
                    // Wait briefly for the old daemon to release the socket
                    thread::sleep(Duration::from_millis(100));
                }
                spawn_daemon()?;
                // Verify the new daemon is ready before returning
                wait_for_daemon(ipc::client::Connection::connect)?;
                Ok(())
            }
        },

        // LLM reference — no daemon needed
        Some(cli::Commands::Llms) => {
            print!("{}", include_str!("llms.txt"));
            Ok(())
        }

        // Regular commands
        Some(cmd) => {
            let conn = get_client_connection()?;
            client::run(conn, cmd, &config)
        }
    }
}

/// Get a sync connection for client use, spawning daemon if needed.
fn get_client_connection() -> Result<ipc::client::Connection> {
    if let Ok(conn) = ipc::client::Connection::connect() {
        return Ok(conn);
    }
    spawn_daemon()?;
    wait_for_daemon(ipc::client::Connection::connect)
}

/// Get a non-blocking connection for session use, spawning daemon if needed.
fn get_session_connection() -> Result<ipc::session::Connection> {
    if let Ok(conn) = ipc::session::Connection::connect_blocking() {
        return Ok(conn);
    }
    spawn_daemon()?;
    wait_for_daemon(ipc::session::Connection::connect_blocking)
}

/// Retry connecting to the daemon with linear backoff.
/// Tries up to 10 times (50ms, 100ms, 150ms... total ~2.75s worst case).
fn wait_for_daemon<T>(connect: impl Fn() -> Result<T>) -> Result<T> {
    for attempt in 1..=10 {
        thread::sleep(Duration::from_millis(50 * attempt));
        if let Ok(conn) = connect() {
            return Ok(conn);
        }
    }
    connect().context("daemon failed to start in time")
}

/// Check if daemon is running by attempting to connect.
fn is_daemon_running() -> bool {
    ipc::client::Connection::connect().is_ok()
}

/// Spawn daemon as background process.
fn spawn_daemon() -> Result<()> {
    let exe = std::env::current_exe().context("failed to get current executable path")?;
    Command::new(exe)
        .args(["daemon", "start"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn daemon")?;
    Ok(())
}
