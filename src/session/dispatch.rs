//! Request dispatching and queue management for sessions.
//!
//! This module handles:
//! - Request classification (instant, read, exec)
//! - Permission checking and prompt queue management
//! - Read and exec queue management
//! - Cancellation handling
//! - PendingExec tick-based execution with sleep/wait support

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::KeyEvent;

use crate::config::DefaultPermission;
use crate::types::{ErrorCode, Request, Response, SessionRequest};

use super::prompt::PromptResult;
use crate::keys::{self, KeyGroup};

/// Insert `Sleep(delay)` between adjacent `Bytes` groups.
/// Gives target applications time to process each chunk of input.
/// No-op if delay is zero. Doesn't insert next to existing Sleep/Wait groups.
fn insert_exec_delays(groups: &mut Vec<KeyGroup>, delay: Duration) {
    if delay.is_zero() || groups.len() < 2 {
        return;
    }

    let mut result = Vec::with_capacity(groups.len() * 2);
    let mut prev_was_bytes = false;

    for group in groups.drain(..) {
        let is_bytes = matches!(group, KeyGroup::Bytes(_));
        if is_bytes && prev_was_bytes {
            result.push(KeyGroup::Sleep(delay));
        }
        prev_was_bytes = is_bytes;
        result.push(group);
    }

    *groups = result;
}

// === Permission Types ===

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionType {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestCategory {
    Instant,
    Read,
    Exec,
}

fn classify_request(request: &Request) -> RequestCategory {
    match request {
        // Instant: no permission, no queue
        Request::Show { .. }
        | Request::Hide { .. }
        | Request::Tag { .. }
        | Request::Clear { .. } => RequestCategory::Instant,

        // Read: read permission, read queue
        Request::Peek { .. } | Request::History { .. } => RequestCategory::Read,

        // Exec: write permission, exec queue
        Request::Exec { .. } => RequestCategory::Exec,

        // Server-side, should not reach session
        Request::List { .. } | Request::Stop => RequestCategory::Instant,
    }
}

fn permission_for_category(category: RequestCategory) -> Option<PermissionType> {
    match category {
        RequestCategory::Instant => None,
        RequestCategory::Read => Some(PermissionType::Read),
        RequestCategory::Exec => Some(PermissionType::Write),
    }
}

// === Prompt State ===

pub enum PromptState {
    None,
    Pending {
        request: SessionRequest,
        permission_type: PermissionType,
        selected: usize, // 0=Allow, 1=Session, 2=Deny
        expanded: bool,
        scroll_offset: usize,
    },
}

impl PromptState {
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending { .. })
    }
}

// === Permissions Tracking ===

pub struct Permissions {
    read_allowed: HashSet<String>,
    write_allowed: HashSet<String>,
}

impl Permissions {
    pub fn new() -> Self {
        Self {
            read_allowed: HashSet::new(),
            write_allowed: HashSet::new(),
        }
    }

    pub fn is_allowed(&self, source_id: &str, ptype: PermissionType) -> bool {
        match ptype {
            PermissionType::Read => self.read_allowed.contains(source_id),
            PermissionType::Write => self.write_allowed.contains(source_id),
        }
    }

    pub fn allow(&mut self, source_id: String, ptype: PermissionType) {
        match ptype {
            PermissionType::Read => self.read_allowed.insert(source_id),
            PermissionType::Write => self.write_allowed.insert(source_id),
        };
    }
}

// === Pending Exec State ===

/// Tracks the pause state of an exec that's currently blocked on a sleep or wait.
enum PendingPause {
    /// Hard sleep — resume at this instant.
    Sleep { resume_at: Instant },
    /// Soft wait — respond early if output settles, or at deadline.
    Wait {
        deadline: Instant,
        idle_threshold: Duration,
    },
}

/// State for an in-progress exec that may include sleep/wait pauses.
///
/// The event loop calls `tick_pending_exec()` every ~16ms to advance.
/// Between ticks, PTY reads update `dispatch.last_pty_activity` which the
/// wait logic checks to detect output quiescence.
pub struct PendingExec {
    request_id: u64,
    groups: Vec<KeyGroup>,
    index: usize,
    pause: Option<PendingPause>,
}

impl PendingExec {
    /// Whether this exec is fully complete (all groups processed, no pending pause).
    fn is_done(&self) -> bool {
        self.index >= self.groups.len() && self.pause.is_none()
    }
}

// === Dispatch State ===

/// Request dispatch state: queues, permissions, and in-flight request tracking.
///
/// Groups all fields related to dispatching client requests through the
/// permission → queue → execution pipeline.
pub struct DispatchState {
    pub prompt_queue: VecDeque<(SessionRequest, PermissionType)>,
    pub read_queue: VecDeque<SessionRequest>,
    pub exec_queue: VecDeque<SessionRequest>,
    pub current_read: Option<u64>,
    pub current_exec: Option<u64>,
    pub pending_exec: Option<PendingExec>,
    pub send_delay_ms: u64,
    pub idle_threshold_ms: u64,
    pub last_pty_activity: Instant,
    pub last_status_push: Instant,
    pub permissions: Permissions,
    pub prompt_state: PromptState,
    pub default_permission: DefaultPermission,
}

impl DispatchState {
    pub fn new(config: &crate::config::Config) -> Self {
        Self {
            prompt_queue: VecDeque::new(),
            read_queue: VecDeque::new(),
            exec_queue: VecDeque::new(),
            current_read: None,
            current_exec: None,
            pending_exec: None,
            send_delay_ms: config.send_delay_ms,
            idle_threshold_ms: config.idle_threshold_ms,
            last_pty_activity: Instant::now(),
            last_status_push: Instant::now(),
            permissions: Permissions::new(),
            prompt_state: PromptState::None,
            default_permission: config.default_permission,
        }
    }

    /// Clear all dispatch state (on disconnect).
    pub fn clear(&mut self) {
        self.prompt_queue.clear();
        self.read_queue.clear();
        self.exec_queue.clear();
        self.current_read = None;
        self.current_exec = None;
        self.pending_exec = None;
        self.prompt_state = PromptState::None;
    }
}

// === Session Dispatch Methods ===

impl super::Session {
    /// Dispatch an incoming request - classify, check permissions, queue or execute.
    pub fn dispatch_request(&mut self, req: SessionRequest) -> Result<()> {
        let category = classify_request(&req.request);
        let permission_type = permission_for_category(category);

        // Check if permission is needed and granted
        let prompt_type = match permission_type {
            None => None,
            Some(ptype) => match self.dispatch.default_permission {
                DefaultPermission::Allow => None,
                DefaultPermission::Deny => {
                    // Denied by policy - respond immediately
                    self.send_response(
                        req.id,
                        Response::err(ErrorCode::PermissionDenied, "denied by policy"),
                    )?;
                    return Ok(());
                }
                DefaultPermission::Prompt => {
                    if self.dispatch.permissions.is_allowed(&req.source_id, ptype) {
                        None
                    } else {
                        Some(ptype)
                    }
                }
            },
        };

        if let Some(ptype) = prompt_type {
            // Add to prompt queue
            self.dispatch.prompt_queue.push_back((req, ptype));
            self.try_show_prompt();
        } else {
            // Route directly to execution
            self.route_to_execution(req, category)?;
        }

        Ok(())
    }

    /// Try to show the next prompt if none is active.
    fn try_show_prompt(&mut self) {
        if self.dispatch.prompt_state.is_pending() {
            return; // Already showing a prompt
        }

        if let Some((request, permission_type)) = self.dispatch.prompt_queue.pop_front() {
            self.dispatch.prompt_state = PromptState::Pending {
                request,
                permission_type,
                selected: 0, // Default to Allow
                expanded: false,
                scroll_offset: 0,
            };
            self.view.mark_dirty();
        }
    }

    /// Handle prompt keyboard input and resolution.
    pub fn handle_prompt_resolution(&mut self, key: &KeyEvent, cols: u16, rows: u16) -> Result<()> {
        let (request, permission_type, selected, expanded, scroll_offset) =
            match std::mem::replace(&mut self.dispatch.prompt_state, PromptState::None) {
                PromptState::Pending {
                    request,
                    permission_type,
                    selected,
                    expanded,
                    scroll_offset,
                } => (request, permission_type, selected, expanded, scroll_offset),
                PromptState::None => return Ok(()),
            };

        let key_result = super::prompt::handle_prompt_key(key, selected, expanded, cols, rows);

        match key_result.result {
            PromptResult::Allow => {
                let category = classify_request(&request.request);
                self.route_to_execution(request, category)?;
            }
            PromptResult::AlwaysAllow => {
                self.dispatch
                    .permissions
                    .allow(request.source_id.clone(), permission_type);
                let category = classify_request(&request.request);
                self.route_to_execution(request, category)?;
            }
            PromptResult::Deny => {
                self.send_response(
                    request.id,
                    Response::err(ErrorCode::PermissionDenied, "denied by user"),
                )?;
            }
            PromptResult::Navigate => {
                let new_expanded = if key_result.toggle_expanded {
                    !expanded
                } else {
                    expanded
                };
                let new_scroll = if key_result.scroll_delta < 0 {
                    scroll_offset.saturating_sub((-key_result.scroll_delta) as usize)
                } else {
                    scroll_offset + key_result.scroll_delta as usize
                };
                // Clamp scroll to max (preserve position across hide/show)
                let new_scroll =
                    new_scroll.min(super::prompt::max_scroll_offset(&request.request, cols, rows));

                self.dispatch.prompt_state = PromptState::Pending {
                    request,
                    permission_type,
                    selected: key_result.selected,
                    expanded: new_expanded,
                    scroll_offset: new_scroll,
                };
                self.view.mark_dirty();
                return Ok(());
            }
        }

        self.dispatch.prompt_state = PromptState::None;
        self.view.mark_dirty();

        // Try to show next queued prompt
        self.try_show_prompt();

        Ok(())
    }

    /// Route a permitted request to execution based on its category.
    fn route_to_execution(&mut self, req: SessionRequest, category: RequestCategory) -> Result<()> {
        match category {
            RequestCategory::Instant => {
                // Execute immediately
                self.execute_and_respond(req)?;
            }
            RequestCategory::Read => {
                if self.dispatch.current_read.is_none() {
                    self.dispatch.current_read = Some(req.id);
                    self.execute_and_respond(req)?;
                } else {
                    self.dispatch.read_queue.push_back(req);
                }
            }
            RequestCategory::Exec => {
                if self.dispatch.current_exec.is_none() {
                    self.execute_exec(req)?;
                } else {
                    self.dispatch.exec_queue.push_back(req);
                }
            }
        }
        Ok(())
    }

    /// Execute a request and send response (for instant/read).
    fn execute_and_respond(&mut self, req: SessionRequest) -> Result<()> {
        let response = self.execute_request(&req);
        self.send_response(req.id, response)?;

        // If it was a read, clear and try advance
        if self.dispatch.current_read == Some(req.id) {
            self.dispatch.current_read = None;
            self.try_advance_read_queue()?;
        }

        Ok(())
    }

    /// Execute an exec request — parse keys, write to PTY, handle sleeps/waits.
    fn execute_exec(&mut self, req: SessionRequest) -> Result<()> {
        let (keys, stdin_text) = match &req.request {
            Request::Exec { keys, stdin, .. } => (keys.clone(), stdin.clone()),
            _ => unreachable!(),
        };

        // Parse keys (already validated at CLI time, but could come from other sources)
        let mut groups = match keys::parse_exec_args(&keys) {
            Ok(groups) => groups,
            Err(e) => {
                self.send_response(req.id, Response::err(ErrorCode::Internal, e))?;
                return Ok(());
            }
        };

        // Resolve {stdin} → Bytes with newline conversion (terminal paste behavior)
        for group in &mut groups {
            if matches!(group, KeyGroup::Stdin) {
                match &stdin_text {
                    Some(text) => {
                        let converted = text.replace("\r\n", "\r").replace('\n', "\r");
                        *group = KeyGroup::Bytes(converted.into_bytes());
                    }
                    None => {
                        self.send_response(
                            req.id,
                            Response::err(
                                ErrorCode::Internal,
                                "{stdin} used but no stdin data provided",
                            ),
                        )?;
                        return Ok(());
                    }
                }
                break; // Only one {stdin} allowed
            }
        }

        // Insert configured delay between adjacent byte groups
        insert_exec_delays(
            &mut groups,
            Duration::from_millis(self.dispatch.send_delay_ms),
        );

        self.dispatch.current_exec = Some(req.id);

        if groups.is_empty() {
            self.finish_exec(req.id)?;
            return Ok(());
        }

        let mut pending = PendingExec {
            request_id: req.id,
            groups,
            index: 0,
            pause: None,
        };

        let bytes = Self::advance_pending_exec(&mut pending, self.dispatch.idle_threshold_ms);
        if !bytes.is_empty() {
            self.queue_pty_write(&bytes);
        }

        if pending.is_done() {
            self.finish_exec(req.id)?;
        } else {
            self.dispatch.pending_exec = Some(pending);
        }

        Ok(())
    }

    /// Advance a pending exec: collect bytes until we hit a pause or finish.
    /// Returns bytes to write to the PTY.
    fn advance_pending_exec(pending: &mut PendingExec, idle_threshold_ms: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        while pending.index < pending.groups.len() {
            match &pending.groups[pending.index] {
                KeyGroup::Bytes(b) => {
                    bytes.extend_from_slice(b);
                    pending.index += 1;
                }
                KeyGroup::Sleep(duration) => {
                    pending.pause = Some(PendingPause::Sleep {
                        resume_at: Instant::now() + *duration,
                    });
                    pending.index += 1;
                    return bytes;
                }
                KeyGroup::Wait {
                    timeout,
                    idle_threshold,
                } => {
                    let threshold =
                        idle_threshold.unwrap_or(Duration::from_millis(idle_threshold_ms));
                    pending.pause = Some(PendingPause::Wait {
                        deadline: Instant::now() + *timeout,
                        idle_threshold: threshold,
                    });
                    pending.index += 1;
                    return bytes;
                }
                KeyGroup::Stdin => unreachable!("Stdin resolved before execution"),
            }
        }
        bytes
    }

    /// Called every event loop tick (~16ms). Advances pending execs past pauses.
    pub fn tick_pending_exec(&mut self) -> Result<()> {
        let mut pending = match self.dispatch.pending_exec.take() {
            Some(p) => p,
            None => return Ok(()),
        };

        // Check if we're paused
        if let Some(ref pause) = pending.pause {
            let now = Instant::now();
            let resume = match pause {
                PendingPause::Sleep { resume_at } => now >= *resume_at,
                PendingPause::Wait {
                    deadline,
                    idle_threshold,
                } => {
                    let idle = now.duration_since(self.dispatch.last_pty_activity);
                    idle >= *idle_threshold || now >= *deadline
                }
            };

            if !resume {
                self.dispatch.pending_exec = Some(pending);
                return Ok(());
            }

            // Pause is over — clear and continue
            pending.pause = None;
        }

        // Continue writing
        let bytes = Self::advance_pending_exec(&mut pending, self.dispatch.idle_threshold_ms);
        if !bytes.is_empty() {
            self.queue_pty_write(&bytes);
        }

        if pending.is_done() {
            self.finish_exec(pending.request_id)?;
        } else {
            self.dispatch.pending_exec = Some(pending);
        }

        Ok(())
    }

    /// Complete an exec — respond to client and advance the queue.
    fn finish_exec(&mut self, request_id: u64) -> Result<()> {
        let response = self.build_exec_response();
        self.send_response(request_id, response)?;
        self.dispatch.current_exec = None;
        self.try_advance_exec_queue()?;
        Ok(())
    }

    /// Try to advance the read queue.
    fn try_advance_read_queue(&mut self) -> Result<()> {
        if self.dispatch.current_read.is_none()
            && let Some(req) = self.dispatch.read_queue.pop_front() {
                self.dispatch.current_read = Some(req.id);
                self.execute_and_respond(req)?;
            }
        Ok(())
    }

    /// Try to advance the exec queue.
    fn try_advance_exec_queue(&mut self) -> Result<()> {
        if self.dispatch.current_exec.is_none()
            && let Some(req) = self.dispatch.exec_queue.pop_front() {
                self.execute_exec(req)?;
            }
        Ok(())
    }

    /// Handle a cancel message from daemon.
    pub fn handle_cancel(&mut self, request_id: u64) {
        // Check prompt queue
        self.dispatch
            .prompt_queue
            .retain(|(req, _)| req.id != request_id);

        // Check active prompt
        if let PromptState::Pending { request, .. } = &self.dispatch.prompt_state
            && request.id == request_id {
                self.dispatch.prompt_state = PromptState::None;
                self.view.mark_dirty();
                self.try_show_prompt();
            }

        // Check read queue
        self.dispatch.read_queue.retain(|req| req.id != request_id);

        // Check current read
        if self.dispatch.current_read == Some(request_id) {
            self.dispatch.current_read = None;
            let _ = self.try_advance_read_queue();
        }

        // Check exec queue
        self.dispatch.exec_queue.retain(|req| req.id != request_id);

        // Check current exec and pending exec
        if self.dispatch.current_exec == Some(request_id) {
            self.dispatch.current_exec = None;
            self.dispatch.pending_exec = None;
            let _ = self.try_advance_exec_queue();
        }
    }

    /// Execute a request and return response.
    pub fn execute_request(&mut self, req: &SessionRequest) -> Response {
        match &req.request {
            Request::Show { .. } => self.cmd_show(),
            Request::Hide { .. } => self.cmd_hide(),
            Request::Tag {
                new_tag, delete, ..
            } => self.cmd_tag(new_tag.clone(), *delete),
            Request::Clear { .. } => self.cmd_clear(),
            Request::Peek { .. } => self.cmd_peek(),
            Request::History { count, offset, .. } => self.cmd_history(*count, *offset),
            // Exec is handled by execute_exec(), not here
            _ => Response::err(ErrorCode::Internal, "unexpected request"),
        }
    }
}
