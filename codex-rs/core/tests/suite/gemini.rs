use std::collections::HashMap;
use std::sync::Arc;

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
use core_test_support::load_default_config_for_test;
use core_test_support::skip_if_no_network;
use futures::StreamExt;
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
}
