//! Client module - handles CLI command execution.
//!
//! The client is a simple one-shot process:
//! 1. Receive AuthRequest, respond with AuthResponse::Client
//! 2. Receive Response
//! 3. Print result and exit

use anyhow::{bail, Context, Result};
use comfy_table::presets::NOTHING;
use comfy_table::{ContentArrangement, Table};

use crate::cli::{self, Commands};
use crate::config::{Config, OutputFormat};
use crate::ipc;
use crate::keys;

use crate::types::{AuthRequest, AuthResponse, Request, Response, Selector, SessionInfo};

fn convert_selector(sel: cli::Selector) -> Selector {
    match sel {
        cli::Selector::Id(id) => Selector::Id(id),
        cli::Selector::Tag(tag) => Selector::Tag(tag),
    }
}

// === Output Formatting ===

fn resolve_format(json_flag: bool, pretty_flag: bool, config: &Config) -> OutputFormat {
    if json_flag {
        OutputFormat::Json
    } else if pretty_flag {
        OutputFormat::Pretty
    } else {
        config.output_format
    }
}

fn format_list(sessions: &[SessionInfo], format: OutputFormat) -> String {
    match format {
        OutputFormat::Json => serde_json::to_string(&sessions).unwrap_or_default(),
        OutputFormat::Pretty => {
            let mut sorted: Vec<&SessionInfo> = sessions.iter().collect();
            sorted.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
            format_list_table(&sorted)
        }
    }
}

/// Truncate path from left, keeping end. Replace home dir with ~.
fn truncate_path(path: &str, max_len: usize) -> String {
    let home = dirs::home_dir().map(|p| p.to_string_lossy().to_string());
    let path = match &home {
        Some(h) if path.starts_with(h) => format!("~{}", &path[h.len()..]),
        _ => path.to_string(),
    };

    let char_count = path.chars().count();
    if char_count <= max_len {
        return path;
    }

    let skip = char_count - max_len + 1;
    let truncated: String = path.chars().skip(skip).collect();
    format!("…{truncated}")
}

fn format_list_table(sessions: &[&SessionInfo]) -> String {
    if sessions.is_empty() {
        return "No sessions".to_string();
    }

    let now = crate::types::epoch_secs();

    let mut table = Table::new();
    table
        .load_preset(NOTHING)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["ID", "TAG", "ACTIVITY", "CREATED", "CWD"]);

    for s in sessions {
        let tag = s
            .tag
            .as_ref()
            .map(|t| format!("@{}", t))
            .unwrap_or_else(|| "-".into());
        let activity = format_relative_time(now, s.last_activity);
        let created = format_relative_time(now, s.created_at);
        let cwd = truncate_path(s.cwd.as_deref().unwrap_or("-"), 30);

        table.add_row(vec![&s.id, &tag, &activity, &created, &cwd]);
    }

    table.to_string()
}

/// Format a timestamp relative to now in human-friendly form.
/// "3s ago", "2m ago", "1h ago", "3h ago", "yesterday", "2d ago"
fn format_relative_time(now: u64, timestamp: u64) -> String {
    if timestamp == 0 {
        return "-".into();
    }

    let delta = now.saturating_sub(timestamp);

    if delta < 60 {
        format!("{}s ago", delta)
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86400 {
        format!("{}h ago", delta / 3600)
    } else if delta < 172800 {
        "yesterday".into()
    } else {
        format!("{}d ago", delta / 86400)
    }
}

// === Client Entry Points ===

/// Run a client command on an established connection.
pub fn run(conn: ipc::client::Connection, cmd: Commands, config: &Config) -> Result<()> {
    let (request, format) = command_to_request(cmd, config)?;
    run_request(conn, request, format, config)
}

/// Run a raw request on an established connection.
/// Used for daemon control commands (Stop) that don't map to CLI commands.
pub fn run_request(
    mut conn: ipc::client::Connection,
    request: Request,
    format: OutputFormat,
    config: &Config,
) -> Result<()> {
    // Receive AuthRequest from server
    let auth_req: AuthRequest = conn.recv().context("failed to receive auth request")?;

    // Send AuthResponse::Client
    conn.send(&AuthResponse::Client {
        id: auth_req.id,
        pid: std::process::id(),
        request: request.clone(),
    })
    .context("failed to send auth response")?;

    // Receive Response
    let response: Response = conn.recv().context("failed to receive response")?;

    // Print and exit
    print_response(response, &request, format, config)
}

/// Convert CLI command to IPC Request and determine output format.
fn command_to_request(cmd: Commands, config: &Config) -> Result<(Request, OutputFormat)> {
    let result = match cmd {
        Commands::New { .. } => unreachable!("new handled in main"),
        Commands::Daemon { .. } => unreachable!("daemon handled in main"),
        Commands::Llms => unreachable!("llms handled in main"),

        Commands::Show { selector } => (
            Request::Show {
                selector: selector.map(convert_selector),
            },
            config.output_format,
        ),

        Commands::Hide { selector } => (
            Request::Hide {
                selector: selector.map(convert_selector),
            },
            config.output_format,
        ),

        Commands::Tag(args) => {
            let format = resolve_format(args.json, args.pretty, config);

            // Determine mode based on arguments:
            // - No args: query own tag
            // - Just @tag: set own tag
            // - selector @tag: set other's tag
            // - -d: delete own tag
            // - selector -d: delete other's tag
            let is_query = args.selector.is_none() && args.new_tag.is_none() && !args.delete;

            let (selector, new_tag) = if is_query {
                // Query mode
                (None, None)
            } else if args.selector.is_some() && args.new_tag.is_none() && !args.delete {
                // Single arg provided: reinterpret based on type
                match args.selector {
                    Some(cli::Selector::Tag(tag)) => (None, Some(tag)),
                    Some(cli::Selector::Id(_)) => {
                        bail!("specify a tag to set or use -d to delete")
                    }
                    None => unreachable!("guarded by is_some()"),
                }
            } else {
                (
                    args.selector.map(convert_selector),
                    args.new_tag.map(|t| t.0),
                )
            };

            (
                Request::Tag {
                    selector,
                    new_tag,
                    delete: args.delete,
                },
                format,
            )
        }

        Commands::Clear { selector } => (
            Request::Clear {
                selector: selector.map(convert_selector),
            },
            config.output_format,
        ),

        Commands::List(args) => {
            let format = resolve_format(args.json, args.pretty, config);
            (
                Request::List {
                    selector: args.selector.map(convert_selector),
                    all: args.all,
                },
                format,
            )
        }

        Commands::Peek(args) => {
            let format = resolve_format(args.json, args.pretty, config);
            (
                Request::Peek {
                    selector: args.selector.map(convert_selector),
                },
                format,
            )
        }

        Commands::History(args) => {
            let format = resolve_format(args.json, args.pretty, config);
            (
                Request::History {
                    selector: args.selector.map(convert_selector),
                    count: args.count,
                    offset: args.offset,
                },
                format,
            )
        }

        Commands::Exec(args) => {
            // Validate key syntax at CLI time — fail early with a clear error
            let groups = keys::parse_exec_args(&args.keys)
                .map_err(|e| anyhow::anyhow!("invalid key syntax: {}", e))?;

            // Handle {stdin}: validate usage, read piped input
            let stdin_count = groups
                .iter()
                .filter(|g| matches!(g, keys::KeyGroup::Stdin))
                .count();
            if stdin_count > 1 {
                bail!("{{stdin}} can only appear once");
            }

            let stdin_text = if stdin_count == 1 {
                use std::io::{IsTerminal, Read};
                if std::io::stdin().is_terminal() {
                    bail!("{{stdin}} requires piped input");
                }
                let mut text = String::new();
                std::io::stdin()
                    .read_to_string(&mut text)
                    .context("failed to read stdin")?;
                // Strip trailing newline added by echo, heredocs, etc.
                // The user controls line endings with explicit keys like {cr}.
                if text.ends_with('\n') {
                    text.pop();
                    if text.ends_with('\r') {
                        text.pop();
                    }
                }
                Some(text)
            } else {
                None
            };

            (
                Request::Exec {
                    selector: Some(convert_selector(args.selector)),
                    keys: args.keys,
                    stdin: stdin_text,
                },
                config.output_format,
            )
        }
    };
    Ok(result)
}

/// Print response based on format and request type.
fn print_response(
    response: Response,
    request: &Request,
    format: OutputFormat,
    _config: &Config,
) -> Result<()> {
    if !response.ok {
        let msg = response.error.unwrap_or_else(|| "unknown error".into());
        eprintln!("error: {}", msg);
        std::process::exit(1);
    }

    match request {
        // Query commands — output data
        Request::List { .. } => {
            if let Some(data) = response.data {
                let sessions: Vec<SessionInfo> = serde_json::from_value(data)?;
                println!("{}", format_list(&sessions, format));
            }
        }

        Request::Peek { .. } => {
            if let Some(data) = response.data {
                match format {
                    OutputFormat::Json => {
                        println!("{}", serde_json::to_string(&data)?);
                    }
                    OutputFormat::Pretty => {
                        if let Some(content) = data.get("content").and_then(|v| v.as_str()) {
                            print!("{}", content);
                            if !content.ends_with('\n') {
                                println!();
                            }
                        }
                    }
                }
            }
        }

        Request::History { .. } => {
            if let Some(data) = response.data {
                match format {
                    OutputFormat::Json => {
                        println!("{}", serde_json::to_string(&data)?);
                    }
                    OutputFormat::Pretty => {
                        if let Some(content) = data.get("content").and_then(|v| v.as_str()) {
                            print!("{}", content);
                            if !content.ends_with('\n') {
                                println!();
                            }
                        }
                    }
                }
            }
        }

        Request::Tag {
            new_tag, delete, ..
        } => {
            let is_query = new_tag.is_none() && !delete;
            if let Some(data) = response.data {
                match format {
                    OutputFormat::Pretty => {
                        if is_query {
                            let id = data.get("id").and_then(|v| v.as_str()).unwrap_or("????");
                            let tag = data.get("tag").and_then(|v| v.as_str());
                            match tag {
                                Some(t) => println!("[{}] @{}", id, t),
                                None => println!("[{}]", id),
                            }
                        }
                        // Set/delete mode: silent success
                    }
                    OutputFormat::Json => {
                        println!("{}", serde_json::to_string(&data)?);
                    }
                }
            }
        }

        // Action commands — silent success, no output
        Request::Show { .. }
        | Request::Hide { .. }
        | Request::Clear { .. }
        | Request::Exec { .. } => {}

        _ => {
            if let Some(data) = response.data {
                println!("{}", serde_json::to_string(&data)?);
            }
        }
    }

    Ok(())
}
