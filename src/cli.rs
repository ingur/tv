//! CLI parsing and types.

use std::str::FromStr;

use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Args, Parser, Subcommand};

const STYLES: Styles = Styles::styled()
    .header(AnsiColor::BrightGreen.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::BrightGreen.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::BrightCyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Cyan.on_default())
    .error(AnsiColor::BrightRed.on_default().effects(Effects::BOLD))
    .valid(AnsiColor::BrightCyan.on_default().effects(Effects::BOLD))
    .invalid(AnsiColor::BrightYellow.on_default().effects(Effects::BOLD));

#[derive(Parser)]
#[command(
    name = "tv",
    version,
    about = "Collaborative terminal for humans and agents",
    long_about = "Collaborative terminal for humans and agents.\n\n\
        Run 'tv' to start a new terminal session.\n\
        Use subcommands from within a session to interact with other sessions.",
    after_help = "\
Sessions are identified by a short hex ID (e.g. a1b2) or a tag (e.g. @work).
Use 'tv ls' to list active sessions. Most commands accept an optional selector
to target a specific session — omit it to target your own.

Use --json on any subcommand for machine-readable output.
Use 'tv llms' for a full programmatic reference (for AI agents).",
    styles = STYLES
)]
pub struct Cli {
    /// Tag for the new session (e.g. @work)
    #[arg(short = 't', long = "tag")]
    pub tag: Option<Tag>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Daemon management
    #[command(hide = true)]
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// Make session visible to tv ls
    Show {
        /// Target session (hex id or @tag), defaults to own session
        selector: Option<Selector>,
    },

    /// Hide session from tv ls
    Hide {
        /// Target session (hex id or @tag), defaults to own session
        selector: Option<Selector>,
    },

    /// Clear scrollback and command history
    Clear {
        /// Target session (hex id or @tag), defaults to own session
        selector: Option<Selector>,
    },

    /// List active sessions
    #[command(
        visible_alias = "ls",
        after_help = "Output columns: ID, TAG, ACTIVITY, CREATED, CWD\nSorted by most recent activity."
    )]
    List(ListArgs),

    /// Get, set, or delete a session tag
    #[command(
        visible_alias = "t",
        after_help = "\
With no arguments, shows the current session's tag.
With just @tag, sets the tag on your own session.
With a selector and @tag, sets the tag on another session."
    )]
    Tag(TagArgs),

    /// View current screen content of a session
    #[command(
        visible_alias = "p",
        after_help = "JSON output includes cursor position, terminal dimensions, and alt screen status."
    )]
    Peek(PeekArgs),

    /// View scrollback history
    #[command(visible_alias = "h")]
    History(HistoryArgs),

    /// Print full reference for AI agents / LLMs
    #[command(
        after_help = "Outputs a comprehensive reference document to stdout.\nDesigned for consumption by AI agents and LLM tools."
    )]
    Llms,

    /// Execute keystrokes in a session
    #[command(
        visible_alias = "x",
        after_help = "\
Examples:
  tv exec @work \"ls -la\" {cr}                  Run a command
  tv exec @work {c-c}                           Send Ctrl+C
  tv exec @work {esc} \":wq!\" {cr}              Exit vim
  tv exec @work {up} {cr}                       Repeat last command
  tv exec @work \"npm start\" {cr} {wait:5s}     Wait for output to settle
  tv exec @work \"ssh prod\" {cr} {sleep:2s} \"ls\" {cr}

Key syntax:
  \"text\"          Literal text, sent as-is
  {cr} {enter}    Enter / Return
  {esc}           Escape
  {tab}           Tab,  {s-tab} Shift+Tab
  {c-KEY}         Ctrl modifier       e.g. {c-c} {c-d} {c-z}
  {a-KEY}         Alt modifier        e.g. {a-x}
  {s-KEY}         Shift modifier      e.g. {s-up}
  {up} {down} {left} {right}          Arrow keys
  {bs} {del} {home} {end} {pageup} {pagedown}
  {f1}..{f12}     Function keys
  {space} {nul}   Space / NUL byte
  {stdin}         Piped input (newlines → Enter, trailing newline stripped)
  {sleep:DUR}     Hard pause          e.g. {sleep:500ms} {sleep:2s}
  {wait:DUR}      Wait for output to settle (idle detection)
  {wait:DUR:T}    Wait with custom idle threshold T (default: 1s)
  {{ }}           Literal { and }

Durations: 500ms, 2s, 1m, 1h, 1m30s (bare number = milliseconds).

The command blocks until all keys are sent (including sleeps/waits).
A small delay (send_delay_ms, default 50ms) is inserted between each
space-separated argument to let applications process input.

Piping with {stdin}:
  echo \"hello\" | tv exec @s {stdin} {cr}
  printf 'line1\\nline2' | tv exec @s \"cat > file << 'EOF'\" {cr} {stdin} {cr} EOF {cr}

{stdin} reads piped input and sends it as keystrokes. Newlines become
Enter. One trailing newline is stripped. No brace escaping needed.
Can appear once. Requires piped input."
    )]
    Exec(ExecArgs),
}

#[derive(Subcommand)]
pub enum DaemonAction {
    /// Start the daemon (foreground)
    Start,
    /// Stop the daemon
    Stop,
    /// Restart the daemon
    Restart,
}

/// Session selector: 1-4 hex chars (id) or @tag
#[derive(Debug, Clone)]
pub enum Selector {
    Id(String),
    Tag(String),
}

impl FromStr for Selector {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(tag) = s.strip_prefix('@') {
            if tag.is_empty() {
                return Err("tag name cannot be empty".into());
            }
            Ok(Selector::Tag(tag.to_string()))
        } else if !s.is_empty() && s.len() <= 4 && s.chars().all(|c| c.is_ascii_hexdigit()) {
            Ok(Selector::Id(s.to_string()))
        } else {
            Err(format!("expected 1-4 hex chars or @tag, got '{}'", s))
        }
    }
}

impl std::fmt::Display for Selector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Selector::Id(id) => write!(f, "{}", id),
            Selector::Tag(tag) => write!(f, "@{}", tag),
        }
    }
}

/// Tag name, stored without @ prefix.
#[derive(Debug, Clone)]
pub struct Tag(pub String);

impl FromStr for Tag {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let name = s.strip_prefix('@').ok_or("tag must start with @")?;
        if name.is_empty() {
            return Err("tag name cannot be empty".into());
        }
        if !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            return Err("tag must be alphanumeric (with - or _)".into());
        }
        Ok(Tag(name.to_string()))
    }
}

impl std::fmt::Display for Tag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "@{}", self.0)
    }
}

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Filter by session (hex id or @tag)
    pub selector: Option<Selector>,
    /// Include hidden sessions
    #[arg(short = 'a', long = "all")]
    pub all: bool,
    /// (noop)
    #[arg(short = 'l', long = "long", hide = true)]
    pub long: bool,
    /// Output as JSON
    #[arg(short = 'j', long = "json")]
    pub json: bool,
    /// Output as human-readable
    #[arg(short = 'p', long = "pretty")]
    pub pretty: bool,
}

#[derive(Args, Debug)]
pub struct TagArgs {
    /// Target session (hex id or @tag), defaults to own session
    pub selector: Option<Selector>,
    /// New tag to assign (e.g. @work)
    pub new_tag: Option<Tag>,
    /// Remove tag from session
    #[arg(short = 'd', long = "delete")]
    pub delete: bool,
    /// Output as JSON
    #[arg(short = 'j', long = "json")]
    pub json: bool,
    /// Output as human-readable
    #[arg(short = 'p', long = "pretty")]
    pub pretty: bool,
}

#[derive(Args, Debug)]
pub struct PeekArgs {
    /// Target session (hex id or @tag), defaults to own session
    pub selector: Option<Selector>,
    /// Output as JSON (includes cursor position and dimensions)
    #[arg(short = 'j', long = "json")]
    pub json: bool,
    /// Output as human-readable
    #[arg(short = 'p', long = "pretty")]
    pub pretty: bool,
}

#[derive(Args, Debug)]
pub struct HistoryArgs {
    /// Target session (hex id or @tag), defaults to own session
    pub selector: Option<Selector>,
    /// Number of lines to return
    #[arg(short = 'n', long = "count")]
    pub count: Option<usize>,
    /// Skip N lines from the end
    #[arg(short = 'o', long = "offset")]
    pub offset: Option<usize>,
    /// Output as JSON
    #[arg(short = 'j', long = "json")]
    pub json: bool,
    /// Output as human-readable
    #[arg(short = 'p', long = "pretty")]
    pub pretty: bool,
}

#[derive(Args, Debug)]
pub struct ExecArgs {
    /// Target session (hex id or @tag)
    pub selector: Selector,
    /// Keys to execute (see 'tv exec --help' for syntax)
    #[arg(trailing_var_arg = true, required = true)]
    pub keys: Vec<String>,
}
