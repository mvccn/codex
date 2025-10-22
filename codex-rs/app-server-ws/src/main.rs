use axum::Router;
use axum::extract::State;
use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::get;
use clap::Parser;
use codex_app_server::public_api::AppServerEngine;
use codex_app_server_protocol::JSONRPCMessage;
use codex_common::ApprovalModeCliArg;
use codex_common::CliConfigOverrides;
use codex_common::SandboxModeCliArg;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_core::protocol::AskForApproval;
use codex_core::protocol_config_types::SandboxMode;
use futures_util::StreamExt;
use futures_util::sink::SinkExt;
use std::future::pending;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::debug;
use tracing::info;
use tracing::warn;

#[derive(Clone)]
struct AppState {
    auth_token: Option<String>,
    engine: AppServerEngine,
}

/// Codex App Server over WebSocket (in‑process) bridge.
#[derive(Debug, Parser)]
#[command(
    name = "codex-app-server-ws",
    about = "WebSocket bridge for the Codex App Server (JSON-RPC)"
)]
struct Args {
    /// Address to bind for the WebSocket server (host:port)
    #[arg(long = "bind", default_value = "127.0.0.1:9100")]
    bind: String,

    /// Optional bearer token required in the Authorization header.
    #[arg(long = "auth-token")]
    auth_token: Option<String>,

    /// Optional path to codex-linux-sandbox executable (Linux only).
    #[arg(long = "codex-linux-sandbox-exe", value_name = "PATH")]
    codex_linux_sandbox_exe: Option<PathBuf>,

    /// Config overrides: -c key=value (repeatable)
    #[clap(flatten)]
    config_overrides: CliConfigOverrides,

    /// Model to use for the engine (overrides config).
    #[arg(long, short = 'm')]
    model: Option<String>,

    /// Configuration profile from config.toml to specify default options.
    #[arg(long = "profile", short = 'p')]
    profile: Option<String>,

    /// Select the sandbox policy for executed commands.
    #[arg(long = "sandbox", short = 's')]
    sandbox_mode: Option<SandboxModeCliArg>,

    /// Configure when to ask for approval before executing commands.
    #[arg(long = "ask-for-approval", short = 'a')]
    approval_policy: Option<ApprovalModeCliArg>,

    /// Working directory for the engine session.
    #[arg(long = "cd", short = 'C')]
    cwd: Option<PathBuf>,

    /// Convenience alias: --sandbox workspace-write and -a on-failure.
    #[arg(long = "full-auto", default_value_t = false)]
    full_auto: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    // Load config via CLI overrides.
    let overrides = match args.config_overrides.parse_overrides() {
        Ok(v) => v,
        Err(e) => anyhow::bail!("error parsing -c overrides: {e}"),
    };
    let mut typed = ConfigOverrides::default();
    if let Some(m) = args.model.clone() {
        typed.model = Some(m);
    }
    if let Some(p) = args.profile.clone() {
        typed.config_profile = Some(p);
    }
    if let Some(s) = args.sandbox_mode {
        typed.sandbox_mode = Some(s.into());
    }
    if let Some(a) = args.approval_policy {
        typed.approval_policy = Some(a.into());
    }
    if let Some(dir) = args.cwd.clone() {
        typed.cwd = Some(dir);
    }
    if args.full_auto {
        if typed.sandbox_mode.is_none() {
            typed.sandbox_mode = Some(SandboxMode::WorkspaceWrite);
        }
        if typed.approval_policy.is_none() {
            typed.approval_policy = Some(AskForApproval::OnFailure);
        }
    }
    let config = Config::load_with_cli_overrides(overrides, typed).await?;
    let engine = AppServerEngine::new(Arc::new(config), args.codex_linux_sandbox_exe);

    let state = AppState {
        auth_token: args.auth_token,
        engine,
    };

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .with_state(state);

    let addr: SocketAddr = args
        .bind
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid bind address {}: {e}", args.bind))?;
    info!("codex-app-server-ws listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => info!(target: "codex-app-server-ws", "ctrl+c received; shutting down"),
        Err(err) => {
            warn!(
                target: "codex-app-server-ws",
                %err,
                "failed to install ctrl+c handler; continuing without graceful shutdown"
            );
            pending::<()>().await;
        }
    }
}

async fn ws_handler(
    headers: HeaderMap,
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    // If an auth token is configured, require an Authorization: Bearer header.
    if let Some(expected) = state.auth_token.clone() {
        let ok = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|h| h == format!("Bearer {expected}"));
        if !ok {
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        }
    }

    ws.on_upgrade(move |socket| handle_socket(state, socket))
}

async fn handle_socket(state: AppState, socket: WebSocket) {
    let (mut conn, mut rx_json) = state.engine.new_connection();

    let (mut ws_tx, mut ws_rx) = socket.split();
    // Forward server → client messages.
    let to_ws = tokio::spawn(async move {
        while let Some(value) = rx_json.recv().await {
            match serde_json::to_string(&value) {
                Ok(mut text) => {
                    debug!(target: "codex-app-server-ws", payload = %text, "→ ws");
                    text.push('\n');
                    if ws_tx.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Err(err) => {
                    warn!(target: "codex-app-server-ws", %err, "failed to serialize response");
                }
            }
        }
    });

    // Forward client → server messages.
    while let Some(msg) = ws_rx.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                debug!(target: "codex-app-server-ws", payload = %text, "← ws");
                match serde_json::from_str::<JSONRPCMessage>(&text) {
                    Ok(JSONRPCMessage::Request(req)) => conn.process_request(req).await,
                    Ok(JSONRPCMessage::Notification(n)) => conn.process_notification(n).await,
                    Ok(JSONRPCMessage::Response(resp)) => conn.process_response(resp).await,
                    Ok(JSONRPCMessage::Error(_)) => {}
                    Err(err) => {
                        warn!(target: "codex-app-server-ws", %err, "failed to parse incoming payload");
                    }
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(Message::Binary(_)) => {}
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
            Err(e) => {
                warn!("websocket error: {e}");
                break;
            }
        }
    }

    let _ = to_ws.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use serde_json::Value;
    use serde_json::json;
    use tokio::net::TcpListener;
    use tokio::net::TcpStream;
    use tokio_tungstenite::MaybeTlsStream;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as WsMsg;

    type TestWsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

    async fn spawn_server(state: AppState) -> SocketAddr {
        let app: Router = Router::new()
            .route("/ws", get(ws_handler))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    async fn wait_for_method(ws: &mut TestWsStream, method: &str, attempts: usize) -> bool {
        use tokio::time::Duration;
        use tokio::time::timeout;

        for _ in 0..attempts {
            match timeout(Duration::from_millis(200), ws.next()).await {
                Ok(Some(Ok(WsMsg::Text(txt)))) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt)
                        && v.get("method").and_then(|m| m.as_str()) == Some(method)
                    {
                        return true;
                    }
                }
                Ok(Some(Ok(WsMsg::Close(_)))) => return false,
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(_))) => return false,
                Ok(None) => return false,
                Err(_) => continue,
            }
        }
        false
    }

    #[tokio::test]
    async fn ws_flow_session_configured() {
        // Build minimal config and engine
        let overrides_cli = CliConfigOverrides {
            raw_overrides: vec![],
        };
        let cli_overrides = overrides_cli.parse_overrides().unwrap();
        let config = Config::load_with_cli_overrides(cli_overrides, ConfigOverrides::default())
            .await
            .expect("load config");
        let engine = AppServerEngine::new(Arc::new(config), None);
        let state = AppState {
            auth_token: None,
            engine,
        };
        let addr = spawn_server(state).await;

        // Connect WS
        let url = format!("ws://{addr}/ws");
        let (mut ws, _resp) = connect_async(url).await.unwrap();

        // Initialize
        let init = json!({
            "method": "initialize",
            "id": 1,
            "params": { "clientInfo": { "name": "tests", "version": "0.0.0" } }
        });
        ws.send(WsMsg::Text(init.to_string().into())).await.unwrap();

        // newConversation
        let tmp = tempfile::tempdir().unwrap();
        let new_conv = json!({
            "method": "newConversation",
            "id": 2,
            "params": { "cwd": tmp.path().to_string_lossy() }
        });
        ws.send(WsMsg::Text(new_conv.to_string().into()))
            .await
            .unwrap();

        // Expect a sessionConfigured server notification shortly after
        use tokio::time::Duration;
        use tokio::time::timeout;
        let mut saw_session_configured = false;
        for _ in 0..50 {
            match timeout(Duration::from_millis(200), ws.next()).await {
                Ok(Some(Ok(WsMsg::Text(txt)))) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt)
                        && v.get("method").and_then(|m| m.as_str()) == Some("sessionConfigured")
                    {
                        saw_session_configured = true;
                        break;
                    }
                }
                Ok(Some(_)) => continue,
                Ok(None) => continue,
                Err(_) => continue, // soft timeout; keep polling up to ~10s
            }
        }
        assert!(
            saw_session_configured,
            "expected sessionConfigured notification"
        );
    }

    #[tokio::test]
    async fn list_conversations_includes_cwd() {
        let overrides_cli = CliConfigOverrides {
            raw_overrides: vec![],
        };
        let cli_overrides = overrides_cli.parse_overrides().unwrap();
        let config = Config::load_with_cli_overrides(cli_overrides, ConfigOverrides::default())
            .await
            .expect("load config");
        let engine = AppServerEngine::new(Arc::new(config), None);
        let state = AppState {
            auth_token: None,
            engine,
        };
        let addr = spawn_server(state).await;

        let url = format!("ws://{addr}/ws");
        let (mut ws, _resp) = connect_async(url).await.unwrap();

        // Initialize connection
        let init = json!({
            "method": "initialize",
            "id": 1,
            "params": { "clientInfo": { "name": "tests", "version": "0.0.0" } }
        });
        ws.send(WsMsg::Text(init.to_string().into())).await.unwrap();

        // Create a new conversation with a specific cwd.
        let tmp = tempfile::tempdir().unwrap();
        let cwd_string = tmp.path().to_string_lossy().to_string();
        let new_conv = json!({
            "method": "newConversation",
            "id": 2,
            "params": { "cwd": cwd_string }
        });
        ws.send(WsMsg::Text(new_conv.to_string().into()))
            .await
            .unwrap();

        use tokio::time::Duration;
        use tokio::time::timeout;

        let mut conversation_id: Option<String> = None;
        let mut saw_session_configured = false;
        for _ in 0..100 {
            match timeout(Duration::from_millis(200), ws.next()).await {
                Ok(Some(Ok(WsMsg::Text(txt)))) => {
                    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
                        continue;
                    };
                    if v.get("id").and_then(Value::as_i64) == Some(2)
                        && let Some(result) = v.get("result")
                        && let Some(cid) = result.get("conversationId").and_then(Value::as_str)
                    {
                        conversation_id = Some(cid.to_string());
                    }
                    if v.get("method")
                        .and_then(Value::as_str)
                        .is_some_and(|m| m == "sessionConfigured")
                    {
                        saw_session_configured = true;
                    }
                    if conversation_id.is_some() && saw_session_configured {
                        break;
                    }
                }
                Ok(Some(Ok(WsMsg::Close(_)))) => break,
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => continue,
            }
        }

        let conversation_id =
            conversation_id.expect("newConversation response should include conversationId");

        // Wait for sessionConfigured before listing
        if !saw_session_configured {
            assert!(
                wait_for_method(&mut ws, "sessionConfigured", 100).await,
                "expected sessionConfigured notification"
            );
        }

        // Send a simple user message so the rollout has a preview entry.
        use codex_app_server_protocol::InputItem as RpcInputItem;
        use codex_app_server_protocol::JSONRPCMessage as RpcMessage;
        use codex_app_server_protocol::JSONRPCRequest as RpcRequest;
        use codex_app_server_protocol::RequestId as RpcRequestId;
        use codex_app_server_protocol::SendUserMessageParams as RpcSendUserMessageParams;
        use codex_protocol::ConversationId as ConvId;

        let cid = ConvId::from_string(&conversation_id).expect("parse conversationId");
        let params = RpcSendUserMessageParams {
            conversation_id: cid,
            items: vec![RpcInputItem::Text {
                text: "list cwd probe".to_string(),
            }],
        };
        let send_request = RpcMessage::Request(RpcRequest {
            id: RpcRequestId::Integer(4),
            method: "sendUserMessage".to_string(),
            params: Some(serde_json::to_value(&params).expect("serialize params")),
        });
        ws.send(WsMsg::Text(
            serde_json::to_string(&send_request).unwrap().into(),
        ))
        .await
        .unwrap();

        // Wait for sendUserMessage response (id 4) to ensure the message was processed.
        for _ in 0..100 {
            match timeout(Duration::from_millis(200), ws.next()).await {
                Ok(Some(Ok(WsMsg::Text(txt)))) => {
                    if serde_json::from_str::<Value>(&txt)
                        .ok()
                        .and_then(|v| v.get("id").and_then(Value::as_i64))
                        == Some(4)
                    {
                        break;
                    }
                }
                Ok(Some(Ok(WsMsg::Close(_)))) => break,
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => continue,
            }
        }

        // Request listConversations
        let list_request = json!({
            "method": "listConversations",
            "id": 3,
            "params": { "pageSize": 20 }
        });
        ws.send(WsMsg::Text(list_request.to_string().into()))
            .await
            .unwrap();

        let mut saw_list_response = false;
        for _ in 0..200 {
            match timeout(Duration::from_millis(200), ws.next()).await {
                Ok(Some(Ok(WsMsg::Text(txt)))) => {
                    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
                        continue;
                    };
                    if v.get("id").and_then(Value::as_i64) != Some(3) {
                        continue;
                    }
                    let Some(items) = v
                        .get("result")
                        .and_then(|r| r.get("items"))
                        .and_then(Value::as_array)
                    else {
                        continue;
                    };

                    let entry = items.iter().find(|item| {
                        item.get("conversationId")
                            .and_then(Value::as_str)
                            .map(|cid| cid == conversation_id)
                            .unwrap_or(false)
                    });

                    let Some(entry) = entry else {
                        panic!(
                            "expected listConversations response to include newly created conversation"
                        );
                    };

                    let cwd = entry.get("cwd").and_then(Value::as_str).unwrap_or_default();
                    assert_eq!(
                        cwd, cwd_string,
                        "listConversations entry should include cwd"
                    );
                    saw_list_response = true;
                    break;
                }
                Ok(Some(Ok(WsMsg::Close(_)))) => break,
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => continue,
            }
        }

        assert!(
            saw_list_response,
            "expected listConversations response with id=3"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ws_lists_existing_sessions_on_disk() {
        use std::fs;
        use uuid::Uuid;

        // Create a temporary CODEX_HOME with a single rollout that looks like a TUI/CLI session.
        let codex_home = tempfile::tempdir().expect("tmp codex_home");
        // Setting env vars mutates global state and is unsafe in Rust 2024.
        unsafe {
            std::env::set_var("CODEX_HOME", codex_home.path());
        }

        let ts = "2025-01-02T12-00-00";
        let uuid = Uuid::new_v4();
        let year = &ts[0..4];
        let month = &ts[5..7];
        let day = &ts[8..10];
        let dir = codex_home
            .path()
            .join("sessions")
            .join(year)
            .join(month)
            .join(day);
        fs::create_dir_all(&dir).expect("create sessions dir");

        let file_path = dir.join(format!("rollout-{ts}-{uuid}.jsonl"));

        // Write meta + a plain user message ResponseItem so the scanner picks it up.
        let meta_line = serde_json::json!({
            "timestamp": "2025-01-02T12:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": uuid,
                "timestamp": "2025-01-02T12:00:00Z",
                "cwd": "/",
                "originator": "codex_tui_rs",
                "cli_version": "0.0.0",
                "instructions": null,
                "source": "cli"
            }
        });
        let user_line = serde_json::json!({
            "timestamp": "2025-01-02T12:00:00Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello from tui"}]
            }
        });
        fs::write(&file_path, format!("{meta_line}\n{user_line}\n")).expect("write rollout file");

        // Start WS server with config derived from CODEX_HOME
        let overrides_cli = CliConfigOverrides {
            raw_overrides: vec![],
        };
        let cli_overrides = overrides_cli.parse_overrides().unwrap();
        let config = Config::load_with_cli_overrides(cli_overrides, ConfigOverrides::default())
            .await
            .expect("load config");
        let engine = AppServerEngine::new(Arc::new(config), None);
        let state = AppState {
            auth_token: None,
            engine,
        };
        let addr = spawn_server(state).await;

        // Connect WS and initialize
        let url = format!("ws://{addr}/ws");
        let (mut ws, _resp) = connect_async(url).await.unwrap();
        let init = json!({
            "method": "initialize",
            "id": 1,
            "params": { "clientInfo": { "name": "tests", "version": "0.0.0" } }
        });
        ws.send(WsMsg::Text(init.to_string().into())).await.unwrap();

        // Request listConversations and expect our file to be present.
        let list_request = json!({
            "method": "listConversations",
            "id": 2,
            "params": { "pageSize": 50 }
        });
        ws.send(WsMsg::Text(list_request.to_string().into()))
            .await
            .unwrap();

        use tokio::time::Duration;
        use tokio::time::timeout;
        let mut saw = false;
        for _ in 0..50 {
            match timeout(Duration::from_millis(200), ws.next()).await {
                Ok(Some(Ok(WsMsg::Text(txt)))) => {
                    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
                        continue;
                    };
                    if v.get("id").and_then(Value::as_i64) != Some(2) {
                        continue;
                    }
                    let Some(items) = v
                        .get("result")
                        .and_then(|r| r.get("items"))
                        .and_then(Value::as_array)
                    else {
                        continue;
                    };

                    let expected_path = std::fs::canonicalize(&file_path).unwrap();
                    saw = items.iter().any(|it| {
                        it.get("path")
                            .and_then(Value::as_str)
                            .map(|p| p == expected_path.to_string_lossy())
                            .unwrap_or(false)
                    });
                    if saw {
                        break;
                    }
                }
                _ => continue,
            }
        }
        assert!(
            saw,
            "expected listConversations to include preexisting TUI session"
        );
    }

    #[tokio::test]
    async fn auth_rejects_without_header() {
        let overrides_cli = CliConfigOverrides {
            raw_overrides: vec![],
        };
        let cli_overrides = overrides_cli.parse_overrides().unwrap();
        let config = Config::load_with_cli_overrides(cli_overrides, ConfigOverrides::default())
            .await
            .expect("load config");
        let engine = AppServerEngine::new(Arc::new(config), None);
        let state = AppState {
            auth_token: Some("secret".to_string()),
            engine,
        };
        let addr = spawn_server(state).await;

        // Without Authorization header, WS handshake should fail.
        let url = format!("ws://{addr}/ws");
        let req = http::Request::builder().uri(url).body(()).unwrap();
        let res = connect_async(req).await;
        assert!(
            res.is_err(),
            "expected WS handshake to be rejected without Authorization header"
        );
    }

    #[tokio::test]
    async fn ws_send_user_turn_emits_task_complete() {
        // Minimal config from defaults; only assert engine activity (no network required).
        let tmp = tempfile::tempdir().expect("tmp dir");
        let overrides_cli = CliConfigOverrides {
            raw_overrides: vec![],
        };
        let cli_overrides = overrides_cli.parse_overrides().unwrap();
        let config = Config::load_with_cli_overrides(cli_overrides, ConfigOverrides::default())
            .await
            .expect("load config");
        let engine = AppServerEngine::new(Arc::new(config), None);
        let state = AppState {
            auth_token: None,
            engine,
        };
        let addr = spawn_server(state).await;

        // Connect WS and initialize
        let url = format!("ws://{addr}/ws");
        let (mut ws, _resp) = connect_async(url).await.unwrap();
        let init = json!({
            "method": "initialize",
            "id": 1,
            "params": { "clientInfo": { "name": "tests", "version": "0.0.0" } }
        });
        ws.send(WsMsg::Text(init.to_string().into())).await.unwrap();

        // Create conversation
        let new_conv = json!({
            "method": "newConversation",
            "id": 2,
            "params": { "cwd": tmp.path().to_string_lossy() }
        });
        ws.send(WsMsg::Text(new_conv.to_string().into()))
            .await
            .unwrap();

        // Await newConversation response for conversationId
        use tokio::time::Duration;
        use tokio::time::timeout;
        let mut conversation_id: Option<String> = None;
        for _ in 0..50 {
            if let Ok(Some(Ok(WsMsg::Text(txt)))) =
                timeout(Duration::from_millis(200), ws.next()).await
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt)
                && v.get("id").and_then(serde_json::Value::as_i64) == Some(2)
            {
                conversation_id = v
                    .get("result")
                    .and_then(|r| r.get("conversationId"))
                    .and_then(|s| s.as_str())
                    .map(str::to_string);
                break;
            }
        }
        let conversation_id = conversation_id.expect("conversationId");

        // Subscribe to events
        let subscribe = json!({
            "method": "addConversationListener",
            "id": 3,
            "params": { "conversationId": conversation_id }
        });
        ws.send(WsMsg::Text(subscribe.to_string().into()))
            .await
            .unwrap();
        // Wait for addConversationListener response
        for _ in 0..50 {
            if let Ok(Some(Ok(WsMsg::Text(txt)))) =
                timeout(Duration::from_millis(200), ws.next()).await
                && serde_json::from_str::<serde_json::Value>(&txt)
                    .ok()
                    .and_then(|v| v.get("id").and_then(serde_json::Value::as_i64))
                    == Some(3)
            {
                break;
            }
        }

        // Send a user turn (build via typed protocol to ensure correct shape)
        use codex_app_server_protocol::InputItem as RpcInputItem;
        use codex_app_server_protocol::JSONRPCMessage as RpcMessage;
        use codex_app_server_protocol::JSONRPCRequest as RpcRequest;
        use codex_app_server_protocol::RequestId as RpcRequestId;
        use codex_app_server_protocol::SendUserTurnParams as RpcSendUserTurnParams;
        use codex_protocol::ConversationId as ConvId;
        use codex_protocol::config_types::ReasoningEffort;
        use codex_protocol::config_types::ReasoningSummary;
        use codex_protocol::protocol::AskForApproval;
        use codex_protocol::protocol::SandboxPolicy;

        let cid = ConvId::from_string(&conversation_id).expect("parse conversationId");
        let params = RpcSendUserTurnParams {
            conversation_id: cid,
            items: vec![RpcInputItem::Text {
                text: "Hello".to_string(),
            }],
            cwd: tmp.path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            model: "mock-model".to_string(),
            effort: Some(ReasoningEffort::Medium),
            summary: ReasoningSummary::Auto,
        };
        let req = RpcRequest {
            id: RpcRequestId::Integer(4),
            method: "sendUserTurn".to_string(),
            params: Some(serde_json::to_value(&params).unwrap()),
        };
        let wire = serde_json::to_string(&RpcMessage::Request(req)).unwrap();
        ws.send(WsMsg::Text(wire.into())).await.unwrap();

        // Ack for sendUserTurn (id=4)
        for _ in 0..50 {
            if let Ok(Some(Ok(WsMsg::Text(txt)))) =
                timeout(Duration::from_millis(200), ws.next()).await
                && serde_json::from_str::<serde_json::Value>(&txt)
                    .ok()
                    .and_then(|v| v.get("id").and_then(serde_json::Value::as_i64))
                    == Some(4)
            {
                eprintln!("ACK <- {txt}");
                break;
            }
        }

        // Expect some activity from the stream
        let mut saw_activity = false;
        for _ in 0..100 {
            if let Ok(Some(Ok(WsMsg::Text(txt)))) =
                timeout(Duration::from_millis(200), ws.next()).await
            {
                eprintln!("WS <- {txt}");
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt)
                    && let Some(method) = v.get("method").and_then(|m| m.as_str())
                    && matches!(
                        method,
                        "codex/event/task_started"
                            | "codex/event/agent_message"
                            | "codex/event/task_complete"
                    )
                {
                    saw_activity = true;
                    break;
                }
            }
        }
        assert!(
            saw_activity,
            "expected activity (task_started/agent_message/task_complete)"
        );
    }

    #[tokio::test]
    async fn ws_multiple_listeners_receive_task_complete() {
        let tmp = tempfile::tempdir().expect("tmp dir");
        let overrides_cli = CliConfigOverrides {
            raw_overrides: vec![],
        };
        let cli_overrides = overrides_cli.parse_overrides().unwrap();
        let config = Config::load_with_cli_overrides(cli_overrides, ConfigOverrides::default())
            .await
            .expect("load config");
        let engine = AppServerEngine::new(Arc::new(config), None);
        let state = AppState {
            auth_token: None,
            engine,
        };
        let addr = spawn_server(state).await;
        let url = format!("ws://{addr}/ws");

        let (mut ws1, _resp1) = connect_async(&url).await.unwrap();
        let init1 = json!({
            "method": "initialize",
            "id": 1,
            "params": { "clientInfo": { "name": "tests", "version": "0.0.0" } }
        });
        ws1.send(WsMsg::Text(init1.to_string().into()))
            .await
            .unwrap();

        let new_conv = json!({
            "method": "newConversation",
            "id": 2,
            "params": { "cwd": tmp.path().to_string_lossy() }
        });
        ws1.send(WsMsg::Text(new_conv.to_string().into()))
            .await
            .unwrap();

        use tokio::time::Duration;
        use tokio::time::timeout;
        let mut conversation_id: Option<String> = None;
        let mut ws1_task_complete_early = false;
        for _ in 0..50 {
            if let Ok(Some(Ok(WsMsg::Text(txt)))) =
                timeout(Duration::from_millis(200), ws1.next()).await
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt)
            {
                if v.get("id").and_then(serde_json::Value::as_i64) == Some(2) {
                    conversation_id = v
                        .get("result")
                        .and_then(|r| r.get("conversationId"))
                        .and_then(|s| s.as_str())
                        .map(str::to_string);
                    break;
                }
                if v.get("method").and_then(|m| m.as_str()) == Some("codex/event/task_complete") {
                    ws1_task_complete_early = true;
                }
            }
        }
        let conversation_id = conversation_id.expect("conversationId");

        let subscribe1 = json!({
            "method": "addConversationListener",
            "id": 3,
            "params": { "conversationId": conversation_id }
        });
        ws1.send(WsMsg::Text(subscribe1.to_string().into()))
            .await
            .unwrap();
        for _ in 0..50 {
            if let Ok(Some(Ok(WsMsg::Text(txt)))) =
                timeout(Duration::from_millis(200), ws1.next()).await
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt)
            {
                if v.get("id").and_then(serde_json::Value::as_i64) == Some(3) {
                    break;
                }
                if v.get("method").and_then(|m| m.as_str()) == Some("codex/event/task_complete") {
                    ws1_task_complete_early = true;
                }
            }
        }

        let (mut ws2, _resp2) = connect_async(&url).await.unwrap();
        let init2 = json!({
            "method": "initialize",
            "id": 1,
            "params": { "clientInfo": { "name": "tests", "version": "0.0.0" } }
        });
        ws2.send(WsMsg::Text(init2.to_string().into()))
            .await
            .unwrap();

        let subscribe2 = json!({
            "method": "addConversationListener",
            "id": 2,
            "params": { "conversationId": conversation_id }
        });
        ws2.send(WsMsg::Text(subscribe2.to_string().into()))
            .await
            .unwrap();
        let mut ws2_task_complete_early = false;
        for _ in 0..50 {
            if let Ok(Some(Ok(WsMsg::Text(txt)))) =
                timeout(Duration::from_millis(200), ws2.next()).await
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt)
            {
                if v.get("id").and_then(serde_json::Value::as_i64) == Some(2) {
                    break;
                }
                if v.get("method").and_then(|m| m.as_str()) == Some("codex/event/task_complete") {
                    ws2_task_complete_early = true;
                }
            }
        }

        use codex_app_server_protocol::InputItem as RpcInputItem;
        use codex_app_server_protocol::JSONRPCMessage as RpcMessage;
        use codex_app_server_protocol::JSONRPCRequest as RpcRequest;
        use codex_app_server_protocol::RequestId as RpcRequestId;
        use codex_app_server_protocol::SendUserTurnParams as RpcSendUserTurnParams;
        use codex_protocol::ConversationId as ConvId;
        use codex_protocol::config_types::ReasoningEffort;
        use codex_protocol::config_types::ReasoningSummary;
        use codex_protocol::protocol::AskForApproval;
        use codex_protocol::protocol::SandboxPolicy;

        let cid = ConvId::from_string(&conversation_id).expect("parse conversationId");
        let params = RpcSendUserTurnParams {
            conversation_id: cid,
            items: vec![RpcInputItem::Text {
                text: "Hello".to_string(),
            }],
            cwd: tmp.path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            model: "mock-model".to_string(),
            effort: Some(ReasoningEffort::Medium),
            summary: ReasoningSummary::Auto,
        };
        let req = RpcRequest {
            id: RpcRequestId::Integer(4),
            method: "sendUserTurn".to_string(),
            params: Some(serde_json::to_value(&params).unwrap()),
        };
        let wire = serde_json::to_string(&RpcMessage::Request(req)).unwrap();
        ws1.send(WsMsg::Text(wire.into())).await.unwrap();

        for _ in 0..50 {
            if let Ok(Some(Ok(WsMsg::Text(txt)))) =
                timeout(Duration::from_millis(200), ws1.next()).await
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt)
            {
                if v.get("id").and_then(serde_json::Value::as_i64) == Some(4) {
                    break;
                }
                if v.get("method").and_then(|m| m.as_str()) == Some("codex/event/task_complete") {
                    ws1_task_complete_early = true;
                }
            }
        }

        let ws1_task = if ws1_task_complete_early {
            true
        } else {
            wait_for_method(&mut ws1, "codex/event/task_complete", 100).await
        };
        let ws2_task = if ws2_task_complete_early {
            true
        } else {
            wait_for_method(&mut ws2, "codex/event/task_complete", 100).await
        };

        assert!(ws1_task, "primary listener should receive task_complete");
        assert!(ws2_task, "secondary listener should receive task_complete");
    }
}
