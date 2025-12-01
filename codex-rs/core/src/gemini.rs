use std::collections::HashMap;
use std::sync::Arc;

use crate::auth::AuthManager;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use crate::client_common::tools::ToolSpec;
use crate::default_client::CodexHttpClient;
use crate::error::CodexErr;
use crate::error::ConnectionFailedError;
use crate::error::ResponseStreamFailed;
use crate::error::Result;
use crate::error::UnexpectedResponseError;
use crate::model_family::ModelFamily;
use crate::model_provider_info::ModelProviderInfo;
use crate::tools::spec::create_tools_json_for_gemini_api;
use codex_otel::otel_event_manager::OtelEventManager;
use codex_protocol::config_types::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TokenUsage;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::Response;
use reqwest::header::ACCEPT;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderName;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::debug;
use uuid::Uuid;

use crate::gemini_models::Blob;
use crate::gemini_models::Content;
use crate::gemini_models::ErrorEnvelope;
use crate::gemini_models::FunctionCall;
use crate::gemini_models::FunctionResponse;
use crate::gemini_models::GenerateContentRequest;
use crate::gemini_models::GenerationConfig;
use crate::gemini_models::Part;
use crate::gemini_models::ThinkingConfig;
use crate::gemini_models::Tool;
use crate::gemini_models::UsageMetadata;

fn provider_requests_sse(provider: &ModelProviderInfo) -> bool {
    provider
        .base_url
        .as_ref()
        .is_some_and(|url| url.contains("streamGenerateContent"))
        || provider.query_params.as_ref().is_some_and(|params| {
            params
                .get("alt")
                .map(|value| value.eq_ignore_ascii_case("sse"))
                .unwrap_or(false)
        })
}

pub async fn stream_gemini(
    prompt: &Prompt,
    model_family: &ModelFamily,
    reasoning_effort: Option<ReasoningEffortConfig>,
    client: &CodexHttpClient,
    provider: &ModelProviderInfo,
    otel_event_manager: &OtelEventManager,
    session_source: &SessionSource,
    auth_manager: &Option<Arc<AuthManager>>,
) -> Result<ResponseStream> {
    let _ = session_source;

    let auth = auth_manager.as_ref().and_then(|manager| manager.auth());
    let request = build_gemini_request(prompt, model_family, reasoning_effort)?;

    let mut request_builder = provider.create_request_builder(client, &auth).await?;
    if provider_requests_sse(provider) {
        request_builder = request_builder.header(ACCEPT, "text/event-stream");
    }
    let response = request_builder
        .json(&request)
        .send()
        .await
        .map_err(|err| CodexErr::ConnectionFailed(ConnectionFailedError { source: err }))?;

    let request_id = extract_request_id(response.headers());
    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "failed to read response body".to_string());
        return Err(CodexErr::UnexpectedStatus(UnexpectedResponseError {
            status,
            body,
            request_id,
        }));
    }

    let mut is_sse = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/event-stream"));
    if !is_sse && provider_requests_sse(provider) {
        is_sse = true;
    }

    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent>>(1600);
    let manager = otel_event_manager.clone();

    tokio::spawn(async move {
        if tx_event.send(Ok(ResponseEvent::Created)).await.is_err() {
            return;
        }

        let outcome = if is_sse {
            process_sse_response(response, tx_event.clone(), manager.clone()).await
        } else {
            process_json_response(response, tx_event.clone(), manager.clone()).await
        };

        if let Err(err) = outcome {
            manager.see_event_completed_failed(&err);
            let _ = tx_event.send(Err(err)).await;
        }
    });

    Ok(ResponseStream { rx_event })
}

fn build_gemini_request(
    prompt: &Prompt,
    model_family: &ModelFamily,
    reasoning_effort: Option<ReasoningEffortConfig>,
) -> Result<GenerateContentRequest> {
    let instructions = prompt.get_full_instructions(model_family).into_owned();
    let formatted_input = prompt.get_formatted_input();
    let contents = map_history_to_contents(&formatted_input);
    let tools = build_tool_declarations(&prompt.tools)?;
    let generation_config =
        build_generation_config(reasoning_effort, prompt.output_schema.as_ref());

    let system_instruction = (!instructions.trim().is_empty()).then(|| Content {
        role: "user".to_string(),
        parts: vec![Part {
            text: Some(instructions),
            inline_data: None,
            media_resolution: None,
            function_call: None,
            function_response: None,
            thought_signature: None,
            thought: None,
        }],
    });

    Ok(GenerateContentRequest {
        contents,
        tools,
        tool_config: None,
        // Gemini generateContent expects cachedContent to be a resource name string
        // (cachedContents/{id}); we do not create cache entries here.
        cached_content: None,
        generation_config,
        system_instruction,
    })
}

fn build_generation_config(
    reasoning_effort: Option<ReasoningEffortConfig>,
    output_schema: Option<&serde_json::Value>,
) -> Option<GenerationConfig> {
    let thinking_level = reasoning_effort.and_then(reasoning_effort_to_thinking_level);

    let (response_mime_type, response_json_schema) = if let Some(schema) = output_schema {
        (Some("application/json".to_string()), Some(schema.clone()))
    } else {
        (None, None)
    };

    let thinking_config = thinking_level.map(|level| ThinkingConfig {
        thinking_level: Some(level),
        thinking_budget: None,
        include_thoughts: None,
    });

    if thinking_config.is_none() && response_mime_type.is_none() && response_json_schema.is_none() {
        return None;
    }

    Some(GenerationConfig {
        max_output_tokens: None,
        temperature: None,
        top_p: None,
        top_k: None,
        thinking_config,
        response_mime_type,
        response_json_schema,
        image_config: None,
    })
}

fn build_tool_declarations(tools: &[ToolSpec]) -> Result<Option<Vec<Tool>>> {
    let json_tools = create_tools_json_for_gemini_api(tools)?;
    if json_tools.is_empty() {
        return Ok(None);
    }

    let mut converted = Vec::new();
    for tool in json_tools {
        match serde_json::from_value::<Tool>(tool) {
            Ok(tool) => converted.push(tool),
            Err(err) => {
                debug!(?err, "Skipping Gemini tool without function_declarations");
            }
        }
    }
    if converted.is_empty() {
        Ok(None)
    } else {
        Ok(Some(converted))
    }
}

fn map_history_to_contents(items: &[ResponseItem]) -> Vec<Content> {
    let mut contents: Vec<Content> = Vec::new();
    let mut function_names = HashMap::<String, String>::new();

    for item in items {
        let (role, new_parts) = match item {
            ResponseItem::Message {
                role,
                content,
                thought_signature,
                ..
            } => {
                let mut parts = content
                    .iter()
                    .flat_map(content_item_to_parts)
                    .collect::<Vec<_>>();
                if parts.is_empty() {
                    continue;
                }
                if let Some(sig) = thought_signature
                    && let Some(last) = parts.last_mut()
                {
                    last.thought_signature = Some(sig.clone());
                }
                (map_role(role), parts)
            }
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                thought_signature,
                ..
            } => {
                function_names.insert(call_id.clone(), name.clone());
                let args =
                    serde_json::from_str(arguments).unwrap_or(Value::String(arguments.clone()));
                let part = Part {
                    text: None,
                    inline_data: None,
                    media_resolution: None,
                    function_call: Some(FunctionCall {
                        name: name.clone(),
                        args,
                    }),
                    function_response: None,
                    thought_signature: thought_signature.clone(),
                    thought: None,
                };
                ("model".to_string(), vec![part])
            }
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } => {
                let name = function_names
                    .get(call_id)
                    .cloned()
                    .unwrap_or_else(|| call_id.clone());
                let part = Part {
                    text: None,
                    inline_data: None,
                    media_resolution: None,
                    function_call: None,
                    function_response: Some(FunctionResponse {
                        name,
                        response: build_function_response_payload(output),
                    }),
                    thought_signature: None,
                    thought: None,
                };
                ("user".to_string(), vec![part])
            }
            ResponseItem::CustomToolCallOutput { call_id, output } => {
                let part = Part {
                    text: Some(format!("Custom tool {call_id} result:\n{output}")),
                    inline_data: None,
                    media_resolution: None,
                    function_call: None,
                    function_response: None,
                    thought_signature: None,
                    thought: None,
                };
                ("user".to_string(), vec![part])
            }
            _ => continue,
        };

        if let Some(last) = contents.last_mut()
            && last.role == role
        {
            last.parts.extend(new_parts);
        } else {
            contents.push(Content {
                role,
                parts: new_parts,
            });
        }
    }

    contents
}

fn content_item_to_parts(item: &ContentItem) -> Vec<Part> {
    match item {
        ContentItem::InputText { text } | ContentItem::OutputText { text } => vec![Part {
            text: Some(text.clone()),
            inline_data: None,
            media_resolution: None,
            function_call: None,
            function_response: None,
            thought_signature: None,
            thought: None,
        }],
        ContentItem::InputImage { image_url } => {
            if let Some(blob) = inline_data_from_image_url(image_url) {
                vec![Part {
                    text: None,
                    inline_data: Some(blob),
                    media_resolution: None,
                    function_call: None,
                    function_response: None,
                    thought_signature: None,
                    thought: None,
                }]
            } else {
                vec![Part {
                    text: Some(format!("Image: {image_url}")),
                    inline_data: None,
                    media_resolution: None,
                    function_call: None,
                    function_response: None,
                    thought_signature: None,
                    thought: None,
                }]
            }
        }
    }
}

fn inline_data_from_image_url(url: &str) -> Option<Blob> {
    let stripped = url.strip_prefix("data:")?;
    let (mime, encoded) = stripped.split_once(";base64,")?;
    Some(Blob {
        mime_type: mime.to_string(),
        data: encoded.to_string(),
    })
}

fn build_function_response_payload(payload: &FunctionCallOutputPayload) -> Value {
    if let Some(items) = &payload.content_items {
        let content = items
            .iter()
            .map(|item| match item {
                FunctionCallOutputContentItem::InputText { text } => Value::Object(
                    [("text".to_string(), Value::String(text.clone()))]
                        .into_iter()
                        .collect(),
                ),
                FunctionCallOutputContentItem::InputImage { image_url } => {
                    if let Some(blob) = inline_data_from_image_url(image_url) {
                        Value::Object(
                            [(
                                "inlineData".to_string(),
                                serde_json::json!({
                                    "mimeType": blob.mime_type,
                                    "data": blob.data
                                }),
                            )]
                            .into_iter()
                            .collect(),
                        )
                    } else {
                        Value::Object(
                            [("text".to_string(), Value::String(image_url.clone()))]
                                .into_iter()
                                .collect(),
                        )
                    }
                }
            })
            .collect::<Vec<_>>();
        serde_json::json!({ "content": content })
    } else {
        serde_json::json!({
            "content": [ { "text": payload.content } ]
        })
    }
}

fn map_role(role: &str) -> String {
    match role {
        "assistant" => "model".to_string(),
        _ => role.to_string(),
    }
}

async fn process_json_response(
    response: Response,
    tx_event: mpsc::Sender<Result<ResponseEvent>>,
    otel_event_manager: OtelEventManager,
) -> Result<()> {
    let request_id = extract_request_id(response.headers());
    let body = response.bytes().await.map_err(|err| {
        CodexErr::ResponseStreamFailed(ResponseStreamFailed {
            source: err,
            request_id: request_id.clone(),
        })
    })?;
    let parsed: crate::gemini_models::GenerateContentResponse = serde_json::from_slice(&body)?;

    let mut text_state = GeminiTextState::default();
    let mut call_tracker = GeminiCallTracker::default();

    if let Some(candidates) = parsed.candidates
        && let Some(candidate) = candidates.into_iter().next()
        && let Some(content) = candidate.content
        && !process_parts(
            &content.parts,
            &tx_event,
            &mut text_state,
            &mut call_tracker,
        )
        .await?
    {
        return Ok(());
    }

    if !text_state.flush(&tx_event).await? {
        return Ok(());
    }

    let token_usage = parsed
        .usage_metadata
        .and_then(|meta| usage_from_metadata(&meta));
    if let Some(usage) = &token_usage {
        otel_event_manager.sse_event_completed(
            usage.input_tokens,
            usage.output_tokens,
            Some(usage.cached_input_tokens),
            Some(usage.reasoning_output_tokens),
            usage.total_tokens,
        );
    }

    let completed = ResponseEvent::Completed {
        response_id: Uuid::new_v4().to_string(),
        token_usage,
    };
    let _ = tx_event.send(Ok(completed)).await;

    Ok(())
}

async fn process_sse_response(
    response: Response,
    tx_event: mpsc::Sender<Result<ResponseEvent>>,
    otel_event_manager: OtelEventManager,
) -> Result<()> {
    let mut stream = response.bytes_stream().eventsource();
    let mut text_state = GeminiTextState::default();
    let mut call_tracker = GeminiCallTracker::default();
    let mut token_usage: Option<TokenUsage> = None;
    let mut saw_done = false;
    let mut received_payload = false;
    let mut saw_event = false;
    let mut last_error: Option<CodexErr> = None;

    while let Some(event) = stream.next().await {
        let event = match event {
            Ok(event) => event,
            Err(err) => {
                return Err(CodexErr::Stream(format!("Gemini SSE error: {err}"), None));
            }
        };

        let data = event.data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            saw_done = true;
            break;
        }

        let envelope = match serde_json::from_str::<GeminiStreamEnvelope>(data) {
            Ok(env) => env,
            Err(err) => {
                if let Ok(error_env) = serde_json::from_str::<ErrorEnvelope>(data)
                    && let Some(error) = error_env.error
                {
                    debug!(
                        code = error.code,
                        status = error.status,
                        message = error.message,
                        "Gemini stream error envelope"
                    );
                    let status = http::StatusCode::from_u16(error.code as u16)
                        .unwrap_or(http::StatusCode::BAD_REQUEST);
                    last_error = Some(CodexErr::UnexpectedStatus(UnexpectedResponseError {
                        status,
                        body: error.message,
                        request_id: None,
                    }));
                    return Err(last_error.unwrap());
                }
                debug!(%err, %data, "Failed to parse Gemini SSE chunk");
                continue;
            }
        };

        let result = match envelope {
            GeminiStreamEnvelope::Wrapped { result } => result,
            GeminiStreamEnvelope::Unwrapped(result) => result,
        };
        saw_event = true;

        if let Some(meta) = result.usage_metadata {
            token_usage = usage_from_metadata(&meta);
            received_payload = true;
        }
        if let Some(candidates) = result.candidates.as_ref() {
            for candidate in candidates {
                if let Some(content) = candidate.content.as_ref() {
                    received_payload = true;
                    if !process_parts(
                        &content.parts,
                        &tx_event,
                        &mut text_state,
                        &mut call_tracker,
                    )
                    .await?
                    {
                        return Ok(());
                    }
                }
            }
        }
    }

    if !saw_done && !received_payload {
        if let Some(err) = last_error {
            return Err(err);
        }
        if !saw_event {
            return Err(CodexErr::Stream(
                "Gemini stream closed before completion".to_string(),
                None,
            ));
        }
        debug!("Gemini stream ended without payload; completing without content");
    }

    if !text_state.flush(&tx_event).await? {
        return Ok(());
    }

    if let Some(usage) = &token_usage {
        otel_event_manager.sse_event_completed(
            usage.input_tokens,
            usage.output_tokens,
            Some(usage.cached_input_tokens),
            Some(usage.reasoning_output_tokens),
            usage.total_tokens,
        );
    }

    let completed = ResponseEvent::Completed {
        response_id: Uuid::new_v4().to_string(),
        token_usage,
    };
    let _ = tx_event.send(Ok(completed)).await;

    Ok(())
}

async fn process_parts(
    parts: &[Part],
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    text_state: &mut GeminiTextState,
    call_tracker: &mut GeminiCallTracker,
) -> Result<bool> {
    for part in parts {
        if let Some(text) = &part.text {
            text_state.note_thought_signature(part.thought_signature.clone());
            if !text_state.push_text(tx_event, text).await? {
                return Ok(false);
            }
        } else if part.thought_signature.is_some() {
            text_state.note_thought_signature(part.thought_signature.clone());
            if !text_state.ensure_started(tx_event).await? {
                return Ok(false);
            }
        }

        if let Some(call) = &part.function_call {
            if !text_state.flush(tx_event).await? {
                return Ok(false);
            }
            let call_id = call_tracker.next_call_id();
            if !emit_function_call(tx_event, call, &call_id, part.thought_signature.clone()).await?
            {
                return Ok(false);
            }
        }
    }

    Ok(true)
}

fn usage_from_metadata(meta: &UsageMetadata) -> Option<TokenUsage> {
    let input = meta.prompt_token_count?;
    let output = meta.candidates_token_count.unwrap_or(0);
    let total = meta.total_token_count.unwrap_or(input + output);
    // Reasoning output tokens logic:
    // If Gemini separates thought tokens in usage metadata, we should use it.
    // 'thoughts_token_count' is in metadata.
    let reasoning = meta.thoughts_token_count.unwrap_or(0);

    Some(TokenUsage {
        input_tokens: input,
        cached_input_tokens: 0,
        output_tokens: output,
        reasoning_output_tokens: reasoning,
        total_tokens: total,
    })
}

async fn emit_function_call(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    call: &FunctionCall,
    call_id: &str,
    thought_signature: Option<String>,
) -> Result<bool> {
    let arguments = serde_json::to_string(&call.args).unwrap_or_else(|_| "{}".to_string());
    let item = ResponseItem::FunctionCall {
        id: None,
        name: call.name.clone(),
        arguments,
        call_id: call_id.to_string(),
        thought_signature,
    };
    let sent = tx_event
        .send(Ok(ResponseEvent::OutputItemDone(item)))
        .await
        .is_ok();
    Ok(sent)
}

#[derive(Default)]
struct GeminiTextState {
    buffer: String,
    active: bool,
    thought_signature: Option<String>,
}

impl GeminiTextState {
    fn note_thought_signature(&mut self, thought_signature: Option<String>) {
        if let Some(sig) = thought_signature {
            self.thought_signature = Some(sig);
        }
    }

    async fn ensure_started(
        &mut self,
        tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    ) -> Result<bool> {
        if self.active {
            return Ok(true);
        }

        let item = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: String::new(),
            }],
            thought_signature: None,
        };
        if tx_event
            .send(Ok(ResponseEvent::OutputItemAdded(item)))
            .await
            .is_err()
        {
            return Ok(false);
        }
        self.active = true;
        Ok(true)
    }

    async fn push_text(
        &mut self,
        tx_event: &mpsc::Sender<Result<ResponseEvent>>,
        text: &str,
    ) -> Result<bool> {
        if text.is_empty() {
            return Ok(true);
        }
        if !self.active && !self.ensure_started(tx_event).await? {
            return Ok(false);
        }

        self.buffer.push_str(text);
        let sent = tx_event
            .send(Ok(ResponseEvent::OutputTextDelta(text.to_string())))
            .await
            .is_ok();
        Ok(sent)
    }

    async fn flush(&mut self, tx_event: &mpsc::Sender<Result<ResponseEvent>>) -> Result<bool> {
        if !self.active {
            return Ok(true);
        }
        let text = std::mem::take(&mut self.buffer);
        let thought_signature = self.thought_signature.take();
        let item = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText { text }],
            thought_signature,
        };
        self.active = false;
        Ok(tx_event
            .send(Ok(ResponseEvent::OutputItemDone(item)))
            .await
            .is_ok())
    }
}

struct GeminiCallTracker {
    next: usize,
}

impl Default for GeminiCallTracker {
    fn default() -> Self {
        Self { next: 1 }
    }
}

impl GeminiCallTracker {
    fn next_call_id(&mut self) -> String {
        let current = self.next;
        self.next += 1;
        format!("gemini_call_{current}")
    }
}

fn reasoning_effort_to_thinking_level(effort: ReasoningEffortConfig) -> Option<String> {
    match effort {
        ReasoningEffortConfig::None => None,
        ReasoningEffortConfig::Minimal | ReasoningEffortConfig::Low => Some("LOW".to_string()),
        ReasoningEffortConfig::Medium
        | ReasoningEffortConfig::High
        | ReasoningEffortConfig::XHigh => Some("HIGH".to_string()),
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum GeminiStreamEnvelope {
    Wrapped { result: GeminiStreamResult },
    Unwrapped(GeminiStreamResult),
}

#[derive(Deserialize)]
struct GeminiStreamResult {
    #[serde(default)]
    candidates: Option<Vec<crate::gemini_models::Candidate>>,
    #[serde(rename = "usageMetadata")]
    usage_metadata: Option<UsageMetadata>,
}

fn extract_request_id(headers: &HeaderMap) -> Option<String> {
    const REQUEST_ID_CANDIDATES: &[&str] = &["x-request-id", "x-oai-request-id", "cf-ray"];
    for header in REQUEST_ID_CANDIDATES {
        let name = HeaderName::from_static(header);
        if let Some(value) = headers.get(&name)
            && let Ok(parsed) = value.to_str()
        {
            return Some(parsed.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD_NO_PAD;

    const GEMINI_THOUGHT_SIGNATURE_PREFIX: &str = "codex_thought_sig_";

    fn thought_signature_for(call_id: &str) -> String {
        let raw = if call_id.starts_with(GEMINI_THOUGHT_SIGNATURE_PREFIX) {
            call_id.to_string()
        } else {
            format!("{GEMINI_THOUGHT_SIGNATURE_PREFIX}{call_id}")
        };
        STANDARD_NO_PAD.encode(raw.as_bytes())
    }

    #[test]
    fn parses_stream_candidates() {
        let raw = r#"{"result":{"candidates":[{"content":{"parts":[{"text":"Hi"}]}}]}}"#;
        let envelope: GeminiStreamEnvelope = serde_json::from_str(raw).expect("parse");
        let result = match envelope {
            GeminiStreamEnvelope::Wrapped { result } => result,
            GeminiStreamEnvelope::Unwrapped(result) => result,
        };
        let candidates = result.candidates.expect("candidates");
        assert_eq!(candidates.len(), 1);
        let content = candidates[0].content.as_ref().expect("content");
        assert_eq!(content.parts.len(), 1);
        assert_eq!(content.parts[0].text.as_deref(), Some("Hi"));
    }

    #[test]
    fn parses_unwrapped_stream_candidates() {
        let raw = r#"{"candidates":[{"content":{"parts":[{"text":"Hello"}]}}]}"#;
        let envelope: GeminiStreamEnvelope = serde_json::from_str(raw).expect("parse");
        let result = match envelope {
            GeminiStreamEnvelope::Wrapped { result } => result,
            GeminiStreamEnvelope::Unwrapped(result) => result,
        };
        let candidates = result.candidates.expect("candidates");
        assert_eq!(candidates.len(), 1);
        let content = candidates[0].content.as_ref().expect("content");
        assert_eq!(content.parts[0].text.as_deref(), Some("Hello"));
    }

    #[test]
    fn thought_signature_is_base64_encoded() {
        let sig = thought_signature_for("gemini_call_1");
        let decoded = STANDARD_NO_PAD
            .decode(sig.as_bytes())
            .expect("base64 decode");
        let decoded = String::from_utf8(decoded).expect("utf8");
        assert!(decoded.contains("gemini_call_1"), "{decoded}");
        assert!(
            decoded.starts_with(GEMINI_THOUGHT_SIGNATURE_PREFIX),
            "{decoded}"
        );
    }
}

#[test]
fn test_history_mapping_parallel_calls_and_outputs() {
    let items = vec![
        ResponseItem::FunctionCall {
            id: None,
            name: "fc1".to_string(),
            arguments: "{}".to_string(),
            call_id: "call_1".to_string(),
            thought_signature: Some("sig1".to_string()),
        },
        ResponseItem::FunctionCall {
            id: None,
            name: "fc2".to_string(),
            arguments: "{}".to_string(),
            call_id: "call_2".to_string(),
            thought_signature: None,
        },
        ResponseItem::FunctionCallOutput {
            call_id: "call_1".to_string(),
            output: FunctionCallOutputPayload {
                content: "out1".to_string(),
                ..Default::default()
            },
            thought_signature: Some("sig1".to_string()),
        },
        ResponseItem::FunctionCallOutput {
            call_id: "call_2".to_string(),
            output: FunctionCallOutputPayload {
                content: "out2".to_string(),
                ..Default::default()
            },
            thought_signature: None,
        },
    ];

    let contents = map_history_to_contents(&items);

    let model_contents: Vec<_> = contents.iter().filter(|c| c.role == "model").collect();
    assert_eq!(model_contents.len(), 1, "Expected 1 merged model content");
    assert_eq!(
        model_contents[0].parts.len(),
        2,
        "Expected 2 parts in merged content"
    );

    let user_contents: Vec<_> = contents.iter().filter(|c| c.role == "user").collect();
    assert_eq!(user_contents.len(), 1, "Expected 1 merged user content");
    assert_eq!(
        user_contents[0].parts.len(),
        2,
        "Expected 2 parts in merged content"
    );

    let out1 = &user_contents[0].parts[0];
    assert_eq!(
        out1.thought_signature.as_deref(),
        None,
        "User output 1 should NOT have signature"
    );
}
