//! Server module - the daemon process.
//!
//! The server is an async tokio process that:
//! 1. Listens for incoming connections
//! 2. Sends AuthRequest to each connection
//! 3. Routes requests between clients and sessions
//! 4. Caches session info for tv list
//!
//! The server is a pure router - all queuing happens session-side.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Grace period after daemon startup during which client requests are deferred,
/// giving existing sessions time to reconnect after a daemon restart.
const SETTLING_DURATION: Duration = Duration::from_millis(500);

use crate::auth;
use crate::config::Config;
use crate::ipc;
use crate::types::*;

type ConnId = u64;

// === Data Structures ===

struct ServerState {
    sessions: HashMap<String, SessionEntry>,
    clients: HashMap<ConnId, ClientEntry>,
    conn_to_session: HashMap<ConnId, String>,
    next_conn_id: ConnId,
    next_request_id: u64,
    /// Deadline for the settling window. `Some` means we're still settling.
    /// Set on startup, cleared after timeout expires and deferred clients are drained.
    settling_deadline: Option<tokio::time::Instant>,
    /// Client requests deferred during the settling window.
    deferred: Vec<DeferredClient>,
}

struct DeferredClient {
    conn_id: ConnId,
    pid: u32,
    request: Request,
    response_tx: oneshot::Sender<Response>,
}

struct SessionEntry {
    conn_id: ConnId,
    info: SessionInfo,
    cmd_tx: mpsc::Sender<SessionCommand>,
}

struct ClientEntry {
    response_tx: oneshot::Sender<Response>,
    session_id: String,
    request_id: u64,
    request: Request,
}

enum ServerMessage {
    ClientReady {
        conn_id: ConnId,
        pid: u32,
        request: Request,
        response_tx: oneshot::Sender<Response>,
    },
    SessionReady {
        conn_id: ConnId,
        id: String,
        pid: u32,
        tag: Option<String>,
        visible: bool,
        cmd_tx: mpsc::Sender<SessionCommand>,
    },
    SessionResponse {
        conn_id: ConnId,
        request_id: u64,
        result: Response,
    },
    SessionStatus {
        conn_id: ConnId,
        update: StatusUpdate,
    },
    Disconnected {
        conn_id: ConnId,
    },
}

enum SessionCommand {
    SendRequest(SessionRequest),
    CancelRequest(u64),
    Close,
}

// === Entry Point ===

/// Run the daemon server (foreground).
pub fn run(config: &Config) -> Result<()> {
    // Initialize tracing
    let level: tracing::Level = config.log_level.into();
    tracing_subscriber::fmt().with_max_level(level).init();

    info!("starting tv daemon");

    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
    rt.block_on(async_run(config))
}

async fn async_run(_config: &Config) -> Result<()> {
    let listener = ipc::server::Listener::bind().await?;
    info!(path = %ipc::socket_path().display(), "listening");

    let (server_tx, mut server_rx) = mpsc::channel::<ServerMessage>(256);
    let mut state = ServerState::new();
    let shutdown_token = CancellationToken::new();

    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        // Settling deadline for the sleep_until arm. The `if` guard on the arm
        // prevents polling when settling_deadline is None, so the unwrap_or
        // value is never actually used.
        let deadline = state.settling_deadline
            .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(1));

        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok(conn) => {
                        let conn_id = state.next_conn_id();
                        let assigned_id = state.generate_session_id();
                        let tx = server_tx.clone();
                        debug!(conn_id, "accepted connection");
                        tokio::spawn(handle_connection(conn_id, conn, assigned_id, tx));
                    }
                    Err(e) => {
                        debug!(error = %e, "failed to accept connection");
                    }
                }
            }
            Some(msg) = server_rx.recv() => {
                state.handle_message(msg, &shutdown_token).await;
            }
            _ = tokio::time::sleep_until(deadline), if state.settling_deadline.is_some() => {
                state.drain_deferred().await;
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM, shutting down");
                state.shutdown().await;
                shutdown_token.cancel();
            }
            _ = tokio::signal::ctrl_c() => {
                info!("received SIGINT, shutting down");
                state.shutdown().await;
                shutdown_token.cancel();
            }
            _ = shutdown_token.cancelled() => {
                info!("daemon shutdown complete");
                break;
            }
        }
    }

    Ok(())
}

// === Connection Handlers ===

async fn handle_connection(
    conn_id: ConnId,
    mut conn: ipc::server::Connection,
    assigned_id: String,
    server_tx: mpsc::Sender<ServerMessage>,
) {
    // Send AuthRequest
    if let Err(e) = conn.send(&AuthRequest { id: assigned_id.clone() }).await {
        debug!(conn_id, error = %e, "failed to send auth request");
        let _ = server_tx.send(ServerMessage::Disconnected { conn_id }).await;
        return;
    }

    // Receive AuthResponse
    let auth: AuthResponse = match conn.recv().await {
        Ok(a) => a,
        Err(e) => {
            debug!(conn_id, error = %e, "failed to receive auth response");
            let _ = server_tx.send(ServerMessage::Disconnected { conn_id }).await;
            return;
        }
    };

    // Verify claimed PID matches socket peer PID
    let peer_pid = conn.peer_pid();
    let claimed_pid = match &auth {
        AuthResponse::Client { pid, .. } => *pid,
        AuthResponse::Session { pid, .. } => *pid,
    };

    if claimed_pid != peer_pid {
        warn!(
            conn_id,
            claimed_pid,
            peer_pid,
            "pid mismatch - rejecting connection"
        );
        let _ = conn
            .send(&Response::err(ErrorCode::AuthFailed, "pid mismatch"))
            .await;
        let _ = server_tx.send(ServerMessage::Disconnected { conn_id }).await;
        return;
    }

    match auth {
        AuthResponse::Client { id: _, pid, request } => {
            handle_client(conn_id, conn, pid, request, server_tx).await;
        }
        AuthResponse::Session { id, pid, tag, visible } => {
            let session_id = if !id.is_empty() { id } else { assigned_id };
            handle_session(conn_id, conn, session_id, pid, tag, visible, server_tx).await;
        }
    }
}

async fn handle_client(
    conn_id: ConnId,
    mut conn: ipc::server::Connection,
    pid: u32,
    request: Request,
    server_tx: mpsc::Sender<ServerMessage>,
) {
    info!(conn_id, pid, ?request, "client request");

    let (response_tx, response_rx) = oneshot::channel();

    if server_tx
        .send(ServerMessage::ClientReady {
            conn_id,
            pid,
            request,
            response_tx,
        })
        .await
        .is_err()
    {
        return;
    }

    // Wait for response OR client disconnect
    tokio::select! {
        result = response_rx => {
            if let Ok(response) = result {
                let _ = conn.send(&response).await;
            }
        }
        _ = conn.closed() => {
            debug!(conn_id, "client disconnected while waiting for response");
        }
    }

    let _ = server_tx.send(ServerMessage::Disconnected { conn_id }).await;
}

async fn handle_session(
    conn_id: ConnId,
    mut conn: ipc::server::Connection,
    session_id: String,
    pid: u32,
    tag: Option<String>,
    visible: bool,
    server_tx: mpsc::Sender<ServerMessage>,
) {
    info!(conn_id, session_id, pid, ?tag, visible, "session registered");

    let (cmd_tx, mut cmd_rx) = mpsc::channel::<SessionCommand>(32);

    if server_tx
        .send(ServerMessage::SessionReady {
            conn_id,
            id: session_id.clone(),
            pid,
            tag,
            visible,
            cmd_tx,
        })
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    SessionCommand::SendRequest(req) => {
                        debug!(conn_id, request_id = req.id, "sending request to session");
                        let msg = DaemonMessage::Request(req);
                        if conn.send(&msg).await.is_err() {
                            break;
                        }
                    }
                    SessionCommand::CancelRequest(id) => {
                        debug!(conn_id, request_id = id, "sending cancel to session");
                        let msg = DaemonMessage::Cancel { id };
                        if conn.send(&msg).await.is_err() {
                            break;
                        }
                    }
                    SessionCommand::Close => {
                        debug!(conn_id, "closing session connection");
                        break;
                    }
                }
            }
            result = conn.recv::<SessionMessage>() => {
                match result {
                    Ok(SessionMessage::Response { id, result }) => {
                        debug!(conn_id, request_id = id, ok = result.ok, "session response");
                        let _ = server_tx.send(ServerMessage::SessionResponse {
                            conn_id, request_id: id, result
                        }).await;
                    }
                    Ok(SessionMessage::Status { update }) => {
                        debug!(conn_id, ?update, "session status update");
                        let _ = server_tx.send(ServerMessage::SessionStatus {
                            conn_id, update
                        }).await;
                    }
                    Err(e) => {
                        debug!(conn_id, error = %e, "session connection error");
                        break;
                    }
                }
            }
        }
    }

    info!(conn_id, session_id, "session disconnected");
    let _ = server_tx.send(ServerMessage::Disconnected { conn_id }).await;
}

// === ServerState Implementation ===

impl ServerState {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            clients: HashMap::new(),
            conn_to_session: HashMap::new(),
            next_conn_id: 1,
            next_request_id: 1,
            settling_deadline: Some(tokio::time::Instant::now() + SETTLING_DURATION),
            deferred: Vec::new(),
        }
    }

    fn next_conn_id(&mut self) -> ConnId {
        let id = self.next_conn_id;
        self.next_conn_id += 1;
        id
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }

    fn generate_session_id(&self) -> String {
        use rand::Rng;
        let mut rng = rand::rng();
        loop {
            let id: String = (0..4).map(|_| format!("{:x}", rng.random::<u8>() & 0xf)).collect();
            if !self.sessions.contains_key(&id) {
                return id;
            }
        }
    }

    async fn handle_message(&mut self, msg: ServerMessage, shutdown_token: &CancellationToken) {
        match msg {
            ServerMessage::ClientReady {
                conn_id,
                pid,
                request,
                response_tx,
            } => {
                self.handle_client_request(conn_id, pid, request, response_tx, shutdown_token)
                    .await;
            }
            ServerMessage::SessionReady {
                conn_id,
                id,
                pid,
                tag,
                visible,
                cmd_tx,
            } => {
                self.register_session(conn_id, id, pid, tag, visible, cmd_tx);
            }
            ServerMessage::SessionResponse {
                conn_id,
                request_id,
                result,
            } => {
                self.handle_session_response(conn_id, request_id, result);
            }
            ServerMessage::SessionStatus { conn_id, update } => {
                self.handle_session_status(conn_id, update);
            }
            ServerMessage::Disconnected { conn_id } => {
                self.handle_disconnect(conn_id).await;
            }
        }
    }

    fn register_session(
        &mut self,
        conn_id: ConnId,
        id: String,
        pid: u32,
        tag: Option<String>,
        visible: bool,
        cmd_tx: mpsc::Sender<SessionCommand>,
    ) {
        let info = SessionInfo::new(id.clone(), pid, tag, visible);
        let entry = SessionEntry {
            conn_id,
            info,
            cmd_tx,
        };
        self.sessions.insert(id.clone(), entry);
        self.conn_to_session.insert(conn_id, id);
    }

    async fn handle_client_request(
        &mut self,
        conn_id: ConnId,
        pid: u32,
        request: Request,
        response_tx: oneshot::Sender<Response>,
        shutdown_token: &CancellationToken,
    ) {
        // Stop is always handled immediately — daemon management command
        if matches!(request, Request::Stop) {
            info!("received stop request, shutting down");
            let _ = response_tx.send(Response::ok_empty());
            self.shutdown().await;
            shutdown_token.cancel();
            return;
        }

        // During settling, defer all requests until sessions have reconnected
        if self.settling_deadline.is_some() {
            debug!(conn_id, pid, ?request, "deferring request (settling)");
            self.deferred.push(DeferredClient {
                conn_id,
                pid,
                request,
                response_tx,
            });
            return;
        }

        self.route_client_request(conn_id, pid, request, response_tx)
            .await;
    }

    /// Route an authenticated client request to the appropriate handler.
    /// Separated from handle_client_request so drain_deferred can reuse it.
    async fn route_client_request(
        &mut self,
        conn_id: ConnId,
        pid: u32,
        request: Request,
        response_tx: oneshot::Sender<Response>,
    ) {
        // List is exempt from auth — it's a global query, not session-specific
        if let Request::List { ref selector, all } = request {
            let result = self.handle_list(selector.as_ref(), all);
            info!(conn_id, ok = result.ok, "client response");
            let _ = response_tx.send(result);
            return;
        }

        // Walk the client's process tree to find which tv session it belongs to.
        // This ensures clients can only interact with sessions they are descendants of.
        let source_session = auth::find_ancestor_pid(pid, |p| {
            self.sessions
                .values()
                .find(|s| s.info.pid == p)
                .map(|s| (s.info.id.clone(), s.info.tag.clone()))
        });

        let Some((source_id, source_tag)) = source_session else {
            info!(conn_id, pid, "client auth failed: not inside a tv session");
            let _ = response_tx.send(Response::err(
                ErrorCode::AuthFailed,
                "not inside a tv session",
            ));
            return;
        };

        match &request {
            Request::Tag {
                selector,
                new_tag,
                delete,
            } if new_tag.is_none() && !delete => {
                // Tag query mode - handle server-side
                let result = self.handle_tag_query(selector.as_ref(), pid);
                info!(conn_id, ok = result.ok, "client response (tag query)");
                let _ = response_tx.send(result);
            }
            _ => {
                let selector = extract_selector(&request);
                match self.resolve_selector(selector, pid) {
                    Ok(session_id) => {
                        self.forward_request(
                            conn_id,
                            session_id,
                            source_id,
                            source_tag,
                            request,
                            response_tx,
                        )
                        .await;
                    }
                    Err(code) => {
                        let result = Response::err(code, code.to_string());
                        info!(conn_id, ok = result.ok, "client response");
                        let _ = response_tx.send(result);
                    }
                }
            }
        }
    }

    /// Process all deferred client requests. Called when the settling window expires.
    async fn drain_deferred(&mut self) {
        self.settling_deadline = None;
        let deferred = std::mem::take(&mut self.deferred);
        let count = deferred.len();
        if count > 0 {
            info!(count, "settling complete, processing deferred requests");
        }
        for client in deferred {
            self.route_client_request(client.conn_id, client.pid, client.request, client.response_tx)
                .await;
        }
    }

    fn resolve_selector(
        &self,
        selector: Option<&Selector>,
        client_pid: u32,
    ) -> Result<String, ErrorCode> {
        match selector {
            Some(Selector::Tag(tag)) => {
                let matches: Vec<_> = self
                    .sessions
                    .values()
                    .filter(|s| s.info.tag.as_deref() == Some(tag.as_str()))
                    .collect();
                match matches.len() {
                    0 => Err(ErrorCode::SessionNotFound),
                    1 => Ok(matches[0].info.id.clone()),
                    _ => Err(ErrorCode::AmbiguousSelector),
                }
            }
            Some(Selector::Id(id)) => {
                let matches: Vec<_> = self
                    .sessions
                    .values()
                    .filter(|s| s.info.id.starts_with(id))
                    .collect();
                match matches.len() {
                    0 => Err(ErrorCode::SessionNotFound),
                    1 => Ok(matches[0].info.id.clone()),
                    _ => Err(ErrorCode::AmbiguousSelector),
                }
            }
            None => auth::find_ancestor_pid(client_pid, |pid| {
                self.sessions
                    .values()
                    .find(|s| s.info.pid == pid)
                    .map(|s| s.info.id.clone())
            })
            .ok_or(ErrorCode::SessionNotFound),
        }
    }

    /// Forward a request immediately to the session. Session handles all queuing.
    async fn forward_request(
        &mut self,
        client_conn_id: ConnId,
        session_id: String,
        source_id: String,
        source_tag: Option<String>,
        request: Request,
        response_tx: oneshot::Sender<Response>,
    ) {
        let request_id = self.next_request_id();

        self.clients.insert(
            client_conn_id,
            ClientEntry {
                response_tx,
                session_id: session_id.clone(),
                request_id,
                request: request.clone(),
            },
        );

        let Some(session) = self.sessions.get_mut(&session_id) else {
            warn!(session_id, "session disappeared before forwarding");
            let _ = self.clients.remove(&client_conn_id);
            return;
        };
        let req = SessionRequest {
            id: request_id,
            source_id,
            source_tag,
            request,
        };

        debug!(request_id, session_id, "forwarding request to session");
        let _ = session.cmd_tx.send(SessionCommand::SendRequest(req)).await;
    }

    /// Handle response from session - find client and relay.
    fn handle_session_response(&mut self, conn_id: ConnId, request_id: u64, result: Response) {
        let session_id = match self.conn_to_session.get(&conn_id) {
            Some(id) => id.clone(),
            None => return,
        };

        // Find client by request_id and relay response
        let client_conn_id = self
            .clients
            .iter()
            .find(|(_, c)| c.request_id == request_id)
            .map(|(id, _)| *id);

        if let Some(client) = client_conn_id.and_then(|id| self.clients.remove(&id)) {
            if result.ok {
                self.update_cache_from_request(&session_id, &client.request);
            }
            info!(ok = result.ok, "client response");
            let _ = client.response_tx.send(result);
        }
    }

    fn update_cache_from_request(&mut self, session_id: &str, request: &Request) {
        let session = match self.sessions.get_mut(session_id) {
            Some(s) => s,
            None => return,
        };

        match request {
            Request::Show { .. } => {
                session.info.visible = true;
            }
            Request::Hide { .. } => {
                session.info.visible = false;
            }
            Request::Tag { new_tag, delete, .. } => {
                if *delete {
                    session.info.tag = None;
                } else {
                    session.info.tag = new_tag.clone();
                }
            }
            _ => {}
        }
    }

    fn handle_session_status(&mut self, conn_id: ConnId, update: StatusUpdate) {
        if let Some(session) = self
            .conn_to_session
            .get(&conn_id)
            .and_then(|id| self.sessions.get_mut(id))
        {
            session.info.update_status(&update);
        }
    }

    async fn handle_disconnect(&mut self, conn_id: ConnId) {
        // Session disconnect - fail all clients targeting this session.
        // Only remove the session if it's still on this connection (not already reconnected).
        if let Some(session_id) = self.conn_to_session.remove(&conn_id) {
            let should_remove = self
                .sessions
                .get(&session_id)
                .is_some_and(|s| s.conn_id == conn_id);

            if should_remove {
                self.sessions.remove(&session_id);

                // Find and fail all clients waiting on this session
                let failed_clients: Vec<ConnId> = self
                    .clients
                    .iter()
                    .filter(|(_, c)| c.session_id == session_id)
                    .map(|(id, _)| *id)
                    .collect();

                for id in failed_clients {
                    if let Some(client) = self.clients.remove(&id) {
                        let _ = client
                            .response_tx
                            .send(Response::err(ErrorCode::Internal, "session disconnected"));
                    }
                }
            }
        }

        // Client disconnect while deferred — drop the entry (response_tx dropped, client sees close)
        self.deferred.retain(|d| d.conn_id != conn_id);

        // Client disconnect - send cancel to session
        if let Some(client) = self.clients.remove(&conn_id) {
            debug!(
                conn_id,
                session_id = %client.session_id,
                request_id = client.request_id,
                "client disconnected, sending cancel to session"
            );

            if let Some(session) = self.sessions.get(&client.session_id) {
                let _ = session
                    .cmd_tx
                    .send(SessionCommand::CancelRequest(client.request_id))
                    .await;
            }
        }
    }

    fn handle_list(&self, selector: Option<&Selector>, all: bool) -> Response {
        let sessions: Vec<&SessionInfo> = self
            .sessions
            .values()
            .filter(|s| all || s.info.visible)
            .filter(|s| match selector {
                None => true,
                Some(Selector::Tag(tag)) => s.info.tag.as_deref() == Some(tag.as_str()),
                Some(Selector::Id(id)) => s.info.id.starts_with(id),
            })
            .map(|s| &s.info)
            .collect();

        Response::ok(sessions)
    }

    fn handle_tag_query(&self, selector: Option<&Selector>, client_pid: u32) -> Response {
        match self.resolve_selector(selector, client_pid) {
            Ok(session_id) => {
                if let Some(session) = self.sessions.get(&session_id) {
                    Response::ok(SessionStateResponse {
                        id: session.info.id.clone(),
                        tag: session.info.tag.clone(),
                        visible: None,
                    })
                } else {
                    Response::err(ErrorCode::SessionNotFound, "session not found")
                }
            }
            Err(code) => Response::err(code, code.to_string()),
        }
    }

    async fn shutdown(&mut self) {
        // Drop deferred clients — their response_tx is dropped, clients see connection close
        self.deferred.clear();
        self.settling_deadline = None;

        for (_, session) in self.sessions.drain() {
            let _ = session.cmd_tx.send(SessionCommand::Close).await;
        }
        self.conn_to_session.clear();
        self.clients.clear();
        let _ = std::fs::remove_file(ipc::socket_path());
    }
}

fn extract_selector(request: &Request) -> Option<&Selector> {
    match request {
        Request::Show { selector } => selector.as_ref(),
        Request::Hide { selector } => selector.as_ref(),
        Request::Tag { selector, .. } => selector.as_ref(),
        Request::Clear { selector } => selector.as_ref(),
        Request::Peek { selector } => selector.as_ref(),
        Request::History { selector, .. } => selector.as_ref(),
        Request::Exec { selector, .. } => selector.as_ref(),
        _ => None,
    }
}
