//! Session module - the terminal emulator process.
//!
//! The session is a mio-based process that:
//! 1. Runs a PTY with the user's shell
//! 2. Maintains terminal state (screen, scrollback)
//! 3. Connects to daemon for receiving requests
//! 4. Handles permission prompts
//! 5. Pushes status updates to daemon

pub mod commands;
pub mod dispatch;
pub mod prompt;
pub mod terminal;
pub mod text;

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::cursor::{SetCursorStyle, Show};
use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::ExecutableCommand;
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use portable_pty::{native_pty_system, Child as PtyChild, CommandBuilder, MasterPty};
use ratatui::prelude::*;
use signal_hook::consts::SIGWINCH;
use signal_hook_mio::v1_0::Signals;

use crate::config::{Config, DefaultVisibility};
use crate::ipc;
use crate::types::{
    AuthRequest, AuthResponse, DaemonMessage, Response, SessionMessage, StatusUpdate,
};

use dispatch::PromptState;
use prompt::PromptInfo;
use terminal::{Size, TerminalView};

const TOKEN_PTY: Token = Token(0);
const TOKEN_SIGNAL: Token = Token(1);
const TOKEN_IPC: Token = Token(2);

/// Set fd to non-blocking mode.
fn set_nonblocking(fd: RawFd) -> Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags == -1 {
            anyhow::bail!("fcntl F_GETFL: {}", io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
            anyhow::bail!("fcntl F_SETFL: {}", io::Error::last_os_error());
        }
    }
    Ok(())
}

// === PTY Write Buffer ===

/// A pending PTY write with partial-write tracking.
struct PtyWriteBuf {
    data: Vec<u8>,
    written: usize,
}

impl PtyWriteBuf {
    fn remaining(&self) -> &[u8] {
        &self.data[self.written..]
    }

    fn advance(&mut self, n: usize) {
        self.written += n;
    }

    fn is_done(&self) -> bool {
        self.written >= self.data.len()
    }
}

// === Session State ===

struct Session {
    // Identity
    session_id: Option<String>,
    tag: Option<String>,
    visible: bool,

    // PTY
    pty_master: Box<dyn MasterPty + Send>,
    pty_writer: Box<dyn Write + Send>,
    pty_write_queue: VecDeque<PtyWriteBuf>,
    child: Box<dyn PtyChild + Send + Sync>,

    // Terminal view (rendering, input, selection)
    view: TerminalView,

    // Text parsing for history
    text_parser: text::TextParser,

    // IPC
    ipc_conn: ipc::session::Connection,
    ipc_authenticated: bool,
    ipc_registered: bool,

    // Request dispatch (queues, permissions, prompts)
    dispatch: dispatch::DispatchState,

    // Status tracking (for pushing to server)
    cwd: Option<String>,
}

impl Session {
    fn new(
        conn: ipc::session::Connection,
        config: &Config,
        tag: Option<String>,
    ) -> Result<(Self, RawFd)> {
        let size = Size::get();

        // PTY setup
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(size.pty())?;
        let mut cmd = CommandBuilder::new_default_prog();
        cmd.cwd(std::env::current_dir()?);
        cmd.env("TERM", "xterm-256color");
        cmd.env("TV_SESSION", "1");

        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let pty_fd = pair.master.as_raw_fd().expect("master PTY fd after openpty");
        let pty_writer = pair.master.take_writer()?;
        set_nonblocking(pty_fd)?;

        // Terminal view (alacritty state, selection, rendering)
        let view = TerminalView::new(size);

        // Text parser for history
        let text_parser = text::TextParser::new(
            size.rows as usize,
            size.cols as usize,
            config.history_size,
        );

        let visible = config.default_visibility == DefaultVisibility::Visible;

        // Get initial cwd
        let cwd = std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned());

        Ok((
            Self {
                session_id: None,
                tag,
                visible,
                pty_master: pair.master,
                pty_writer,
                pty_write_queue: VecDeque::new(),
                child,
                view,
                text_parser,
                ipc_conn: conn,
                ipc_authenticated: false,
                ipc_registered: false,
                dispatch: dispatch::DispatchState::new(config),
                cwd,
            },
            pty_fd,
        ))
    }

    // === PTY I/O ===

    /// Queue bytes to write to the PTY. Never blocks.
    ///
    /// Data is buffered and flushed when the PTY fd becomes writable (via mio).
    /// This keeps the event loop responsive during large pastes and when the
    /// child process is slow to read.
    fn queue_pty_write(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.pty_write_queue.push_back(PtyWriteBuf {
            data: bytes.to_vec(),
            written: 0,
        });
    }

    /// Flush queued PTY writes. Called when mio reports the PTY fd is writable.
    ///
    /// Writes as much as possible without blocking — stops on WouldBlock and
    /// lets the event loop continue. Remaining data stays queued for next flush.
    fn flush_pty_writes(&mut self) -> Result<()> {
        while let Some(buf) = self.pty_write_queue.front_mut() {
            match self.pty_writer.write(buf.remaining()) {
                Ok(0) => break,
                Ok(n) => {
                    buf.advance(n);
                    if buf.is_done() {
                        self.pty_write_queue.pop_front();
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Whether there are pending PTY writes waiting to be flushed.
    fn has_pending_pty_writes(&self) -> bool {
        !self.pty_write_queue.is_empty()
    }

    /// Handle PTY read - feed data to terminal view and text parser.
    fn handle_pty_read(&mut self, pty_reader: &mut dyn Read, buf: &mut [u8]) -> Result<bool> {
        loop {
            match pty_reader.read(buf) {
                Ok(0) => return Ok(false), // PTY closed
                Ok(n) => {
                    let data = &buf[..n];

                    // Feed to terminal view for rendering
                    self.view.feed_pty_output(data);

                    // Feed to text parser for history
                    self.text_parser.advance(data);

                    self.dispatch.last_pty_activity = Instant::now();
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Ok(false),
            }
        }

        // Push activity status to daemon (debounced)
        self.maybe_push_status()?;

        Ok(true)
    }

    /// Process terminal events from alacritty (PTY writes, title, bell, etc.).
    fn drain_terminal_events(&mut self) {
        for event in self.view.drain_terminal_events() {
            match event {
                terminal::TerminalEvent::PtyWrite(bytes) => {
                    self.queue_pty_write(&bytes);
                }
                terminal::TerminalEvent::Title(title) => {
                    // Forward title to outer terminal via OSC 2
                    let seq = format!("\x1b]2;{}\x1b\\", title);
                    let _ = io::stdout().write_all(seq.as_bytes());
                }
                terminal::TerminalEvent::ResetTitle => {
                    let _ = io::stdout().write_all(b"\x1b]2;\x1b\\");
                }
                terminal::TerminalEvent::Bell => {
                    let _ = io::stdout().write_all(b"\x07");
                }
            }
        }
    }

    // === Status Push ===

    /// Push status to daemon if enough time has passed since last push (debounced ~2s).
    fn maybe_push_status(&mut self) -> Result<()> {
        if !self.ipc_authenticated {
            return Ok(());
        }

        let now = Instant::now();
        if now.duration_since(self.dispatch.last_status_push) < Duration::from_secs(2) {
            return Ok(());
        }

        self.update_cwd();
        self.push_status(now)
    }

    /// Resolve CWD from the foreground process in the PTY.
    fn update_cwd(&mut self) {
        if let Some(fg_pid) = self.pty_master.process_group_leader()
            && let Some(cwd) = crate::auth::get_process_cwd(fg_pid as u32)
        {
            self.cwd = Some(cwd);
        }
    }

    /// Push current status to the server unconditionally.
    fn push_status(&mut self, now: Instant) -> Result<()> {
        if !self.ipc_authenticated {
            return Ok(());
        }

        self.dispatch.last_status_push = now;

        self.ipc_conn.queue_send(&SessionMessage::Status {
            update: StatusUpdate {
                cwd: self.cwd.clone(),
                in_alt_screen: self.text_parser.in_alt_screen(),
            },
        })?;

        Ok(())
    }

    // === Resize ===

    /// Handle terminal resize.
    fn handle_resize(&mut self) {
        let size = self.view.resize();
        self.text_parser
            .resize(size.rows as usize, size.cols as usize);
        let _ = self.pty_master.resize(size.pty());
    }

    // === IPC ===

    /// Handle IPC events.
    fn handle_ipc(&mut self, poll: &Poll) -> Result<()> {
        // Read available data
        if !self.ipc_conn.read_into_buffer()? {
            // Disconnected - deregister BEFORE dropping socket (critical ordering)
            if let Some(fd) = self.ipc_conn.as_raw_fd() {
                let _ = poll.registry().deregister(&mut SourceFd(&fd));
            }
            self.ipc_conn.handle_disconnect();
            self.ipc_registered = false;
            self.ipc_authenticated = false;
            self.dispatch.clear(); // Clear all queues and pending state
            self.view.mark_dirty();
            return Ok(());
        }

        // Handle authentication
        if !self.ipc_authenticated
            && let Some(auth_req) = self.ipc_conn.try_recv::<AuthRequest>()?
        {
            // First connect: use server's ID. Reconnect: use our stored ID.
            let id_to_send = self.session_id.clone().unwrap_or_default();
            let use_server_id = id_to_send.is_empty();

            self.ipc_conn.queue_send(&AuthResponse::Session {
                id: if use_server_id {
                    auth_req.id.clone()
                } else {
                    id_to_send
                },
                pid: std::process::id(),
                tag: self.tag.clone(),
                visible: self.visible,
            })?;

            // Store the ID we're using
            if use_server_id {
                self.session_id = Some(auth_req.id);
            }

            self.ipc_authenticated = true;
            self.view.mark_dirty();

            // Push initial status to server
            self.push_status(Instant::now())?;
        }

        // Handle incoming messages (when authenticated)
        if self.ipc_authenticated {
            while let Some(msg) = self.ipc_conn.try_recv::<DaemonMessage>()? {
                match msg {
                    DaemonMessage::Request(req) => {
                        self.dispatch_request(req)?;
                    }
                    DaemonMessage::Cancel { id } => {
                        self.handle_cancel(id);
                    }
                }
            }
        }

        // Flush any pending writes
        self.ipc_conn.flush()?;

        Ok(())
    }

    /// Send response back to daemon.
    pub fn send_response(&mut self, id: u64, result: Response) -> Result<()> {
        self.ipc_conn
            .queue_send(&SessionMessage::Response { id, result })?;
        Ok(())
    }

    // === Input ===

    /// Handle keyboard and mouse input.
    fn handle_input(&mut self) -> Result<()> {
        loop {
            match event::poll(Duration::ZERO) {
                Ok(false) => break,
                Ok(true) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }

            let ev = match event::read() {
                Ok(ev) => ev,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };

            match ev {
                Event::Key(key) => {
                    // If prompting, handle prompt keys only - swallow all others
                    if self.dispatch.prompt_state.is_pending() {
                        let size = self.view.size();
                        self.handle_prompt_resolution(&key, size.cols, size.rows)?;
                        continue;
                    }

                    if let Some(bytes) = self.view.handle_key(&key) {
                        self.queue_pty_write(&bytes);
                    }
                }
                Event::Mouse(mouse) => {
                    // If prompting, ignore mouse events
                    if self.dispatch.prompt_state.is_pending() {
                        continue;
                    }

                    if let Some(bytes) = self.view.handle_mouse(&mouse) {
                        self.queue_pty_write(&bytes);
                    }
                }
                Event::Paste(text) => {
                    if self.dispatch.prompt_state.is_pending() {
                        continue;
                    }

                    let bytes = self.view.encode_paste(&text);
                    if !bytes.is_empty() {
                        self.queue_pty_write(&bytes);
                    }
                }
                Event::FocusGained => {
                    if self.view.wants_focus_events() {
                        self.queue_pty_write(b"\x1b[I");
                    }
                }
                Event::FocusLost => {
                    if self.view.wants_focus_events() {
                        self.queue_pty_write(b"\x1b[O");
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    // === Reconnect ===

    /// Try to reconnect to daemon if disconnected.
    fn try_reconnect(&mut self, poll: &Poll) -> Result<()> {
        if self.ipc_registered || !self.ipc_conn.should_retry() {
            return Ok(());
        }

        if self.ipc_conn.try_reconnect()?
            && let Some(fd) = self.ipc_conn.as_raw_fd()
        {
            poll.registry().register(
                &mut SourceFd(&fd),
                TOKEN_IPC,
                Interest::READABLE | Interest::WRITABLE,
            )?;
            self.ipc_registered = true;
            self.ipc_authenticated = false;
            self.view.mark_dirty();
        }

        Ok(())
    }

    // === Render ===

    /// Render terminal and prompt overlay.
    fn render(&mut self, rterm: &mut ratatui::Terminal<CrosstermBackend<&mut io::Stdout>>) -> Result<()> {
        let prompt_info = match &self.dispatch.prompt_state {
            PromptState::Pending {
                request,
                permission_type,
                selected,
                expanded,
                scroll_offset,
            } => Some(PromptInfo {
                source_id: request.source_id.clone(),
                source_tag: request.source_tag.clone(),
                permission_type: *permission_type,
                request: request.request.clone(),
                selected: *selected,
                expanded: *expanded,
                scroll_offset: *scroll_offset,
            }),
            PromptState::None => None,
        };

        self.view.render(rterm, prompt_info.as_ref())
    }
}

// === Main Entry Point ===

/// Run a terminal session with an established daemon connection.
pub fn run(conn: ipc::session::Connection, config: &Config, tag: Option<String>) -> Result<()> {
    let (mut session, pty_fd) = Session::new(conn, config, tag)?;
    let mut pty_reader = session.pty_master.try_clone_reader()?;

    // Polling setup
    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(64);
    let mut signals = Signals::new([SIGWINCH])?;

    poll.registry()
        .register(&mut SourceFd(&pty_fd), TOKEN_PTY, Interest::READABLE)?;
    poll.registry()
        .register(&mut signals, TOKEN_SIGNAL, Interest::READABLE)?;

    // Register IPC if connected
    if let Some(fd) = session.ipc_conn.as_raw_fd() {
        poll.registry().register(
            &mut SourceFd(&fd),
            TOKEN_IPC,
            Interest::READABLE | Interest::WRITABLE,
        )?;
        session.ipc_registered = true;
    }

    // Ratatui terminal
    let mut stdout = io::stdout();

    // Install panic hook to restore terminal state on crash
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            io::stderr(),
            crossterm::terminal::EndSynchronizedUpdate,
            DisableMouseCapture,
            DisableBracketedPaste,
            DisableFocusChange,
            SetCursorStyle::DefaultUserShape,
            Show,
        );
        original_hook(info);
    }));

    enable_raw_mode()?;
    stdout.execute(EnableMouseCapture)?;
    stdout.execute(EnableBracketedPaste)?;
    stdout.execute(EnableFocusChange)?;
    let backend = CrosstermBackend::new(&mut stdout);
    let mut rterm = ratatui::Terminal::new(backend)?;
    rterm.clear()?;

    let mut pty_buf = [0u8; 16384];

    let result: Result<()> = (|| {
        loop {
            // Poll with short timeout for responsive UI
            match poll.poll(&mut events, Some(Duration::from_millis(16))) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }

            // Handle mio events
            for ev in events.iter() {
                match ev.token() {
                    TOKEN_PTY => {
                        if ev.is_readable()
                            && !session.handle_pty_read(&mut *pty_reader, &mut pty_buf)?
                        {
                            return Ok(()); // PTY closed, exit
                        }
                        if ev.is_writable() {
                            session.flush_pty_writes()?;
                        }
                    }
                    TOKEN_SIGNAL => {
                        for _ in signals.pending() {
                            session.handle_resize();
                        }
                    }
                    TOKEN_IPC => {
                        if ev.is_readable() {
                            session.handle_ipc(&poll)?;
                        }
                        if ev.is_writable() && session.ipc_conn.has_pending_writes() {
                            session.ipc_conn.flush()?;
                        }
                    }
                    _ => {}
                }
            }

            // Check if child process has exited
            if let Ok(Some(_status)) = session.child.try_wait() {
                // Drain any remaining PTY output before exiting
                let _ = session.handle_pty_read(&mut *pty_reader, &mut pty_buf);
                return Ok(());
            }

            // Advance pending execs (handles sleep timing)
            session.tick_pending_exec()?;

            // Continuous auto-scroll during drag at edges
            session.view.tick_scroll_drag();

            // Process terminal events from alacritty (PTY writes, title, bell)
            session.drain_terminal_events();

            // Handle keyboard and mouse input
            session.handle_input()?;

            // Flush any responses queued during keyboard handling (e.g., prompt responses)
            if session.ipc_conn.has_pending_writes() {
                session.ipc_conn.flush()?;
            }

            // Handle IPC reconnection
            session.try_reconnect(&poll)?;

            // Update PTY write interest based on queue state
            let pty_interest = if session.has_pending_pty_writes() {
                Interest::READABLE | Interest::WRITABLE
            } else {
                Interest::READABLE
            };
            poll.registry()
                .reregister(&mut SourceFd(&pty_fd), TOKEN_PTY, pty_interest)?;

            // Render if dirty
            if session.view.is_dirty() {
                session.render(&mut rterm)?;
            }
        }
    })();

    // Cleanup
    drop(rterm);
    let _ = stdout.execute(DisableBracketedPaste);
    let _ = stdout.execute(DisableMouseCapture);
    let _ = stdout.execute(DisableFocusChange);
    let _ = stdout.execute(SetCursorStyle::DefaultUserShape);
    let _ = stdout.execute(Show);
    disable_raw_mode()?;

    result
}
