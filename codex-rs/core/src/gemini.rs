use std::sync::Arc;
use std::time::Duration;

use crate::AuthManager;
use crate::ModelProviderInfo;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use crate::default_client::CodexHttpClient;
use crate::error::CodexErr;
use crate::error::ConnectionFailedError;
use crate::error::ResponseStreamFailed;
use crate::error::Result;
use crate::error::UnexpectedResponseError;
use crate::model_family::ModelFamily;
use crate::tools::spec::create_tools_json_for_gemini_api;
use bytes::Bytes;
use codex_otel::otel_event_manager::OtelEventManager;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use eventsource_stream::Eventsource;
use futures::Stream;
use futures::StreamExt;
use futures::TryStreamExt;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::debug;

/// Native Gemini adapter dispatcher. Uses streaming (`streamGenerateContent`)
/// when the provider is configured for it (typically via a base URL that
/// contains `:streamGenerateContent` or a query parameter `alt=sse`), and
/// falls back to a single `generateContent` request otherwise.
pub(crate) async fn stream_gemini(
    prompt: &Prompt,
    model_family: &ModelFamily,
    client: &CodexHttpClient,
    provider: &ModelProviderInfo,
    otel_event_manager: &OtelEventManager,
    _session_source: &SessionSource,
    auth_manager: &Option<Arc<AuthManager>>,
) -> Result<ResponseStream> {
    let payload = build_gemini_payload(prompt, model_family)?;

    if provider_requests_streaming(provider) {
        stream_gemini_streaming(payload, client, provider, otel_event_manager, auth_manager).await
    } else {
        stream_gemini_once(payload, client, provider, otel_event_manager, auth_manager).await
    }
}

/// Process Gemini SSE events by forwarding assistant deltas + tool calls
/// as Codex `ResponseEvent`s.
async fn process_gemini_sse<S>(
    stream: S,
    tx_event: mpsc::Sender<Result<ResponseEvent>>,
    idle_timeout: Duration,
    otel_event_manager: OtelEventManager,
) where
    S: Stream<Item = Result<Bytes>> + Unpin,
{
    let mut stream = stream.eventsource();
    let mut assistant_item: Option<ResponseItem> = None;

    loop {
        let response = timeout(idle_timeout, stream.next()).await;
        otel_event_manager.log_sse_event(&response, idle_timeout);

        let sse = match response {
            Ok(Some(Ok(ev))) => ev,
            Ok(Some(Err(e))) => {
                let _ = tx_event
                    .send(Err(CodexErr::Stream(e.to_string(), None)))
                    .await;
                return;
            }
            Ok(None) => {
                if let Some(item) = assistant_item.take() {
                    let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await;
                }
                let _ = tx_event
                    .send(Ok(ResponseEvent::Completed {
                        response_id: String::new(),
                        token_usage: None,
                    }))
                    .await;
                return;
            }
            Err(_) => {
                let _ = tx_event
                    .send(Err(CodexErr::Stream(
                        "idle timeout waiting for SSE".into(),
                        None,
                    )))
                    .await;
                return;
            }
        };

        let data = sse.data.trim();
        if data.is_empty() {
            continue;
        }

        if data.eq_ignore_ascii_case("[DONE]") {
            if let Some(item) = assistant_item.take() {
                let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await;
            }
            let _ = tx_event
                .send(Ok(ResponseEvent::Completed {
                    response_id: String::new(),
                    token_usage: None,
                }))
                .await;
            return;
        }

        let chunk: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                debug!("Failed to parse Gemini SSE chunk: {e}");
                continue;
            }
        };

        let payload = chunk.get("result").cloned().unwrap_or(chunk);
        let parsed = parse_gemini_response(&payload);

        if let Some(text) = parsed.text {
            if append_assistant_text(&tx_event, &mut assistant_item, text).await {
                continue;
            } else {
                return;
            }
        }

        if !parsed.function_calls.is_empty() {
            if let Some(item) = assistant_item.take()
                && tx_event
                    .send(Ok(ResponseEvent::OutputItemDone(item)))
                    .await
                    .is_err()
            {
                return;
            }

            for (idx, fc) in parsed.function_calls.into_iter().enumerate() {
                let call_id = format!("gemini_call_{}", idx + 1);
                let arguments = match serde_json::to_string(&fc.args) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = tx_event.send(Err(CodexErr::Json(e))).await;
                        return;
                    }
                };

                let item = ResponseItem::FunctionCall {
                    id: None,
                    name: fc.name,
                    arguments,
                    call_id,
                };

                if tx_event
                    .send(Ok(ResponseEvent::OutputItemDone(item)))
                    .await
                    .is_err()
                {
                    return;
                }
            }

            let _ = tx_event
                .send(Ok(ResponseEvent::Completed {
                    response_id: String::new(),
                    token_usage: None,
                }))
                .await;
            return;
        }
    }
}

/// Minimal representation of a Gemini function call used for translation
/// into Codex `ResponseItem::FunctionCall`.
struct GeminiFunctionCall {
    name: String,
    args: Value,
}

/// Parsed Gemini response fields that Codex cares about: assistant text
/// and function calls. We currently focus on the first candidate.
struct ParsedGeminiResponse {
    text: Option<String>,
    function_calls: Vec<GeminiFunctionCall>,
}

fn parse_gemini_response(root: &Value) -> ParsedGeminiResponse {
    let mut text: Option<String> = None;
    let mut function_calls: Vec<GeminiFunctionCall> = Vec::new();

    // Preferred path: candidates[0].content.parts[*]
    if let Some(candidates) = root.get("candidates").and_then(|v| v.as_array())
        && let Some(first) = candidates.first()
        && let Some(content) = first.get("content")
        && let Some(parts) = content.get("parts").and_then(|v| v.as_array())
    {
        let mut buf = String::new();
        for part in parts {
            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                buf.push_str(t);
            }

            if let Some(fc_val) = part
                .get("functionCall")
                .or_else(|| part.get("function_call"))
                && let Some(name) = fc_val.get("name").and_then(|v| v.as_str())
            {
                let args = fc_val.get("args").cloned().unwrap_or(Value::Null);
                function_calls.push(GeminiFunctionCall {
                    name: name.to_string(),
                    args,
                });
            }
        }

        if !buf.is_empty() {
            text = Some(buf);
        }
    }

    // Fallback path: top‑level OpenAPI‑compatible functionCalls array.
    if let Some(fcs) = root.get("functionCalls").and_then(|v| v.as_array()) {
        for fc in fcs {
            if let Some(name) = fc.get("name").and_then(|v| v.as_str()) {
                let args = fc.get("args").cloned().unwrap_or(Value::Null);
                function_calls.push(GeminiFunctionCall {
                    name: name.to_string(),
                    args,
                });
            }
        }
    }

    ParsedGeminiResponse {
        text,
        function_calls,
    }
}

/// Translate Codex `ResponseItem` history into Gemini `contents`. This is a
/// best‑effort mapping that:
///   * preserves the user/assistant roles where possible
///   * encodes text as `parts[{text: ...}]`
///   * flattens tool outputs into user-visible text snippets
fn build_gemini_contents(items: &[ResponseItem]) -> Vec<Value> {
    let mut contents: Vec<Value> = Vec::new();
    let mut current_role: Option<String> = None;
    let mut current_parts: Vec<Value> = Vec::new();

    let flush = |role: &mut Option<String>, parts: &mut Vec<Value>, contents: &mut Vec<Value>| {
        if parts.is_empty() {
            return;
        }

        let role_val = role.as_deref().unwrap_or("user").to_string();
        contents.push(json!({
            "role": role_val,
            "parts": parts.clone(),
        }));
        parts.clear();
    };

    for item in items {
        match item {
            ResponseItem::Message { role, content, .. } => {
                // Map Codex roles to Gemini roles.
                let gemini_role = if role == "assistant" {
                    "model".to_string()
                } else {
                    "user".to_string()
                };

                if current_role.as_deref() != Some(gemini_role.as_str()) {
                    flush(&mut current_role, &mut current_parts, &mut contents);
                    current_role = Some(gemini_role);
                }

                for c in content {
                    match c {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            current_parts.push(json!({ "text": text }));
                        }
                        ContentItem::InputImage { image_url } => {
                            // Minimal image support: describe the image path/URL
                            // as text so Gemini can still reason about it.
                            current_parts.push(json!({
                                "text": format!("Image: {image_url}"),
                            }));
                        }
                    }
                }
            }
            ResponseItem::FunctionCallOutput { call_id, output } => {
                // Present tool outputs as user-visible text so the model
                // can incorporate them into follow‑up reasoning.
                if current_role.as_deref() != Some("user") {
                    flush(&mut current_role, &mut current_parts, &mut contents);
                    current_role = Some("user".to_string());
                }

                current_parts.push(json!({
                    "text": format!("Tool {call_id} result:\n{}", output.content),
                }));
            }
            ResponseItem::CustomToolCallOutput { call_id, output } => {
                if current_role.as_deref() != Some("user") {
                    flush(&mut current_role, &mut current_parts, &mut contents);
                    current_role = Some("user".to_string());
                }

                current_parts.push(json!({
                    "text": format!("Custom tool {call_id} result:\n{output}"),
                }));
            }
            // Skip agent‑internal items that should not be sent to the model.
            ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::GhostSnapshot { .. }
            | ResponseItem::CompactionSummary { .. }
            | ResponseItem::Other => {}
        }
    }

    flush(&mut current_role, &mut current_parts, &mut contents);
    contents
}
// (all Gemini-specific tests live in `core/tests/gemini_*.rs` to keep the
// production code path focused; see those files for adapter tests.)

fn provider_requests_streaming(provider: &ModelProviderInfo) -> bool {
    provider
        .base_url
        .as_deref()
        .map(|u| u.contains(":streamGenerateContent"))
        .unwrap_or(false)
        || provider.query_params.as_ref().is_some_and(|params| {
            params
                .get("alt")
                .map(|v| v.eq_ignore_ascii_case("sse"))
                .unwrap_or(false)
        })
}

fn build_gemini_payload(prompt: &Prompt, model_family: &ModelFamily) -> Result<Value> {
    if prompt.output_schema.is_some() {
        return Err(CodexErr::UnsupportedOperation(
            "output_schema is not supported for Gemini wire_api".to_string(),
        ));
    }

    let full_instructions = prompt.get_full_instructions(model_family);
    let input_with_instructions = prompt.get_formatted_input();

    let contents_json = build_gemini_contents(&input_with_instructions);
    let tools_json = create_tools_json_for_gemini_api(&prompt.tools)?;

    let mut payload = json!({
        "contents": contents_json,
    });

    if !full_instructions.is_empty() {
        payload["systemInstruction"] = json!({
            "role": "system",
            "parts": [ { "text": full_instructions } ],
        });
    }

    if !tools_json.is_empty() {
        payload["tools"] = Value::Array(tools_json);
        payload["toolConfig"] = json!({
            "functionCallingConfig": {
                "mode": "AUTO",
            }
        });
    }

    Ok(payload)
}

async fn stream_gemini_once(
    payload: Value,
    client: &CodexHttpClient,
    provider: &ModelProviderInfo,
    otel_event_manager: &OtelEventManager,
    _auth_manager: &Option<Arc<AuthManager>>,
) -> Result<ResponseStream> {
    // Gemini authentication is driven entirely by API keys passed via
    // `env_http_headers` / `http_headers`. Avoid forwarding any existing
    // ChatGPT auth token as an `Authorization: Bearer` header, since the
    // Gemini endpoint does not accept it and will respond with 401 when an
    // unexpected credential is present.
    let auth: Option<crate::auth::CodexAuth> = None;
    let target_url = provider.get_full_url(&auth);
    debug!(
        "Gemini request (non-streaming) -> {} payload={}",
        target_url, payload
    );

    let mut req_builder = provider.create_request_builder(client, &auth).await?;

    req_builder = req_builder
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&payload);

    let res = otel_event_manager
        .log_request(0, || req_builder.send())
        .await;

    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent>>(16);

    tokio::spawn(async move {
        match res {
            Ok(resp) => {
                let status = resp.status();

                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    let err = CodexErr::UnexpectedStatus(UnexpectedResponseError {
                        status,
                        body,
                        request_id: None,
                    });
                    let _ = tx_event.send(Err(err)).await;
                    return;
                }

                let body_bytes = match resp.bytes().await {
                    Ok(b) => b,
                    Err(e) => {
                        let err = CodexErr::ConnectionFailed(ConnectionFailedError { source: e });
                        let _ = tx_event.send(Err(err)).await;
                        return;
                    }
                };

                let body_val: Value = match serde_json::from_slice(&body_bytes) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = tx_event.send(Err(CodexErr::Json(e))).await;
                        return;
                    }
                };

                let parsed = parse_gemini_response(&body_val);

                if let Some(text) = parsed.text
                    && !text.is_empty()
                {
                    let item = ResponseItem::Message {
                        id: None,
                        role: "assistant".to_string(),
                        content: vec![ContentItem::OutputText { text }],
                    };
                    if tx_event
                        .send(Ok(ResponseEvent::OutputItemAdded(item.clone())))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    if tx_event
                        .send(Ok(ResponseEvent::OutputItemDone(item)))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }

                for (idx, fc) in parsed.function_calls.into_iter().enumerate() {
                    let call_id = format!("gemini_call_{}", idx + 1);
                    let arguments = match serde_json::to_string(&fc.args) {
                        Ok(s) => s,
                        Err(e) => {
                            let _ = tx_event.send(Err(CodexErr::Json(e))).await;
                            return;
                        }
                    };

                    let item = ResponseItem::FunctionCall {
                        id: None,
                        name: fc.name,
                        arguments,
                        call_id,
                    };

                    if tx_event
                        .send(Ok(ResponseEvent::OutputItemDone(item)))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }

                let _ = tx_event
                    .send(Ok(ResponseEvent::Completed {
                        response_id: String::new(),
                        token_usage: None,
                    }))
                    .await;
            }
            Err(e) => {
                let err = CodexErr::ConnectionFailed(ConnectionFailedError { source: e });
                let _ = tx_event.send(Err(err)).await;
            }
        }
    });

    Ok(ResponseStream { rx_event })
}

async fn stream_gemini_streaming(
    payload: Value,
    client: &CodexHttpClient,
    provider: &ModelProviderInfo,
    otel_event_manager: &OtelEventManager,
    _auth_manager: &Option<Arc<AuthManager>>,
) -> Result<ResponseStream> {
    let auth: Option<crate::auth::CodexAuth> = None;
    let target_url = provider.get_full_url(&auth);
    debug!(
        "Gemini request (streaming) -> {} payload={}",
        target_url, payload
    );
    let mut req_builder = provider.create_request_builder(client, &auth).await?;

    req_builder = req_builder
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .json(&payload);

    let res = otel_event_manager
        .log_request(0, || req_builder.send())
        .await;

    match res {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(CodexErr::UnexpectedStatus(UnexpectedResponseError {
                    status,
                    body,
                    request_id: None,
                }));
            }

            let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent>>(1600);

            let stream = resp.bytes_stream().map_err(|e| {
                CodexErr::ResponseStreamFailed(ResponseStreamFailed {
                    source: e,
                    request_id: None,
                })
            });

            tokio::spawn(process_gemini_sse(
                stream,
                tx_event,
                provider.stream_idle_timeout(),
                otel_event_manager.clone(),
            ));

            Ok(ResponseStream { rx_event })
        }
        Err(e) => Err(CodexErr::ConnectionFailed(ConnectionFailedError {
            source: e,
        })),
    }
}

async fn append_assistant_text(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    assistant_item: &mut Option<ResponseItem>,
    text: String,
) -> bool {
    if text.is_empty() {
        return true;
    }

    if assistant_item.is_none() {
        let item = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: Vec::new(),
        };
        if tx_event
            .send(Ok(ResponseEvent::OutputItemAdded(item.clone())))
            .await
            .is_err()
        {
            return false;
        }
        *assistant_item = Some(item);
    }

    if let Some(ResponseItem::Message { content, .. }) = assistant_item {
        content.push(ContentItem::OutputText { text: text.clone() });
        if tx_event
            .send(Ok(ResponseEvent::OutputTextDelta(text)))
            .await
            .is_err()
        {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod gemini_tests {
    use super::*;
    use crate::client_common::Prompt;
    use crate::model_family::derive_default_model_family;

    #[test]
    fn parse_gemini_response_handles_text_and_function_call() {
        let json = json!({
            "candidates": [
                {
                    "content": {
                        "parts": [
                            { "text": "Hello " },
                            {
                                "functionCall": {
                                    "name": "do_something",
                                    "args": { "x": 1 }
                                }
                            }
                        ]
                    }
                }
            ]
        });

        let parsed = parse_gemini_response(&json);
        assert_eq!(parsed.text.as_deref(), Some("Hello "));
        assert_eq!(parsed.function_calls.len(), 1);
        assert_eq!(parsed.function_calls[0].name, "do_something");
        assert_eq!(parsed.function_calls[0].args["x"], json!(1));
    }

    #[test]
    fn parse_gemini_response_handles_top_level_function_calls() {
        let json = json!({
            "functionCalls": [
                {
                    "name": "top_level",
                    "args": { "a": "b" }
                }
            ]
        });

        let parsed = parse_gemini_response(&json);
        assert!(parsed.text.is_none());
        assert_eq!(parsed.function_calls.len(), 1);
        assert_eq!(parsed.function_calls[0].name, "top_level");
        assert_eq!(parsed.function_calls[0].args["a"], json!("b"));
    }

    #[test]
    fn build_gemini_contents_maps_roles_and_tool_outputs() {
        let mut items = Vec::<ResponseItem>::new();

        // User message
        items.push(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "hi there".to_string(),
            }],
        });

        // Assistant message
        items.push(ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "ok".to_string(),
            }],
        });

        // Tool output, both as FunctionCallOutput and CustomToolCallOutput.
        items.push(ResponseItem::FunctionCallOutput {
            call_id: "call-1".to_string(),
            output: codex_protocol::models::FunctionCallOutputPayload {
                content: "result-1".to_string(),
                ..Default::default()
            },
        });
        items.push(ResponseItem::CustomToolCallOutput {
            call_id: "call-2".to_string(),
            output: "result-2".to_string(),
        });

        let contents = build_gemini_contents(&items);
        assert!(!contents.is_empty(), "expected contents to be non-empty");

        // First content should be user role carrying the initial text.
        assert_eq!(contents[0]["role"], json!("user"));
        let first_text = contents[0]["parts"][0]["text"].as_str().unwrap_or_default();
        assert!(
            first_text.contains("hi there"),
            "expected first content to include user text"
        );

        // There should be a model role entry.
        assert!(
            contents.iter().any(|c| c["role"] == json!("model")),
            "expected at least one model role content"
        );

        // Tool outputs should be rendered as user-role text mentioning the call id.
        let body = serde_json::to_string(&contents).unwrap();
        assert!(
            body.contains("Tool call-1 result:\\nresult-1"),
            "expected function tool output to be present"
        );
        assert!(
            body.contains("Custom tool call-2 result:\\nresult-2"),
            "expected custom tool output to be present"
        );
    }

    #[test]
    fn build_gemini_payload_includes_system_instruction_and_contents() {
        let model = "models/gemini-2.0-flash";
        let family = derive_default_model_family(model);

        let mut prompt = Prompt::default();
        prompt.input = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "test".to_string(),
            }],
        }];

        let payload = build_gemini_payload(&prompt, &family).expect("payload");

        assert!(payload.get("contents").is_some(), "contents missing");
        assert!(
            payload.get("systemInstruction").is_some(),
            "systemInstruction missing"
        );
    }
}
