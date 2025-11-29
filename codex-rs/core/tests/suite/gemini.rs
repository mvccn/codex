use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use codex_app_server_protocol::AuthMode;
use codex_core::ContentItem;
use codex_core::ModelClient;
use codex_core::ModelProviderInfo;
use codex_core::Prompt;
use codex_core::ResponseEvent;
use codex_core::ResponseItem;
use codex_core::WireApi;
use codex_otel::otel_event_manager::OtelEventManager;
use codex_protocol::ConversationId;
use codex_protocol::models::FunctionCallOutputPayload;
use core_test_support::load_default_config_for_test;
use core_test_support::skip_if_no_network;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

async fn build_gemini_client(
    server: &MockServer,
    base_path: &str,
    query_params: Option<HashMap<String, String>>,
) -> (ModelClient, Arc<codex_core::config::Config>) {
    let provider = ModelProviderInfo {
        name: "gemini-test".to_string(),
        base_url: Some(format!("{}{}", server.uri(), base_path)),
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api: WireApi::Gemini,
        query_params,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: Some(0),
        stream_max_retries: Some(0),
        stream_idle_timeout_ms: Some(5_000),
        requires_openai_auth: false,
    };

    let codex_home =
        TempDir::new().unwrap_or_else(|e| panic!("failed to create TempDir for config: {e}"));
    let mut config = load_default_config_for_test(&codex_home);
    config.model = "models/gemini-2.0-flash".to_string();
    config.model_family = codex_core::model_family::find_family_for_model(&config.model)
        .unwrap_or_else(|| codex_core::model_family::derive_default_model_family(&config.model));
    config.model_provider_id = provider.name.clone();
    config.model_provider = provider.clone();
    config.show_raw_agent_reasoning = true;
    let effort = config.model_reasoning_effort;
    let summary = config.model_reasoning_summary;
    let config = Arc::new(config);

    let conversation_id = ConversationId::new();
    let otel_event_manager = OtelEventManager::new(
        conversation_id,
        config.model.as_str(),
        config.model_family.slug.as_str(),
        None,
        Some("test@test.com".to_string()),
        Some(AuthMode::ChatGPT),
        false,
        "test".to_string(),
    );

    let client = ModelClient::new(
        Arc::clone(&config),
        None,
        otel_event_manager,
        provider,
        effort,
        summary,
        conversation_id,
        codex_protocol::protocol::SessionSource::Exec,
    );

    (client, config)
}

fn simple_user_prompt(text: &str) -> Prompt {
    let mut prompt = Prompt::default();
    prompt.input = vec![ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        thought_signature: None,
    }];
    prompt
}

fn extract_assistant_text(events: &[ResponseEvent]) -> String {
    let mut buf = String::new();
    for ev in events {
        match ev {
            ResponseEvent::OutputItemDone(ResponseItem::Message { role, content, .. })
                if role == "assistant" =>
            {
                for c in content {
                    if let ContentItem::OutputText { text } = c {
                        buf.push_str(text);
                    }
                }
            }
            ResponseEvent::OutputTextDelta(text) => {
                buf.push_str(text);
            }
            _ => {}
        }
    }
    buf
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_non_streaming_round_trip_and_payload_shape() {
    skip_if_no_network!();

    let server = MockServer::start().await;

    let body = json!({
        "candidates": [
            {
                "content": {
                    "parts": [
                        { "text": "Hello from Gemini" }
                    ],
                    "role": "model"
                }
            }
        ],
        "usageMetadata": {
            "promptTokenCount": 3,
            "candidatesTokenCount": 2,
            "totalTokenCount": 5
        }
    });

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.0-flash:generateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(body.clone()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let (client, _config) = build_gemini_client(
        &server,
        "/v1beta/models/gemini-2.0-flash:generateContent",
        None,
    )
    .await;

    let prompt = simple_user_prompt("hi");
    let mut stream = client.stream(&prompt).await.expect("stream");
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.expect("event should be Ok"));
    }

    let assistant_text = extract_assistant_text(&events);
    assert!(
        assistant_text.contains("Hello from Gemini"),
        "assistant text should come from Gemini response, got {assistant_text:?}"
    );

    let completed_usage = events.iter().find_map(|ev| {
        if let ResponseEvent::Completed { token_usage, .. } = ev {
            token_usage.clone()
        } else {
            None
        }
    });
    let usage = completed_usage.expect("token usage should be present");
    assert_eq!(usage.input_tokens, 3);
    assert_eq!(usage.output_tokens, 2);
    assert_eq!(usage.total_tokens, 5);

    let requests = server
        .received_requests()
        .await
        .expect("expected at least one request");
    assert_eq!(requests.len(), 1, "expected a single Gemini request");

    let payload: Value = requests[0]
        .body_json()
        .expect("Gemini request body should be JSON");
    assert!(
        payload.get("contents").is_some(),
        "Gemini payload must contain contents: {payload:?}"
    );
    assert!(
        !serde_json::to_string(&payload)
            .expect("serialize payload")
            .contains("additionalProperties"),
        "Gemini payload must not contain additionalProperties anywhere",
    );
    assert!(
        !serde_json::to_string(&payload)
            .expect("serialize payload")
            .contains("strict"),
        "Gemini payload must not contain strict anywhere",
    );

    let generation_config = payload
        .get("generationConfig")
        .and_then(Value::as_object)
        .expect("generationConfig");
    let thinking_config = generation_config
        .get("thinkingConfig")
        .and_then(Value::as_object)
        .expect("thinkingConfig");
    assert_eq!(
        thinking_config.get("thinkingLevel"),
        Some(&json!("HIGH")),
        "Gemini requests should propagate reasoning effort into thinking_level"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_non_streaming_includes_output_schema_and_thinking_level() {
    skip_if_no_network!();

    let server = MockServer::start().await;

    let body = json!({
        "candidates": [
            {
                "content": {
                    "parts": [
                        { "text": "structured" }
                    ],
                    "role": "model"
                }
            }
        ]
    });

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.0-flash:generateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(body.clone()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let (client, _config) = build_gemini_client(
        &server,
        "/v1beta/models/gemini-2.0-flash:generateContent",
        None,
    )
    .await;

    let mut prompt = simple_user_prompt("hi");
    prompt.output_schema = Some(json!({
        "type": "object",
        "properties": {
            "foo": { "type": "string" }
        },
        "required": ["foo"]
    }));

    let mut stream = client.stream(&prompt).await.expect("stream");
    while let Some(ev) = stream.next().await {
        ev.expect("event should be Ok");
    }

    let requests = server
        .received_requests()
        .await
        .expect("expected at least one request");
    let payload: Value = requests[0]
        .body_json()
        .expect("Gemini request body should be JSON");

    let generation_config = payload
        .get("generationConfig")
        .and_then(Value::as_object)
        .expect("generationConfig");
    let thinking_config = generation_config
        .get("thinkingConfig")
        .and_then(Value::as_object)
        .expect("thinkingConfig");
    assert_eq!(thinking_config.get("thinkingLevel"), Some(&json!("HIGH")));
    assert_eq!(
        generation_config.get("responseMimeType"),
        Some(&json!("application/json"))
    );
    let schema = generation_config
        .get("responseJsonSchema")
        .and_then(Value::as_object)
        .expect("responseJsonSchema");
    assert_eq!(schema.get("type"), Some(&json!("object")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_streaming_sse_round_trip() {
    skip_if_no_network!();

    let server = MockServer::start().await;

    let sse_body = "\
data: {\"result\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}]}}]}}\n\
\n\
data: {\"result\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" world\"}]}}]}}\n\
\n\
data: {\"result\":{\"usageMetadata\":{\"promptTokenCount\":4,\"candidatesTokenCount\":3,\"totalTokenCount\":7}}}\n\
\n\
data: [DONE]\n\
\n";

    Mock::given(method("POST"))
        .and(path(
            "/v1beta/models/gemini-2.0-flash:streamGenerateContent",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut query_params = HashMap::new();
    query_params.insert("alt".to_string(), "sse".to_string());
    let (client, _config) = build_gemini_client(
        &server,
        "/v1beta/models/gemini-2.0-flash:streamGenerateContent",
        Some(query_params),
    )
    .await;

    let prompt = simple_user_prompt("hi");
    let mut stream = client.stream(&prompt).await.expect("stream");
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.expect("event should be Ok"));
    }

    let assistant_text = extract_assistant_text(&events);
    assert!(
        assistant_text.contains("Hello world"),
        "expected concatenated streaming text, got {assistant_text:?}"
    );

    let completed_usage = events.iter().find_map(|ev| {
        if let ResponseEvent::Completed { token_usage, .. } = ev {
            token_usage.clone()
        } else {
            None
        }
    });
    let usage = completed_usage.expect("token usage should be present");
    assert_eq!(usage.input_tokens, 4);
    assert_eq!(usage.output_tokens, 3);
    assert_eq!(usage.total_tokens, 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_non_streaming_emits_function_call() {
    skip_if_no_network!();

    let server = MockServer::start().await;

    let body = json!({
        "candidates": [
            {
                "content": {
                    "parts": [
                        {
                            "functionCall": {
                                "name": "write_file",
                                "args": { "path": "/tmp/demo.txt", "contents": "hello" }
                            }
                        }
                    ],
                    "role": "model"
                }
            }
        ]
    });

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.0-flash:generateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(body.clone()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let (client, _config) = build_gemini_client(
        &server,
        "/v1beta/models/gemini-2.0-flash:generateContent",
        None,
    )
    .await;

    let prompt = simple_user_prompt("write a file");
    let mut stream = client.stream(&prompt).await.expect("stream");
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.expect("event should be Ok"));
    }

    let function_call = events.iter().find_map(|ev| match ev {
        ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
            name,
            arguments,
            call_id,
            ..
        }) => Some((name, arguments, call_id)),
        _ => None,
    });

    let (name, arguments, call_id) =
        function_call.expect("expected a function call event from Gemini response");
    assert_eq!(name, "write_file");
    assert_eq!(call_id, "gemini_call_1");

    let args: Value = serde_json::from_str(arguments).expect("args should be valid JSON");
    assert_eq!(args["path"], json!("/tmp/demo.txt"));
    assert_eq!(args["contents"], json!("hello"));

    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, ResponseEvent::Completed { .. })),
        "expected stream completion event"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_streaming_flushes_text_before_tool_call() {
    skip_if_no_network!();

    let server = MockServer::start().await;

    let sse_body = "\
data: {\"result\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Write \"}]}}]}}\n\
\n\
data: {\"result\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"append_file\",\"args\":{\"path\":\"/tmp/log.txt\",\"content\":\"details\"}}}]}}]}}\n\
\n\
data: [DONE]\n\
\n";

    Mock::given(method("POST"))
        .and(path(
            "/v1beta/models/gemini-2.0-flash:streamGenerateContent",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut query_params = HashMap::new();
    query_params.insert("alt".to_string(), "sse".to_string());
    let (client, _config) = build_gemini_client(
        &server,
        "/v1beta/models/gemini-2.0-flash:streamGenerateContent",
        Some(query_params),
    )
    .await;

    let prompt = simple_user_prompt("prepare file");
    let mut stream = client.stream(&prompt).await.expect("stream");
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.expect("event should be Ok"));
    }

    let mut deltas = String::new();
    for ev in &events {
        if let ResponseEvent::OutputTextDelta(text) = ev {
            deltas.push_str(text);
        }
    }
    assert_eq!(deltas, "Write ");

    let mut order = Vec::new();
    let mut function_call = None;
    for ev in events {
        match ev {
            ResponseEvent::OutputItemDone(ResponseItem::Message { .. }) => {
                order.push("assistant_done")
            }
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            }) => {
                order.push("function_call");
                function_call = Some((name, arguments, call_id));
            }
            ResponseEvent::Completed { .. } => order.push("completed"),
            ResponseEvent::OutputItemAdded(_) | ResponseEvent::OutputTextDelta(_) => {}
            _ => {}
        }
    }

    assert_eq!(
        order,
        vec!["assistant_done", "function_call", "completed"],
        "expected assistant message to flush before tool call"
    );

    let (name, arguments, call_id) =
        function_call.expect("expected function call after assistant text");
    assert_eq!(name, "append_file");
    assert_eq!(call_id, "gemini_call_1");

    let args: Value = serde_json::from_str(&arguments).expect("arguments should parse");
    assert_eq!(args["path"], json!("/tmp/log.txt"));
    assert_eq!(args["content"], json!("details"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_streaming_handles_text_and_function_call_in_same_chunk() {
    skip_if_no_network!();

    let server = MockServer::start().await;

    let sse_body = "\
data: {\"result\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Plan and call tool\"},{\"functionCall\":{\"name\":\"append_file\",\"args\":{\"path\":\"/tmp/log.txt\",\"content\":\"details\"}}}]}}]}}\n\
\n\
data: [DONE]\n\
\n";

    Mock::given(method("POST"))
        .and(path(
            "/v1beta/models/gemini-2.0-flash:streamGenerateContent",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut query_params = HashMap::new();
    query_params.insert("alt".to_string(), "sse".to_string());
    let (client, _config) = build_gemini_client(
        &server,
        "/v1beta/models/gemini-2.0-flash:streamGenerateContent",
        Some(query_params),
    )
    .await;

    let prompt = simple_user_prompt("prepare file");
    let mut stream = client.stream(&prompt).await.expect("stream");
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.expect("event should be Ok"));
    }

    let assistant_text = extract_assistant_text(&events);
    assert!(
        assistant_text.contains("Plan and call tool"),
        "assistant text should be present when text and functionCall share a chunk"
    );

    let mut order = Vec::new();
    let mut function_call = None;
    for ev in events {
        match ev {
            ResponseEvent::OutputItemDone(ResponseItem::Message { .. }) => {
                order.push("assistant_done")
            }
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            }) => {
                order.push("function_call");
                function_call = Some((name, arguments, call_id));
            }
            ResponseEvent::Completed { .. } => order.push("completed"),
            ResponseEvent::OutputItemAdded(_) | ResponseEvent::OutputTextDelta(_) => {}
            _ => {}
        }
    }

    assert_eq!(
        order,
        vec!["assistant_done", "function_call", "completed"],
        "function call should not be dropped when chunk also contains text"
    );

    let (name, arguments, call_id) =
        function_call.expect("expected function call even when text precedes it");
    assert_eq!(name, "append_file");
    assert_eq!(call_id, "gemini_call_1");

    let args: Value = serde_json::from_str(&arguments).expect("arguments should parse");
    assert_eq!(args["path"], json!("/tmp/log.txt"));
    assert_eq!(args["content"], json!("details"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_non_streaming_emits_multiple_function_calls_with_rich_arguments() {
    skip_if_no_network!();

    let server = MockServer::start().await;

    let body = json!({
        "candidates": [
            {
                "content": {
                    "parts": [
                        {
                            "functionCall": {
                                "name": "append_file",
                                "args": {
                                    "path": "/tmp/activity.log",
                                    "content": "first line",
                                    "mode": "append",
                                    "permissions": "0644"
                                }
                            }
                        },
                        {
                            "functionCall": {
                                "name": "shell",
                                "args": {
                                    "command": "ls /tmp",
                                    "timeout_ms": 5000,
                                    "env": { "DEBUG": "1" }
                                }
                            }
                        }
                    ],
                    "role": "model"
                }
            }
        ]
    });

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.0-flash:generateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(body.clone()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let (client, _config) = build_gemini_client(
        &server,
        "/v1beta/models/gemini-2.0-flash:generateContent",
        None,
    )
    .await;

    let prompt = simple_user_prompt("make two tool calls");
    let mut stream = client.stream(&prompt).await.expect("stream");
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.expect("event should be Ok"));
    }

    let mut function_calls = Vec::new();
    for ev in &events {
        if let ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
            name,
            arguments,
            call_id,
            ..
        }) = ev
        {
            function_calls.push((name.clone(), arguments.clone(), call_id.clone()));
        }
    }

    assert_eq!(
        function_calls.len(),
        2,
        "expected two function calls from Gemini payload"
    );

    let (first_name, first_args, first_call_id) = &function_calls[0];
    assert_eq!(first_name, "append_file");
    assert_eq!(first_call_id, "gemini_call_1");
    let first_json: Value = serde_json::from_str(first_args).expect("first call args");
    assert_eq!(first_json["path"], json!("/tmp/activity.log"));
    assert_eq!(first_json["mode"], json!("append"));
    assert_eq!(first_json["permissions"], json!("0644"));
    assert_eq!(first_json["content"], json!("first line"));

    let (second_name, second_args, second_call_id) = &function_calls[1];
    assert_eq!(second_name, "shell");
    assert_eq!(second_call_id, "gemini_call_2");
    let second_json: Value = serde_json::from_str(second_args).expect("second call args");
    assert_eq!(second_json["command"], json!("ls /tmp"));
    assert_eq!(second_json["timeout_ms"], json!(5000));
    assert_eq!(second_json["env"]["DEBUG"], json!("1"));

    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, ResponseEvent::Completed { .. })),
        "expected stream completion event"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_streaming_emits_exec_function_call_with_error_details() {
    skip_if_no_network!();

    let server = MockServer::start().await;

    let sse_body = "\
data: {\"result\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Running command\"}]}}]}}\n\
\n\
data: {\"result\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"shell\",\"args\":{\"command\":\"cat restricted.txt\",\"exit_code\":1,\"stderr\":\"permission denied\"}}}]}}]}}\n\
\n\
data: [DONE]\n\
\n";

    Mock::given(method("POST"))
        .and(path(
            "/v1beta/models/gemini-2.0-flash:streamGenerateContent",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut query_params = HashMap::new();
    query_params.insert("alt".to_string(), "sse".to_string());
    let (client, _config) = build_gemini_client(
        &server,
        "/v1beta/models/gemini-2.0-flash:streamGenerateContent",
        Some(query_params),
    )
    .await;

    let prompt = simple_user_prompt("run exec");
    let mut stream = client.stream(&prompt).await.expect("stream");
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.expect("event should be Ok"));
    }

    let deltas: String = events
        .iter()
        .filter_map(|ev| {
            if let ResponseEvent::OutputTextDelta(text) = ev {
                Some(text.clone())
            } else {
                None
            }
        })
        .collect();
    assert!(
        deltas.contains("Running command"),
        "expected assistant text delta before tool call"
    );

    let function_call = events.iter().find_map(|ev| match ev {
        ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
            name,
            arguments,
            call_id,
            ..
        }) => Some((name, arguments, call_id)),
        _ => None,
    });

    let (name, arguments, call_id) =
        function_call.expect("expected a function call event from Gemini stream");
    assert_eq!(name, "shell");
    assert_eq!(call_id, "gemini_call_1");

    let args: Value = serde_json::from_str(arguments).expect("args should parse");
    assert_eq!(args["command"], json!("cat restricted.txt"));
    assert_eq!(args["exit_code"], json!(1));
    assert_eq!(args["stderr"], json!("permission denied"));

    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, ResponseEvent::Completed { .. })),
        "expected stream completion event"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_payload_includes_tool_outputs_in_followup_turn() {
    skip_if_no_network!();

    let server = MockServer::start().await;

    let body = json!({
        "candidates": [
            {
                "content": {
                    "parts": [
                        { "text": "ok" }
                    ],
                    "role": "model"
                }
            }
        ]
    });

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.0-flash:generateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(body.clone()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let (client, _config) = build_gemini_client(
        &server,
        "/v1beta/models/gemini-2.0-flash:generateContent",
        None,
    )
    .await;

    let mut prompt = Prompt::default();
    prompt.input.push(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "run tools".to_string(),
        }],
        thought_signature: None,
    });
    prompt.input.push(ResponseItem::FunctionCall {
        id: None,
        name: "write_file".to_string(),
        arguments: json!({ "path": "/tmp/demo.txt", "contents": "hello" }).to_string(),
        call_id: "call-1".to_string(),
        thought_signature: Some("Y29kZXhfdGhvdWdodF9zaWdfY2FsbC0x".to_string()),
    });
    prompt.input.push(ResponseItem::FunctionCallOutput {
        call_id: "call-1".to_string(),
        output: FunctionCallOutputPayload {
            content: "file saved".to_string(),
            ..Default::default()
        },
        thought_signature: Some("Y29kZXhfdGhvdWdodF9zaWdfY2FsbC0x".to_string()),
    });
    prompt.input.push(ResponseItem::CustomToolCallOutput {
        call_id: "call-2".to_string(),
        output: "exec done".to_string(),
    });

    let mut stream = client.stream(&prompt).await.expect("stream");
    while let Some(ev) = stream.next().await {
        ev.expect("event should be Ok");
    }

    let requests = server
        .received_requests()
        .await
        .expect("expected a follow-up Gemini request");
    assert_eq!(requests.len(), 1, "expected a single follow-up request");

    let payload: Value = requests[0]
        .body_json()
        .expect("Gemini follow-up request body should be JSON");
    let contents = payload["contents"].as_array().cloned().unwrap_or_default();

    let function_call_entry = contents
        .iter()
        .find(|c| {
            c["parts"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| part.get("functionCall").is_some()))
        })
        .expect("functionCall part missing from Gemini payload");
    assert_eq!(
        function_call_entry["role"],
        json!("model"),
        "functionCall should be emitted as a model role entry"
    );
    let function_call = function_call_entry["parts"][0]["functionCall"]
        .as_object()
        .expect("functionCall shape");
    assert_eq!(
        function_call.get("name"),
        Some(&json!("write_file")),
        "functionCall should include the function name"
    );
    let function_call_sig = function_call_entry["parts"][0]
        .get("thoughtSignature")
        .and_then(Value::as_str)
        .expect("thought signature value");
    let decoded_sig = STANDARD_NO_PAD
        .decode(function_call_sig.as_bytes())
        .expect("base64 thought signature");
    let decoded_sig = String::from_utf8(decoded_sig).expect("utf8 thought signature");
    assert!(
        decoded_sig.starts_with("codex_thought_sig_call-1"),
        "expected prefixed thought signature, got {decoded_sig:?}"
    );
    let call_args = function_call
        .get("args")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        call_args.get("path"),
        Some(&json!("/tmp/demo.txt")),
        "functionCall should include arguments"
    );

    let function_entry = contents
        .iter()
        .find(|c| {
            c["parts"].as_array().is_some_and(|parts| {
                parts
                    .iter()
                    .any(|part| part.get("functionResponse").is_some())
            })
        })
        .expect("functionResponse part missing from Gemini payload");
    assert_eq!(
        function_entry["role"],
        json!("user"),
        "functionResponse payloads should be wrapped as a user role entry"
    );
    let function_response = function_entry["parts"][0]["functionResponse"]
        .as_object()
        .expect("functionResponse shape");
    assert_eq!(
        function_response.get("name"),
        Some(&json!("write_file")),
        "functionResponse should include the function name"
    );
    let function_response_sig = function_entry["parts"][0]
        .get("thoughtSignature")
        .and_then(Value::as_str)
        .expect("thought signature value");
    let decoded_response_sig = STANDARD_NO_PAD
        .decode(function_response_sig.as_bytes())
        .expect("base64 thought signature");
    let decoded_response_sig =
        String::from_utf8(decoded_response_sig).expect("utf8 thought signature");
    assert!(
        decoded_response_sig.starts_with("codex_thought_sig_call-1"),
        "expected prefixed thought signature, got {decoded_response_sig:?}"
    );
    let function_content_text = function_response
        .get("response")
        .and_then(|v| v.get("content"))
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|v| v.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        function_content_text.contains("file saved"),
        "functionResponse should carry the tool output"
    );

    let body = serde_json::to_string(&payload).expect("serialize payload");
    assert!(
        body.contains("Custom tool call-2 result:\\nexec done"),
        "expected custom tool call output to be included in contents"
    );
}
