use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerateContentRequest {
    pub contents: Vec<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(rename = "toolConfig", skip_serializing_if = "Option::is_none")]
    pub tool_config: Option<ToolConfig>,
    #[serde(rename = "generationConfig")]
    pub generation_config: Option<GenerationConfig>,
    #[serde(rename = "systemInstruction")]
    pub system_instruction: Option<Content>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolConfig {
    #[serde(
        rename = "functionCallingConfig",
        skip_serializing_if = "Option::is_none"
    )]
    pub function_calling_config: Option<FunctionCallingConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCallingConfig {
    #[serde(rename = "mode", skip_serializing_if = "Option::is_none")]
    pub mode: Option<FunctionCallingMode>,
    #[serde(
        rename = "allowedFunctionNames",
        alias = "allowed_function_names",
        skip_serializing_if = "Option::is_none"
    )]
    pub allowed_function_names: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FunctionCallingMode {
    Auto,
    Any,
    None,
    Validated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Content {
    #[serde(default)]
    pub role: String,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Part {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(rename = "inlineData", skip_serializing_if = "Option::is_none")]
    pub inline_data: Option<Blob>,
    #[serde(rename = "mediaResolution", skip_serializing_if = "Option::is_none")]
    pub media_resolution: Option<MediaResolution>,
    #[serde(rename = "functionCall", skip_serializing_if = "Option::is_none")]
    pub function_call: Option<FunctionCall>,
    #[serde(rename = "functionResponse", skip_serializing_if = "Option::is_none")]
    pub function_response: Option<FunctionResponse>,
    // Gemini 3.0+ thought signature field (opaque bytes, base64 encoded string)
    #[serde(
        rename = "thoughtSignature",
        alias = "thought_signature",
        skip_serializing_if = "Option::is_none"
    )]
    pub thought_signature: Option<String>,
    // Gemini thinking process summary flag
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blob {
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaResolution {
    pub level: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionResponse {
    pub name: String,
    pub response: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    #[serde(
        rename = "functionDeclarations",
        alias = "function_declarations",
        skip_serializing_if = "Option::is_none"
    )]
    pub function_declarations: Option<Vec<FunctionDeclaration>>,
    #[serde(rename = "googleSearch", skip_serializing_if = "Option::is_none")]
    pub google_search: Option<serde_json::Value>,
    #[serde(rename = "codeExecution", skip_serializing_if = "Option::is_none")]
    pub code_execution: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDeclaration {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationConfig {
    #[serde(rename = "maxOutputTokens", skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(rename = "topK", skip_serializing_if = "Option::is_none")]
    pub top_k: Option<i32>,
    #[serde(
        rename = "thinkingConfig",
        alias = "thinking_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub thinking_config: Option<ThinkingConfig>,
    #[serde(rename = "responseMimeType", skip_serializing_if = "Option::is_none")]
    pub response_mime_type: Option<String>,
    #[serde(rename = "responseJsonSchema", skip_serializing_if = "Option::is_none")]
    pub response_json_schema: Option<serde_json::Value>,
    #[serde(rename = "imageConfig", skip_serializing_if = "Option::is_none")]
    pub image_config: Option<ImageConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingConfig {
    #[serde(
        rename = "thinkingLevel",
        alias = "thinking_level",
        skip_serializing_if = "Option::is_none"
    )]
    pub thinking_level: Option<String>,
    #[serde(
        rename = "thinkingBudget",
        alias = "thinking_budget",
        skip_serializing_if = "Option::is_none"
    )]
    pub thinking_budget: Option<i32>,
    #[serde(
        rename = "includeThoughts",
        alias = "include_thoughts",
        skip_serializing_if = "Option::is_none"
    )]
    pub include_thoughts: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageConfig {
    #[serde(rename = "aspectRatio", skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<String>,
    #[serde(rename = "imageSize", skip_serializing_if = "Option::is_none")]
    pub image_size: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerateContentResponse {
    pub candidates: Option<Vec<Candidate>>,
    pub prompt_feedback: Option<PromptFeedback>,
    #[serde(rename = "usageMetadata")]
    pub usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub error: Option<GoogleApiError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleApiError {
    pub code: i32,
    pub message: String,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub content: Option<Content>,
    #[serde(rename = "finishReason")]
    pub finish_reason: Option<String>,
    pub index: Option<i32>,
    #[serde(rename = "safetyRatings")]
    pub safety_ratings: Option<Vec<SafetyRating>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptFeedback {
    #[serde(rename = "blockReason")]
    pub block_reason: Option<String>,
    #[serde(rename = "safetyRatings")]
    pub safety_ratings: Option<Vec<SafetyRating>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyRating {
    pub category: String,
    pub probability: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageMetadata {
    #[serde(rename = "promptTokenCount")]
    pub prompt_token_count: Option<i64>,
    #[serde(rename = "candidatesTokenCount")]
    pub candidates_token_count: Option<i64>,
    #[serde(rename = "totalTokenCount")]
    pub total_token_count: Option<i64>,
    #[serde(rename = "thoughtsTokenCount")]
    pub thoughts_token_count: Option<i64>,
}

/// Derive the canonical Gemini generateContent REST URL for a model slug.
pub fn default_gemini_generate_url(model: &str) -> Option<String> {
    let normalized = if model.starts_with("models/") {
        model.to_string()
    } else if model.starts_with("gemini") {
        format!("models/{model}")
    } else {
        return None;
    };

    Some(format!(
        "https://generativelanguage.googleapis.com/v1beta/{normalized}:generateContent"
    ))
}
