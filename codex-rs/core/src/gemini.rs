use std::collections::HashMap;
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
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TokenUsage;
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
    S: Stream<Item = Result<Bytes>> + Unpin + Eventsource,
{
    let mut stream = stream.eventsource();
    let mut assistant_item: Option<ResponseItem> = None;
    let mut reasoning_item: Option<ResponseItem> = None;
    let mut token_usage: Option<TokenUsage> = None;

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
                if let Some(item) = reasoning_item.take() {
                    let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await;
                }
                let _ = tx_event
                    .send(Ok(ResponseEvent::Completed {
                        response_id: String::new(),
                        token_usage: token_usage.clone(),
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
            if let Some(item) = reasoning_item.take() {
                let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await;
            }
            let _ = tx_event
                .send(Ok(ResponseEvent::Completed {
                    response_id: String::new(),
                    token_usage: token_usage.clone(),
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
        if let Some(usage) = extract_token_usage(&payload) {
            token_usage = Some(usage);
        }
        let parsed = parse_gemini_response(&payload);
        let ParsedGeminiResponse {
            text,
            function_calls,
            reasoning,
        } = parsed;

        if let Some(reasoning) = reasoning
            && !append_reasoning_text(&tx_event, &mut reasoning_item, reasoning).await
        {
            return;
        }

        if let Some(text) = text
            && !append_assistant_text(&tx_event, &mut assistant_item, text).await
        {
            return;
        }

        if !function_calls.is_empty() {
            if let Some(item) = assistant_item.take()
                && tx_event
                    .send(Ok(ResponseEvent::OutputItemDone(item)))
                    .await
                    .is_err()
            {
                return;
            }

            if let Some(item) = reasoning_item.take()
                && tx_event
                    .send(Ok(ResponseEvent::OutputItemDone(item)))
                    .await
                    .is_err()
            {
                return;
            }

            for (idx, fc) in function_calls.into_iter().enumerate() {
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
                    token_usage: token_usage.clone(),
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
    reasoning: Option<String>,
}

fn extract_token_usage(root: &Value) -> Option<TokenUsage> {
    let usage = root
        .get("usageMetadata")
        .or_else(|| root.get("result").and_then(|v| v.get("usageMetadata")))?;

    let input_tokens = usage
        .get("promptTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("candidatesTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let total_tokens = usage
        .get("totalTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(input_tokens + output_tokens);

    Some(TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens,
        ..Default::default()
    })
}

fn safety_settings_allow_tools() -> Vec<Value> {
    vec![
        json!({
            "category": "HARM_CATEGORY_DANGEROUS_CONTENT",
            "threshold": "BLOCK_NONE",
        }),
        json!({
            "category": "HARM_CATEGORY_HARASSMENT",
            "threshold": "BLOCK_NONE",
        }),
        json!({
            "category": "HARM_CATEGORY_HATE_SPEECH",
            "threshold": "BLOCK_NONE",
        }),
        json!({
            "category": "HARM_CATEGORY_SEXUALLY_EXPLICIT",
            "threshold": "BLOCK_NONE",
        }),
        json!({
            "category": "HARM_CATEGORY_CIVIC_INTEGRITY",
            "threshold": "BLOCK_NONE",
        }),
    ]
}

fn extract_text_value(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }

    if let Some(text) = value.get("text").and_then(Value::as_str) {
        return Some(text.to_string());
    }

    if value.is_null() {
        return None;
    }

    serde_json::to_string(value).ok()
}

fn parse_gemini_response(root: &Value) -> ParsedGeminiResponse {
    let mut text: Option<String> = None;
    let mut function_calls: Vec<GeminiFunctionCall> = Vec::new();
    let mut reasoning: Option<String> = None;

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

            if let Some(thought_val) = part.get("thought")
                && let Some(thought_text) = extract_text_value(thought_val)
            {
                let entry = reasoning.get_or_insert_with(String::new);
                entry.push_str(&thought_text);
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
        reasoning,
    }
}

/// Translate Codex `ResponseItem` history into Gemini `contents`. This is a
/// best‑effort mapping that:
///   * preserves the user/assistant roles where possible
///   * encodes text as `parts[{text: ...}]`
///   * returns tool outputs as Gemini `functionResponse` parts
fn build_gemini_contents(items: &[ResponseItem]) -> Vec<Value> {
    let mut contents: Vec<Value> = Vec::new();
    let mut current_role: Option<String> = None;
    let mut current_parts: Vec<Value> = Vec::new();
    let mut function_call_names: HashMap<String, String> = HashMap::new();
    let mut function_call_signatures: HashMap<String, String> = HashMap::new();

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
            ResponseItem::FunctionCall {
                name,
                call_id,
                arguments,
                ..
            } => {
                function_call_names.insert(call_id.clone(), name.clone());
                let thought_signature = format!("codex_thought_sig_{call_id}");
                function_call_signatures.insert(call_id.clone(), thought_signature.clone());

                if current_role.as_deref() != Some("model") {
                    flush(&mut current_role, &mut current_parts, &mut contents);
                    current_role = Some("model".to_string());
                }

                let args_json = serde_json::from_str(arguments).unwrap_or_else(|e| {
                    debug!("Failed to parse function call args for {name}: {e}");
                    Value::Null
                });

                current_parts.push(json!({
                    "functionCall": {
                        "name": name,
                        "args": args_json,
                    },
                    "thought_signature": thought_signature,
                }));
            }
            ResponseItem::FunctionCallOutput { call_id, output } => {
                let function_name = function_call_names
                    .get(call_id)
                    .cloned()
                    .unwrap_or_else(|| call_id.clone());
                let thought_signature = function_call_signatures.get(call_id).cloned();

                if current_role.as_deref() != Some("user") {
                    flush(&mut current_role, &mut current_parts, &mut contents);
                    current_role = Some("user".to_string());
                }

                let mut response_content: Vec<Value> = Vec::new();
                if let Some(items) = &output.content_items {
                    for item in items {
                        match item {
                            FunctionCallOutputContentItem::InputText { text } => {
                                response_content.push(json!({ "text": text }));
                            }
                            FunctionCallOutputContentItem::InputImage { image_url } => {
                                response_content.push(json!({
                                    "text": format!("Image: {image_url}"),
                                }));
                            }
                        }
                    }
                }

                if response_content.is_empty() {
                    response_content.push(json!({ "text": output.content }));
                }

                let mut function_response = json!({
                    "functionResponse": {
                        "name": function_name,
                        "response": {
                            "name": function_name,
                            "content": response_content,
                        },
                    },
                });

                if let Some(signature) = thought_signature {
                    function_response["thought_signature"] = json!(signature);
                }

                current_parts.push(function_response);
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
// Adapter-focused Gemini tests live in `core/tests/gemini_*.rs` to keep the
// production code path focused. Live smoke tests that hit the real Gemini API
// sit under `live_tests` below.

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

    payload["safetySettings"] = Value::Array(safety_settings_allow_tools());

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

                let token_usage = extract_token_usage(&body_val);
                let parsed = parse_gemini_response(&body_val);
                let ParsedGeminiResponse {
                    text,
                    function_calls,
                    reasoning,
                } = parsed;
                let mut reasoning_item: Option<ResponseItem> = None;

                if let Some(reasoning_text) = reasoning
                    && !append_reasoning_text(&tx_event, &mut reasoning_item, reasoning_text).await
                {
                    return;
                }

                if let Some(text) = text
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

                for (idx, fc) in function_calls.into_iter().enumerate() {
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

                if let Some(item) = reasoning_item.take()
                    && tx_event
                        .send(Ok(ResponseEvent::OutputItemDone(item)))
                        .await
                        .is_err()
                {
                    return;
                }

                let _ = tx_event
                    .send(Ok(ResponseEvent::Completed {
                        response_id: String::new(),
                        token_usage: token_usage.clone(),
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

#[cfg(test)]
mod live_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use anyhow::Context;
    use anyhow::Result;
    use anyhow::anyhow;
    use codex_app_server_protocol::AuthMode;
    use codex_otel::otel_event_manager::OtelEventManager;
    use codex_protocol::ConversationId;
    use codex_protocol::protocol::SessionSource;
    use core_test_support::skip_if_no_network;
    use futures::StreamExt;
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use tempfile::TempDir;

    use crate::ContentItem;
    use crate::ModelClient;
    use crate::Prompt;
    use crate::ResponseEvent;
    use crate::ResponseItem;
    use crate::client_common::tools::ResponsesApiTool;
    use crate::client_common::tools::ToolSpec;
    use crate::config::Config;
    use crate::config::ConfigOverrides;
    use crate::config::ConfigToml;
    use crate::gemini_models::create_gemini_provider_for_model;
    use crate::model_family::derive_default_model_family;
    use crate::model_family::find_family_for_model;
    use crate::tools::spec::JsonSchema;

    const RUN_LIVE_ENV: &str = "RUN_LIVE_GEMINI";
    const API_KEY_ENV: &str = "GEMINI_API_KEY";
    const DEFAULT_MODEL: &str = "models/gemini-2.5-flash";

    struct LiveClient {
        client: ModelClient,
        _config: Arc<Config>,
    }

    /// Build a Config rooted in a temp Codex home so live calls do not touch
    /// the user's real state.
    fn load_live_config(codex_home: &TempDir) -> Option<Config> {
        Config::load_from_base_config_with_overrides(
            ConfigToml::default(),
            ConfigOverrides::default(),
            codex_home.path().to_path_buf(),
        )
        .ok()
    }

    /// Gate live calls so they only run when explicitly opted in and
    /// credentials are present.
    fn should_run_live() -> Option<String> {
        if std::env::var(RUN_LIVE_ENV).unwrap_or_default() != "1" {
            eprintln!("skipping live Gemini tests – set {RUN_LIVE_ENV}=1 to enable");
            return None;
        }

        if std::env::var(API_KEY_ENV).is_err() {
            eprintln!("skipping live Gemini tests – {API_KEY_ENV} is not set");
            return None;
        }

        let model = std::env::var("GEMINI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        Some(model)
    }

    /// Build a ModelClient pointed at the public Gemini endpoint using the
    /// provided model slug and the GEMINI_API_KEY header.
    async fn build_live_client(model: &str) -> Option<LiveClient> {
        let provider =
            match create_gemini_provider_for_model(model, "gemini-live", API_KEY_ENV, false) {
                Some(p) => p,
                None => {
                    eprintln!("skipping live Gemini tests – unsupported model slug {model}");
                    return None;
                }
            };

        let codex_home = TempDir::new().ok()?;
        let mut config = load_live_config(&codex_home)?;
        config.model = model.to_string();
        config.model_family = find_family_for_model(&config.model)
            .unwrap_or_else(|| derive_default_model_family(&config.model));
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
            Some("live@test.com".to_string()),
            Some(AuthMode::ChatGPT),
            false,
            "live".to_string(),
        );

        let client = ModelClient::new(
            Arc::clone(&config),
            None,
            otel_event_manager,
            provider,
            effort,
            summary,
            conversation_id,
            SessionSource::Exec,
        );

        Some(LiveClient {
            client,
            _config: config,
        })
    }

    /// Create a Prompt with a single user message and the supplied tool set.
    fn prompt_with_tools(tools: &[ToolSpec], user_prompt: &str) -> Prompt {
        let mut prompt = Prompt::default();
        prompt.input.push(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: user_prompt.to_string(),
            }],
        });
        prompt.tools = tools.to_vec();
        prompt
    }

    /// Common catalog of tools exposed to the live prompt matrix.
    fn tool_catalog() -> Vec<ToolSpec> {
        vec![
            weather_tool(),
            stock_quote_tool(),
            create_order_tool(),
            set_timer_tool(),
            append_file_tool(),
            shell_tool(),
            crate::tools::handlers::PLAN_TOOL.clone(),
        ]
    }

    /// Tool spec for basic weather lookup with explicit city + unit fields.
    fn weather_tool() -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "city".to_string(),
            JsonSchema::String {
                description: Some("City to fetch weather for.".to_string()),
            },
        );
        properties.insert(
            "unit".to_string(),
            JsonSchema::String {
                description: Some("Temperature unit, e.g. celsius.".to_string()),
            },
        );

        ToolSpec::Function(ResponsesApiTool {
            name: "get_weather".to_string(),
            description: "Look up the current weather for a city.".to_string(),
            strict: false,
            parameters: JsonSchema::Object {
                properties,
                required: Some(vec!["city".to_string(), "unit".to_string()]),
                additional_properties: None,
            },
        })
    }

    /// Tool spec used to test tool-choice disambiguation for stock lookups.
    fn stock_quote_tool() -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "symbol".to_string(),
            JsonSchema::String {
                description: Some("Ticker symbol to look up, e.g. AAPL.".to_string()),
            },
        );

        ToolSpec::Function(ResponsesApiTool {
            name: "get_stock_quote".to_string(),
            description: "Fetch a real-time stock quote.".to_string(),
            strict: false,
            parameters: JsonSchema::Object {
                properties,
                required: Some(vec!["symbol".to_string()]),
                additional_properties: None,
            },
        })
    }

    /// Tool spec for timers with a numeric duration and optional label.
    fn set_timer_tool() -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "minutes".to_string(),
            JsonSchema::Number {
                description: Some("How long the timer should run, in minutes.".to_string()),
            },
        );
        properties.insert(
            "label".to_string(),
            JsonSchema::String {
                description: Some("Optional label to show with the timer.".to_string()),
            },
        );

        ToolSpec::Function(ResponsesApiTool {
            name: "set_timer".to_string(),
            description: "Start a countdown timer.".to_string(),
            strict: false,
            parameters: JsonSchema::Object {
                properties,
                required: Some(vec!["minutes".to_string()]),
                additional_properties: None,
            },
        })
    }

    /// Tool spec for a nested order payload covering arrays and objects.
    fn create_order_tool() -> ToolSpec {
        let mut item_props = BTreeMap::new();
        item_props.insert(
            "sku".to_string(),
            JsonSchema::String {
                description: Some("Item SKU.".to_string()),
            },
        );
        item_props.insert(
            "quantity".to_string(),
            JsonSchema::Number {
                description: Some("Quantity of the item.".to_string()),
            },
        );

        let item_schema = JsonSchema::Object {
            properties: item_props,
            required: Some(vec!["sku".to_string(), "quantity".to_string()]),
            additional_properties: None,
        };

        let mut shipping_props = BTreeMap::new();
        shipping_props.insert(
            "name".to_string(),
            JsonSchema::String {
                description: Some("Recipient name.".to_string()),
            },
        );
        shipping_props.insert(
            "line1".to_string(),
            JsonSchema::String {
                description: Some("Street line 1.".to_string()),
            },
        );
        shipping_props.insert(
            "city".to_string(),
            JsonSchema::String {
                description: Some("City.".to_string()),
            },
        );
        shipping_props.insert(
            "state".to_string(),
            JsonSchema::String {
                description: Some("State or province.".to_string()),
            },
        );
        shipping_props.insert(
            "postal_code".to_string(),
            JsonSchema::String {
                description: Some("Postal or ZIP code.".to_string()),
            },
        );

        let shipping_schema = JsonSchema::Object {
            properties: shipping_props,
            required: Some(vec![
                "name".to_string(),
                "line1".to_string(),
                "city".to_string(),
                "state".to_string(),
                "postal_code".to_string(),
            ]),
            additional_properties: None,
        };

        let mut order_props = BTreeMap::new();
        order_props.insert(
            "items".to_string(),
            JsonSchema::Array {
                items: Box::new(item_schema),
                description: Some("Order line items.".to_string()),
            },
        );
        order_props.insert("shipping".to_string(), shipping_schema);
        order_props.insert(
            "notes".to_string(),
            JsonSchema::String {
                description: Some("Optional delivery notes.".to_string()),
            },
        );

        ToolSpec::Function(ResponsesApiTool {
            name: "create_order".to_string(),
            description: "Create a mock order with items and shipping details.".to_string(),
            strict: false,
            parameters: JsonSchema::Object {
                properties: order_props,
                required: Some(vec!["items".to_string(), "shipping".to_string()]),
                additional_properties: None,
            },
        })
    }

    /// Tool spec for appending text to a file path.
    fn append_file_tool() -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "path".to_string(),
            JsonSchema::String {
                description: Some("Path to write.".to_string()),
            },
        );
        properties.insert(
            "content".to_string(),
            JsonSchema::String {
                description: Some("Content to append.".to_string()),
            },
        );
        properties.insert(
            "mode".to_string(),
            JsonSchema::String {
                description: Some("Optional write mode, e.g. append.".to_string()),
            },
        );

        ToolSpec::Function(ResponsesApiTool {
            name: "append_file".to_string(),
            description: "Append text to a file path.".to_string(),
            strict: false,
            parameters: JsonSchema::Object {
                properties,
                required: Some(vec!["path".to_string(), "content".to_string()]),
                additional_properties: None,
            },
        })
    }

    /// Tool spec for issuing shell commands.
    fn shell_tool() -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "command".to_string(),
            JsonSchema::String {
                description: Some("Command to execute.".to_string()),
            },
        );
        properties.insert(
            "timeout_ms".to_string(),
            JsonSchema::Number {
                description: Some("Optional timeout in ms.".to_string()),
            },
        );

        ToolSpec::Function(ResponsesApiTool {
            name: "shell".to_string(),
            description: "Run a shell command.".to_string(),
            strict: false,
            parameters: JsonSchema::Object {
                properties,
                required: Some(vec!["command".to_string()]),
                additional_properties: None,
            },
        })
    }

    /// Run a prompt against Gemini and collect the full event stream.
    async fn execute_prompt(client: &ModelClient, prompt: Prompt) -> Result<Vec<ResponseEvent>> {
        let mut stream = client.stream(&prompt).await?;
        let mut events = Vec::new();

        while let Some(event) = stream.next().await {
            events.push(event?);
        }

        Ok(events)
    }

    /// Pick the first function call from the stream and parse its JSON args.
    fn extract_function_call(events: &[ResponseEvent]) -> Result<(String, Value)> {
        let (name, args) = events
            .iter()
            .find_map(|event| match event {
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                    name,
                    arguments,
                    ..
                }) => Some((name.clone(), arguments.clone())),
                _ => None,
            })
            .ok_or_else(|| anyhow!("response did not contain a function call"))?;

        let parsed: Value = serde_json::from_str(&args)?;
        Ok((name, parsed))
    }

    /// Coerce JSON numeric values into i64 for easier equality checks.
    fn round_to_i64(value: &Value) -> Option<i64> {
        value
            .as_i64()
            .or_else(|| value.as_f64().map(|v| v.round() as i64))
    }

    /// Single prompt + expectation for the live Gemini matrix.
    struct LiveCase {
        name: &'static str,
        prompt: String,
        expected_tool: &'static str,
        assert_args: Box<dyn Fn(&Value) + Send + Sync>,
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn live_gemini_function_calls() -> Result<()> {
        skip_if_no_network!(Ok(()));
        let Some(model) = should_run_live() else {
            return Ok(());
        };
        let Some(LiveClient { client, .. }) = build_live_client(&model).await else {
            return Ok(());
        };

        let tools = tool_catalog();
        let cases = vec![
            LiveCase {
                name: "weather_paris",
                prompt: "Use the get_weather(city, unit) function to fetch the weather for Paris in celsius. Respond only with a function call."
                    .to_string(),
                expected_tool: "get_weather",
                assert_args: Box::new(|args| {
                    let city = args
                        .get("city")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_lowercase();
                    assert_eq!(city, "paris");

                    let unit = args
                        .get("unit")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let normalized = unit.to_lowercase();
                    assert!(
                        normalized.contains('c'),
                        "expected Celsius unit, got {unit:?}"
                    );
                }),
            },
            LiveCase {
                name: "stock_disambiguation",
                prompt: "Choose the best tool to return the stock quote for AAPL. Tools available: get_weather(city, unit) and get_stock_quote(symbol). Return only the tool call."
                    .to_string(),
                expected_tool: "get_stock_quote",
                assert_args: Box::new(|args| {
                    let symbol = args
                        .get("symbol")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_uppercase();
                    assert_eq!(symbol, "AAPL");
                }),
            },
            LiveCase {
                name: "create_order_structured",
                prompt: "Place an order via create_order(items, shipping, notes). Add 2 units of sku abc-123 and 1 unit of sku orange-456. Ship to Grace Hopper at 123 Harbor Road, Oakland, CA 94607. Delivery notes should say 'leave at front desk'. Respond only with a function call."
                    .to_string(),
                expected_tool: "create_order",
                assert_args: Box::new(|args| {
                    let empty_items = Vec::new();
                    let items = args
                        .get("items")
                        .and_then(Value::as_array)
                        .unwrap_or(&empty_items);
                    assert_eq!(items.len(), 2, "expected two items, got {items:?}");

                    let first = items.first().unwrap_or(&Value::Null);
                    assert_eq!(
                        first.get("sku").and_then(Value::as_str),
                        Some("abc-123")
                    );
                    let first_qty = first.get("quantity").and_then(round_to_i64);
                    assert_eq!(first_qty, Some(2));

                    let second = items.get(1).unwrap_or(&Value::Null);
                    assert_eq!(
                        second.get("sku").and_then(Value::as_str),
                        Some("orange-456")
                    );
                    let second_qty = second.get("quantity").and_then(round_to_i64);
                    assert_eq!(second_qty, Some(1));

                    let empty_map = serde_json::Map::new();
                    let shipping = args
                        .get("shipping")
                        .and_then(Value::as_object)
                        .unwrap_or(&empty_map);
                    assert_eq!(shipping.get("city").and_then(Value::as_str), Some("Oakland"));
                    assert_eq!(shipping.get("state").and_then(Value::as_str), Some("CA"));
                    assert_eq!(
                        shipping.get("postal_code").and_then(Value::as_str),
                        Some("94607")
                    );
                    assert_eq!(
                        shipping.get("name").and_then(Value::as_str),
                        Some("Grace Hopper")
                    );

                    if let Some(notes) = args.get("notes").and_then(Value::as_str) {
                        assert!(
                            notes.to_lowercase().contains("front"),
                            "expected notes to mention delivery instructions"
                        );
                    }
                }),
            },
            LiveCase {
                name: "timer_normalization",
                prompt: "Use the set_timer(minutes, label) function to start a timer for \"ten minuttes\" while I steep green tea. Normalize the duration to a numeric value in minutes and respond only with a function call."
                    .to_string(),
                expected_tool: "set_timer",
                assert_args: Box::new(|args| {
                    let minutes_value = args.get("minutes").unwrap_or(&Value::Null);
                    let minutes = round_to_i64(minutes_value).unwrap_or_default();
                    assert_eq!(minutes, 10);

                    if let Some(label) = args.get("label").and_then(Value::as_str) {
                        assert!(
                            label.to_lowercase().contains("tea"),
                            "expected label to mention tea"
                        );
                    }
                }),
            },
            LiveCase {
                name: "append_file",
                prompt: "Call append_file(path, content, mode) to append 'hello from live' to /tmp/gemini-live.txt with mode \"append\". Respond only with a function call."
                    .to_string(),
                expected_tool: "append_file",
                assert_args: Box::new(|args| {
                    assert_eq!(
                        args.get("path").and_then(Value::as_str),
                        Some("/tmp/gemini-live.txt")
                    );
                    assert_eq!(
                        args.get("content").and_then(Value::as_str),
                        Some("hello from live")
                    );
                    assert_eq!(
                        args.get("mode").and_then(Value::as_str),
                        Some("append")
                    );
                }),
            },
            LiveCase {
                name: "shell_ls",
                prompt: "Call shell(command, timeout_ms) to list /tmp with a 2000ms timeout. Respond only with a function call."
                    .to_string(),
                expected_tool: "shell",
                assert_args: Box::new(|args| {
                    assert_eq!(args.get("command").and_then(Value::as_str), Some("ls /tmp"));
                    let timeout = round_to_i64(args.get("timeout_ms").unwrap_or(&Value::Null)).unwrap_or(0);
                    assert!(
                        (1_000..=10_000).contains(&timeout),
                        "timeout should be reasonable, got {timeout}"
                    );
                }),
            },
            LiveCase {
                name: "plan_create",
                prompt: "Call update_plan(plan) to create a 2-step plan: step1='Collect files', step2='Summarize changes'. Mark step1 in_progress and step2 pending. Respond only with the function call."
                    .to_string(),
                expected_tool: "update_plan",
                assert_args: Box::new(|args| {
                    let steps = args
                        .get("plan")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    assert_eq!(steps.len(), 2, "expected 2 plan items");
                    let statuses: Vec<_> = steps
                        .iter()
                        .map(|s| s.get("status").and_then(Value::as_str).unwrap_or_default())
                        .collect();
                    assert_eq!(statuses[0], "in_progress");
                    assert_eq!(statuses[1], "pending");
                    let step_texts: Vec<_> = steps
                        .iter()
                        .map(|s| s.get("step").and_then(Value::as_str).unwrap_or_default())
                        .collect();
                    assert!(
                        step_texts[0].to_lowercase().contains("collect"),
                        "step 1 should mention collect"
                    );
                    assert!(
                        step_texts[1].to_lowercase().contains("summarize"),
                        "step 2 should mention summarize"
                    );
                }),
            },
            LiveCase {
                name: "plan_update",
                prompt: "Update the plan via update_plan: mark step1 completed, step2 in_progress, and add a pending step3 'Ship changes'. Respond only with the function call."
                    .to_string(),
                expected_tool: "update_plan",
                assert_args: Box::new(|args| {
                    let steps = args
                        .get("plan")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    assert_eq!(steps.len(), 3, "expected 3 plan items");
                    let statuses: Vec<_> = steps
                        .iter()
                        .map(|s| s.get("status").and_then(Value::as_str).unwrap_or_default())
                        .collect();
                    assert_eq!(statuses[0], "completed");
                    assert_eq!(statuses[1], "in_progress");
                    assert_eq!(statuses[2], "pending");
                    let step3 = steps
                        .get(2)
                        .and_then(|s| s.get("step").and_then(Value::as_str))
                        .unwrap_or_default()
                        .to_lowercase();
                    assert!(
                        step3.contains("ship"),
                        "step 3 should mention ship changes, got {step3}"
                    );
                }),
            },
        ];

        for case in cases {
            let prompt = prompt_with_tools(&tools, &case.prompt);
            println!(
                "live_gemini sending case={} model={} prompt={}",
                case.name, model, case.prompt
            );
            let events = execute_prompt(&client, prompt)
                .await
                .with_context(|| format!("case {} failed to stream from Gemini", case.name))?;
            assert!(
                events
                    .iter()
                    .any(|ev| matches!(ev, ResponseEvent::Completed { .. })),
                "case {} did not finish the stream",
                case.name
            );

            let (tool_name, args) =
                extract_function_call(&events).with_context(|| format!("case {}", case.name))?;
            assert_eq!(
                tool_name, case.expected_tool,
                "case {} should call the expected tool",
                case.name
            );
            (case.assert_args)(&args);
            println!(
                "live_gemini case={} tool={} args={}",
                case.name,
                tool_name,
                serde_json::to_string_pretty(&args).unwrap_or_else(|_| "<unprintable>".to_string())
            );
        }

        Ok(())
    }
}

async fn append_reasoning_text(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    reasoning_item: &mut Option<ResponseItem>,
    text: String,
) -> bool {
    if text.is_empty() {
        return true;
    }

    if reasoning_item.is_none() {
        let item = ResponseItem::Reasoning {
            id: "gemini_reasoning_1".to_string(),
            summary: vec![ReasoningItemReasoningSummary::SummaryText {
                text: String::new(),
            }],
            content: Some(Vec::new()),
            encrypted_content: None,
        };
        if tx_event
            .send(Ok(ResponseEvent::OutputItemAdded(item.clone())))
            .await
            .is_err()
        {
            return false;
        }
        *reasoning_item = Some(item);
    }

    if let Some(ResponseItem::Reasoning {
        summary, content, ..
    }) = reasoning_item
    {
        if let Some(ReasoningItemReasoningSummary::SummaryText { text: summary_text }) =
            summary.first_mut()
        {
            summary_text.push_str(&text);
            if tx_event
                .send(Ok(ResponseEvent::ReasoningSummaryDelta {
                    delta: text.clone(),
                    summary_index: 0,
                }))
                .await
                .is_err()
            {
                return false;
            }
        }

        if let Some(content_items) = content {
            content_items.push(ReasoningItemContent::ReasoningText { text: text.clone() });
            let content_index = content_items.len() as i64 - 1;
            if tx_event
                .send(Ok(ResponseEvent::ReasoningContentDelta {
                    delta: text,
                    content_index,
                }))
                .await
                .is_err()
            {
                return false;
            }
        }
    }

    true
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
    fn parse_gemini_response_extracts_thought_parts() {
        let json = json!({
            "candidates": [
                {
                    "content": {
                        "parts": [
                            { "thought": "Consider inputs. " },
                            { "text": "Finalize." }
                        ]
                    }
                }
            ]
        });

        let parsed = parse_gemini_response(&json);
        assert_eq!(
            parsed.reasoning.as_deref(),
            Some("Consider inputs. "),
            "expected reasoning to include thought text"
        );
        assert_eq!(
            parsed.text.as_deref(),
            Some("Finalize."),
            "expected text to include non-thinking content"
        );
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
        items.push(ResponseItem::FunctionCall {
            id: None,
            name: "write_file".to_string(),
            arguments: "{}".to_string(),
            call_id: "call-1".to_string(),
        });
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

        let function_entry = contents
            .iter()
            .find(|c| c["role"] == json!("function"))
            .expect("expected a function role content entry");
        let function_response = function_entry["parts"][0]["functionResponse"]
            .as_object()
            .expect("functionResponse part should exist");
        assert_eq!(
            function_response.get("name"),
            Some(&json!("write_file")),
            "functionResponse must carry the tool name"
        );
        let response_content = function_response
            .get("response")
            .and_then(|v| v.get("content"))
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            response_content.contains("result-1"),
            "expected function output content to be present"
        );

        let has_custom_output = contents.iter().any(|c| {
            c["parts"].as_array().is_some_and(|parts| {
                parts.iter().any(|p| {
                    p.get("text")
                        .and_then(Value::as_str)
                        .map(|t| t.contains("Custom tool call-2 result:\nresult-2"))
                        .unwrap_or(false)
                })
            })
        });
        assert!(
            has_custom_output,
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
        let safety = payload
            .get("safetySettings")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(
            !safety.is_empty(),
            "safetySettings should be present to relax harmful content filters"
        );
    }

    #[test]
    fn extract_token_usage_reads_usage_metadata() {
        let json = json!({
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 4,
                "totalTokenCount": 14
            }
        });

        let usage = extract_token_usage(&json).expect("usage metadata");
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 4);
        assert_eq!(usage.total_tokens, 14);
    }
}
