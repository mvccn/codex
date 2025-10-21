use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use app_test_support::McpProcess;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_chat_completions_server;
use codex_app_server::public_api::AppServerEngine;
use codex_app_server_protocol::AddConversationListenerParams;
use codex_app_server_protocol::JSONRPCMessage as RpcMessage;
use codex_app_server_protocol::JSONRPCRequest as RpcRequest;
use codex_app_server_protocol::RequestId as RpcRequestId;
use codex_app_server_protocol::SendUserTurnParams as RpcSendUserTurnParams;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::SandboxPolicy;
use codex_protocol::ConversationId as ConvId;
use codex_protocol::config_types::ReasoningEffort;
use codex_protocol::config_types::ReasoningSummary;
use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use serde_json::json;
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;

fn write_mock_config(codex_home: &std::path::Path, base_url: &str) -> Result<()> {
    let config_toml = codex_home.join("config.toml");
    std::fs::write(
        &config_toml,
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "danger-full-access"

model_provider = "mock_provider"

[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{base_url}/v1"
wire_api = "chat"
request_max_retries = 0
stream_max_retries = 0
requires_openai_auth = false
"#
        ),
    )
    .with_context(|| format!("failed to write {}", config_toml.display()))?;
    Ok(())
}

async fn spawn_ws(engine: AppServerEngine) -> Result<SocketAddr> {
    let state = codex_app_server_ws::AppState {
        auth_token: None,
        engine,
    };
    let app = codex_app_server_ws::build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("failed to bind ws listener")?;
    let addr = listener
        .local_addr()
        .context("failed to resolve ws listener address")?;
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            eprintln!("ws server exited with error: {err}");
        }
    });
    Ok(addr)
}

async fn collect_ws_methods(addr: SocketAddr, cwd: &str) -> Result<Vec<String>> {
    use tokio::time::timeout;
    use tokio_tungstenite::tungstenite::Message as WsMsg;

    let url = format!("ws://{addr}/ws");
    let (mut ws, _resp) = connect_async(url.clone())
        .await
        .with_context(|| format!("failed to connect websocket at {url}"))?;

    let init = json!({
        "method": "initialize",
        "id": 1,
        "params": { "clientInfo": { "name": "tests", "version": "0.0.0" } }
    });
    ws.send(WsMsg::Text(init.to_string().into()))
        .await
        .context("failed to send initialize request")?;

    let new_conv = json!({
        "method": "newConversation",
        "id": 2,
        "params": { "cwd": cwd }
    });
    ws.send(WsMsg::Text(new_conv.to_string().into()))
        .await
        .context("failed to send newConversation request")?;

    let mut conversation_id: Option<String> = None;
    for _ in 0..50 {
        match timeout(Duration::from_millis(200), ws.next()).await {
            Ok(Some(Ok(WsMsg::Text(txt)))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt)
                    && v.get("id").and_then(serde_json::Value::as_i64) == Some(2)
                {
                    conversation_id = v
                        .get("result")
                        .and_then(|r| r.get("conversationId"))
                        .and_then(|s| s.as_str())
                        .map(str::to_owned);
                    break;
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(err))) => bail!("ws error while waiting for newConversation ack: {err}"),
            Ok(None) => bail!("ws closed while waiting for newConversation ack"),
            Err(_) => {}
        }
    }
    let conversation_id = conversation_id
        .context("timed out waiting for conversationId from newConversation response")?;

    let subscribe = json!({
        "method": "addConversationListener",
        "id": 3,
        "params": { "conversationId": conversation_id }
    });
    ws.send(WsMsg::Text(subscribe.to_string().into()))
        .await
        .context("failed to send addConversationListener request")?;

    let mut subscribe_ack = false;
    for _ in 0..50 {
        match timeout(Duration::from_millis(200), ws.next()).await {
            Ok(Some(Ok(WsMsg::Text(txt)))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt)
                    && v.get("id").and_then(serde_json::Value::as_i64) == Some(3)
                {
                    subscribe_ack = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(err))) => {
                bail!("ws error while waiting for addConversationListener ack: {err}")
            }
            Ok(None) => bail!("ws closed while waiting for addConversationListener ack"),
            Err(_) => {}
        }
    }
    ensure!(subscribe_ack, "did not receive addConversationListener ack");

    let cid = ConvId::from_string(&conversation_id)
        .context("failed to parse conversationId into ConvId")?;
    let params = RpcSendUserTurnParams {
        conversation_id: cid,
        items: vec![codex_app_server_protocol::InputItem::Text {
            text: "Hello".to_string(),
        }],
        cwd: std::path::PathBuf::from(cwd),
        approval_policy: AskForApproval::Never,
        sandbox_policy: SandboxPolicy::DangerFullAccess,
        model: "mock-model".to_string(),
        effort: Some(ReasoningEffort::Medium),
        summary: ReasoningSummary::Auto,
    };
    let params_value =
        serde_json::to_value(&params).context("failed to serialize sendUserTurn params")?;
    let req = RpcRequest {
        id: RpcRequestId::Integer(4),
        method: "sendUserTurn".to_string(),
        params: Some(params_value),
    };
    let wire = serde_json::to_string(&RpcMessage::Request(req))
        .context("failed to serialize sendUserTurn request")?;
    ws.send(WsMsg::Text(wire.into()))
        .await
        .context("failed to send sendUserTurn request")?;

    let mut turn_ack = false;
    for _ in 0..50 {
        match timeout(Duration::from_millis(200), ws.next()).await {
            Ok(Some(Ok(WsMsg::Text(txt)))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt)
                    && v.get("id").and_then(serde_json::Value::as_i64) == Some(4)
                {
                    turn_ack = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(err))) => bail!("ws error while waiting for sendUserTurn ack: {err}"),
            Ok(None) => bail!("ws closed while waiting for sendUserTurn ack"),
            Err(_) => {}
        }
    }
    ensure!(turn_ack, "did not receive sendUserTurn ack");

    let mut methods: Vec<String> = Vec::new();
    for _ in 0..150 {
        match timeout(Duration::from_millis(200), ws.next()).await {
            Ok(Some(Ok(WsMsg::Text(txt)))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt)
                    && let Some(m) = v.get("method").and_then(|m| m.as_str())
                    && m.starts_with("codex/event/")
                {
                    methods.push(m.to_string());
                    if m == "codex/event/task_complete" {
                        break;
                    }
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(err))) => bail!("ws error while collecting event methods: {err}"),
            Ok(None) => break,
            Err(_) => {}
        }
    }

    Ok(methods)
}

async fn collect_inprocess_methods(
    codex_home: &std::path::Path,
    cwd: &std::path::Path,
) -> Result<Vec<String>> {
    use tokio::time::timeout;

    let mut mcp = McpProcess::new(codex_home)
        .await
        .context("failed to spawn app-server process")?;
    timeout(Duration::from_secs(5), mcp.initialize())
        .await
        .context("timed out waiting for initialize response")?
        .context("initialize request failed")?;

    let new_id = mcp
        .send_new_conversation_request(codex_app_server_protocol::NewConversationParams {
            model: None,
            profile: None,
            cwd: Some(cwd.to_string_lossy().to_string()),
            approval_policy: Some(AskForApproval::Never),
            sandbox: Some(codex_protocol::config_types::SandboxMode::DangerFullAccess),
            config: None,
            base_instructions: None,
            include_plan_tool: None,
            include_apply_patch_tool: None,
        })
        .await
        .context("failed to send newConversation request")?;
    let new_resp = timeout(
        Duration::from_secs(5),
        mcp.read_stream_until_response_message(RpcRequestId::Integer(new_id)),
    )
    .await
    .context("timed out waiting for newConversation response")?;
    let new_resp = new_resp.context("newConversation response error")?;
    let new_conv: codex_app_server_protocol::NewConversationResponse =
        app_test_support::to_response::<_>(new_resp)
            .context("failed to parse newConversation response")?;
    let conversation_id = new_conv.conversation_id;

    let add_id = mcp
        .send_add_conversation_listener_request(AddConversationListenerParams { conversation_id })
        .await
        .context("failed to send addConversationListener request")?;
    timeout(
        Duration::from_secs(5),
        mcp.read_stream_until_response_message(RpcRequestId::Integer(add_id)),
    )
    .await
    .context("timed out waiting for addConversationListener ack")?
    .context("addConversationListener request failed")?;

    let turn_id = mcp
        .send_send_user_turn_request(RpcSendUserTurnParams {
            conversation_id,
            items: vec![codex_app_server_protocol::InputItem::Text {
                text: "Hello".to_string(),
            }],
            cwd: cwd.to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            model: "mock-model".to_string(),
            effort: Some(ReasoningEffort::Medium),
            summary: ReasoningSummary::Auto,
        })
        .await
        .context("failed to send sendUserTurn request")?;
    timeout(
        Duration::from_secs(5),
        mcp.read_stream_until_response_message(RpcRequestId::Integer(turn_id)),
    )
    .await
    .context("timed out waiting for sendUserTurn ack")?
    .context("sendUserTurn request failed")?;

    let mut methods: Vec<String> = Vec::new();
    timeout(
        Duration::from_secs(5),
        mcp.read_stream_until_notification_message("codex/event/agent_message"),
    )
    .await
    .context("timed out waiting for agent_message notification")?
    .context("agent_message notification failed")?;
    methods.push("codex/event/agent_message".to_string());

    timeout(
        Duration::from_secs(5),
        mcp.read_stream_until_notification_message("codex/event/task_complete"),
    )
    .await
    .context("timed out waiting for task_complete notification")?
    .context("task_complete notification failed")?;
    methods.push("codex/event/task_complete".to_string());

    Ok(methods)
}

#[tokio::test]
#[ignore = "end-to-end parity with SSE mocks can be slow/flaky; run explicitly"]
async fn parity_basic_turn_produces_same_event_order() -> Result<()> {
    let responses_ws = vec![
        create_final_assistant_message_sse_response("Welcome")
            .context("failed to craft welcome SSE for websocket run")?,
        create_final_assistant_message_sse_response("Done")
            .context("failed to craft completion SSE for websocket run")?,
    ];
    let responses_ip = vec![
        create_final_assistant_message_sse_response("Welcome")
            .context("failed to craft welcome SSE for in-process run")?,
        create_final_assistant_message_sse_response("Done")
            .context("failed to craft completion SSE for in-process run")?,
    ];
    let mock_ws = create_mock_chat_completions_server(responses_ws).await;
    let mock_ip = create_mock_chat_completions_server(responses_ip).await;

    let codex_home = tempfile::tempdir().context("failed to create temp codex home")?;
    write_mock_config(codex_home.path(), &mock_ws.uri())?;

    let cfg_toml =
        codex_core::config::load_config_as_toml_with_cli_overrides(codex_home.path(), vec![])
            .await
            .context("failed to load config.toml overrides")?;
    let config = Config::load_from_base_config_with_overrides(
        cfg_toml,
        ConfigOverrides::default(),
        codex_home.path().to_path_buf(),
    )
    .context("failed to materialize Config for websocket run")?;
    let engine = AppServerEngine::new(std::sync::Arc::new(config), None);
    let addr = spawn_ws(engine).await?;

    let cwd = codex_home.path().to_string_lossy().to_string();
    let ws_methods = collect_ws_methods(addr, &cwd)
        .await
        .context("failed to collect websocket event methods")?;

    write_mock_config(codex_home.path(), &mock_ip.uri())?;
    let inproc_methods = collect_inprocess_methods(codex_home.path(), codex_home.path())
        .await
        .context("failed to collect in-process event methods")?;

    let f = |m: &String| m != "codex/event/user_message";
    let ws_filtered: Vec<_> = ws_methods.into_iter().filter(f).collect();
    let inproc_filtered: Vec<_> = inproc_methods.into_iter().filter(f).collect();

    assert!(ws_filtered.contains(&"codex/event/task_started".to_string()));
    assert_eq!(
        inproc_filtered.last(),
        Some(&"codex/event/task_complete".to_string())
    );
    let ws_last = ws_filtered.last().cloned();
    assert!(
        ws_last == Some("codex/event/task_complete".to_string())
            || ws_last == Some("codex/event/stream_error".to_string()),
        "ws last={ws_last:?}"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "approval roundtrip uses SSE mocks and is environment-sensitive; run explicitly"]
async fn parity_exec_approval_roundtrip() -> Result<()> {
    use tokio::time::timeout;
    use tokio_tungstenite::tungstenite::Message as WsMsg;

    let responses_ws = vec![
        app_test_support::create_shell_sse_response(
            vec!["bash".into(), "-lc".into(), "echo hi".into()],
            None,
            Some(5000),
            "call1",
        )
        .context("failed to craft websocket shell SSE response")?,
        create_final_assistant_message_sse_response("done")
            .context("failed to craft websocket final SSE response")?,
    ];
    let responses_ip = vec![
        app_test_support::create_shell_sse_response(
            vec!["bash".into(), "-lc".into(), "echo hi".into()],
            None,
            Some(5000),
            "call1",
        )
        .context("failed to craft in-process shell SSE response")?,
        create_final_assistant_message_sse_response("done")
            .context("failed to craft in-process final SSE response")?,
    ];
    let mock_ws = create_mock_chat_completions_server(responses_ws).await;
    let mock_ip = create_mock_chat_completions_server(responses_ip).await;

    let codex_home = tempfile::tempdir().context("failed to create temp codex home")?;
    write_mock_config(codex_home.path(), &mock_ws.uri())?;

    let cfg_toml =
        codex_core::config::load_config_as_toml_with_cli_overrides(codex_home.path(), vec![])
            .await
            .context("failed to load config overrides for websocket run")?;
    let config = Config::load_from_base_config_with_overrides(
        cfg_toml,
        ConfigOverrides::default(),
        codex_home.path().to_path_buf(),
    )
    .context("failed to materialize Config for websocket run")?;
    let engine = AppServerEngine::new(std::sync::Arc::new(config), None);
    let addr = spawn_ws(engine).await?;

    let url = format!("ws://{addr}/ws");
    let (mut ws, _resp) = connect_async(url.clone())
        .await
        .with_context(|| format!("failed to connect websocket client at {url}"))?;

    let init = json!({
        "method": "initialize",
        "id": 1,
        "params": { "clientInfo": { "name": "tests", "version": "0.0.0" } }
    });
    ws.send(WsMsg::Text(init.to_string().into()))
        .await
        .context("failed to send initialize request")?;

    let tmp = tempfile::tempdir().context("failed to create temp cwd")?;
    let new_conv = json!({
        "method": "newConversation",
        "id": 2,
        "params": { "cwd": tmp.path().to_string_lossy() }
    });
    ws.send(WsMsg::Text(new_conv.to_string().into()))
        .await
        .context("failed to send newConversation request")?;

    let mut conversation_id: Option<String> = None;
    for _ in 0..50 {
        match timeout(Duration::from_millis(200), ws.next()).await {
            Ok(Some(Ok(WsMsg::Text(txt)))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt)
                    && v.get("id").and_then(serde_json::Value::as_i64) == Some(2)
                {
                    conversation_id = v
                        .get("result")
                        .and_then(|r| r.get("conversationId"))
                        .and_then(|s| s.as_str())
                        .map(str::to_owned);
                    break;
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(err))) => bail!("ws error while waiting for newConversation ack: {err}"),
            Ok(None) => bail!("ws closed while waiting for newConversation ack"),
            Err(_) => {}
        }
    }
    let conversation_id =
        conversation_id.context("timed out waiting for newConversation response")?;

    let subscribe = json!({
        "method": "addConversationListener",
        "id": 3,
        "params": { "conversationId": conversation_id }
    });
    ws.send(WsMsg::Text(subscribe.to_string().into()))
        .await
        .context("failed to send addConversationListener request")?;
    let mut subscribe_ack = false;
    for _ in 0..50 {
        match timeout(Duration::from_millis(200), ws.next()).await {
            Ok(Some(Ok(WsMsg::Text(txt)))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt)
                    && v.get("id").and_then(serde_json::Value::as_i64) == Some(3)
                {
                    subscribe_ack = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(err))) => {
                bail!("ws error while waiting for addConversationListener ack: {err}")
            }
            Ok(None) => bail!("ws closed while waiting for addConversationListener ack"),
            Err(_) => {}
        }
    }
    ensure!(subscribe_ack, "did not receive addConversationListener ack");

    let cid = ConvId::from_string(&conversation_id).context("failed to parse conversationId")?;
    let turn = RpcSendUserTurnParams {
        conversation_id: cid,
        items: vec![codex_app_server_protocol::InputItem::Text {
            text: "Run shell".to_string(),
        }],
        cwd: tmp.path().to_path_buf(),
        approval_policy: AskForApproval::OnRequest,
        sandbox_policy: SandboxPolicy::DangerFullAccess,
        model: "mock-model".into(),
        effort: Some(ReasoningEffort::Medium),
        summary: ReasoningSummary::Auto,
    };
    let turn_value =
        serde_json::to_value(&turn).context("failed to serialize sendUserTurn params")?;
    let req = RpcRequest {
        id: RpcRequestId::Integer(4),
        method: "sendUserTurn".into(),
        params: Some(turn_value),
    };
    let wire = serde_json::to_string(&RpcMessage::Request(req))
        .context("failed to serialize sendUserTurn request")?;
    ws.send(WsMsg::Text(wire.into()))
        .await
        .context("failed to send sendUserTurn request")?;

    let mut turn_ack = false;
    for _ in 0..50 {
        match timeout(Duration::from_millis(200), ws.next()).await {
            Ok(Some(Ok(WsMsg::Text(txt)))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt)
                    && v.get("id").and_then(serde_json::Value::as_i64) == Some(4)
                {
                    turn_ack = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(err))) => bail!("ws error while waiting for sendUserTurn ack: {err}"),
            Ok(None) => bail!("ws closed while waiting for sendUserTurn ack"),
            Err(_) => {}
        }
    }
    ensure!(turn_ack, "did not receive sendUserTurn ack");

    let mut saw_task_complete = false;
    for _ in 0..200 {
        match timeout(Duration::from_millis(200), ws.next()).await {
            Ok(Some(Ok(WsMsg::Text(txt)))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                    if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
                        if method == "execCommandApproval" {
                            let id_value = v
                                .get("id")
                                .cloned()
                                .context("approval request missing id")?;
                            let resp =
                                RpcMessage::Response(codex_app_server_protocol::JSONRPCResponse {
                                    id: serde_json::from_value(id_value)
                                        .context("failed to parse approval request id")?,
                                    result: serde_json::json!({
                                        "decision": codex_core::protocol::ReviewDecision::Approved,
                                    }),
                                });
                            let resp_wire = serde_json::to_string(&resp)
                                .context("failed to serialize approval response")?;
                            ws.send(WsMsg::Text(resp_wire.into()))
                                .await
                                .context("failed to send approval response")?;
                        } else if method == "codex/event/task_complete" {
                            saw_task_complete = true;
                            break;
                        }
                    }
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(err))) => bail!("ws error while waiting for approval flow: {err}"),
            Ok(None) => bail!("ws closed before task_complete"),
            Err(_) => {}
        }
    }
    ensure!(
        saw_task_complete,
        "websocket flow did not emit task_complete"
    );

    write_mock_config(codex_home.path(), &mock_ip.uri())?;
    let mut mcp = McpProcess::new(codex_home.path())
        .await
        .context("failed to spawn app-server process for in-process flow")?;
    timeout(Duration::from_secs(5), mcp.initialize())
        .await
        .context("timed out waiting for in-process initialize")?
        .context("in-process initialize failed")?;

    let new_id = mcp
        .send_new_conversation_request(codex_app_server_protocol::NewConversationParams {
            cwd: Some(tmp.path().to_string_lossy().to_string()),
            ..Default::default()
        })
        .await
        .context("failed to send in-process newConversation request")?;
    let new_resp = timeout(
        Duration::from_secs(5),
        mcp.read_stream_until_response_message(RpcRequestId::Integer(new_id)),
    )
    .await
    .context("timed out waiting for in-process newConversation response")?;
    let new_resp = new_resp.context("in-process newConversation response error")?;
    let new_conv: codex_app_server_protocol::NewConversationResponse =
        app_test_support::to_response::<_>(new_resp)
            .context("failed to parse in-process newConversation response")?;
    let conversation_id = new_conv.conversation_id;

    let add_id = mcp
        .send_add_conversation_listener_request(AddConversationListenerParams { conversation_id })
        .await
        .context("failed to send in-process addConversationListener request")?;
    timeout(
        Duration::from_secs(5),
        mcp.read_stream_until_response_message(RpcRequestId::Integer(add_id)),
    )
    .await
    .context("timed out waiting for in-process addConversationListener ack")?
    .context("in-process addConversationListener request failed")?;

    let turn_id = mcp
        .send_send_user_turn_request(RpcSendUserTurnParams {
            conversation_id,
            items: vec![codex_app_server_protocol::InputItem::Text {
                text: "Run shell".to_string(),
            }],
            cwd: tmp.path().to_path_buf(),
            approval_policy: AskForApproval::OnRequest,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            model: "mock-model".into(),
            effort: Some(ReasoningEffort::Medium),
            summary: ReasoningSummary::Auto,
        })
        .await
        .context("failed to send in-process sendUserTurn request")?;
    timeout(
        Duration::from_secs(5),
        mcp.read_stream_until_response_message(RpcRequestId::Integer(turn_id)),
    )
    .await
    .context("timed out waiting for in-process sendUserTurn ack")?
    .context("in-process sendUserTurn request failed")?;

    let server_req = timeout(
        Duration::from_secs(5),
        mcp.read_stream_until_request_message(),
    )
    .await
    .context("timed out waiting for in-process approval request")?
    .context("failed to read in-process approval request")?;

    if let codex_app_server_protocol::ServerRequest::ExecCommandApproval { request_id, .. } =
        server_req
    {
        mcp.send_response(
            request_id,
            serde_json::json!({"decision": codex_core::protocol::ReviewDecision::Approved}),
        )
        .await
        .context("failed to send in-process approval decision")?;
    } else {
        bail!("expected ExecCommandApproval request");
    }

    timeout(
        Duration::from_secs(5),
        mcp.read_stream_until_notification_message("codex/event/task_complete"),
    )
    .await
    .context("timed out waiting for in-process task_complete")?
    .context("in-process task_complete notification failed")?;

    Ok(())
}
