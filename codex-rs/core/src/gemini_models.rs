use crate::WireApi;

/// Static metadata for Gemini models supported by Codex.
///
/// This struct keeps together:
/// - The canonical Gemini model id (as used by the HTTP API).
/// - Slug aliases (for example `models/gemini-2.5-flash` vs `gemini-2.5-flash`).
/// - Detailed capability flags copied from the public Gemini docs.
/// - Default REST endpoint suffixes for non‑streaming and streaming calls.
#[derive(Debug, Clone)]
pub struct GeminiCapabilities {
    pub batch_api: bool,
    pub caching: bool,
    pub code_execution: bool,
    pub file_search: bool,
    pub function_calling: bool,
    pub grounding_with_google_maps: bool,
    pub image_generation: bool,
    pub live_api: bool,
    pub search_grounding: bool,
    pub structured_outputs: bool,
    pub thinking: bool,
    pub url_context: bool,
}

#[derive(Debug, Clone)]
pub struct GeminiModelInfo {
    /// Canonical Gemini model id, for example `gemini-2.5-flash`.
    pub id: &'static str,
    /// Accepted aliases for this model, typically including `models/<id>`.
    pub aliases: &'static [&'static str],
    /// Human‑readable summary of the model’s strengths.
    pub description: &'static str,
    /// Capability flags (Batch API, file search, structured outputs, etc.)
    /// sourced from the official Gemini model docs.
    pub capabilities: GeminiCapabilities,
    /// REST endpoint suffix for non‑streaming content generation.
    ///
    /// This is appended to `.../models/{id}` when constructing the base URL.
    pub endpoint_suffix_generate: &'static str,
    /// REST endpoint suffix for streaming content generation.
    ///
    /// This is appended to `.../models/{id}` when constructing the base URL
    /// for SSE streaming.
    pub endpoint_suffix_stream: &'static str,
}

/// Default public Gemini API host and API version used by Codex.
pub const GEMINI_DEFAULT_API_ROOT: &str = "https://generativelanguage.googleapis.com";
pub const GEMINI_DEFAULT_API_VERSION: &str = "v1beta";

/// Catalog of Gemini models Codex knows about. This list is intentionally
/// small and focused on the most common general‑purpose models in the 2.5 and
/// 3.x families; it can be extended over time without breaking existing
/// callers.
const GEMINI_MODELS: &[GeminiModelInfo] = &[
    GeminiModelInfo {
        id: "gemini-2.5-pro",
        aliases: &["gemini-2.5-pro", "models/gemini-2.5-pro"],
        description: "Gemini 2.5 Pro: state‑of‑the‑art thinking model for complex reasoning and long‑context tasks.",
        capabilities: GeminiCapabilities {
            batch_api: true,
            caching: true,
            code_execution: true,
            file_search: true,
            function_calling: true,
            grounding_with_google_maps: true,
            image_generation: false,
            live_api: false,
            search_grounding: true,
            structured_outputs: true,
            thinking: true,
            url_context: true,
        },
        endpoint_suffix_generate: ":generateContent",
        endpoint_suffix_stream: ":streamGenerateContent",
    },
    GeminiModelInfo {
        id: "gemini-2.5-flash",
        aliases: &["gemini-2.5-flash", "models/gemini-2.5-flash"],
        description: "Gemini 2.5 Flash: fast, price‑efficient model for large‑scale processing and agentic use‑cases.",
        capabilities: GeminiCapabilities {
            batch_api: true,
            caching: true,
            code_execution: true,
            file_search: true,
            function_calling: true,
            grounding_with_google_maps: true,
            image_generation: false,
            live_api: false,
            search_grounding: true,
            structured_outputs: true,
            thinking: true,
            url_context: true,
        },
        endpoint_suffix_generate: ":generateContent",
        endpoint_suffix_stream: ":streamGenerateContent",
    },
    GeminiModelInfo {
        id: "gemini-3-pro-preview",
        aliases: &["gemini-3-pro-preview", "models/gemini-3-pro-preview"],
        description: "Gemini 3 Pro Preview: latest multimodal model with advanced reasoning and interactivity.",
        capabilities: GeminiCapabilities {
            batch_api: true,
            caching: true,
            code_execution: true,
            file_search: true,
            function_calling: true,
            grounding_with_google_maps: false,
            image_generation: false,
            live_api: false,
            search_grounding: true,
            structured_outputs: true,
            thinking: true,
            url_context: true,
        },
        endpoint_suffix_generate: ":generateContent",
        endpoint_suffix_stream: ":streamGenerateContent",
    },
    GeminiModelInfo {
        id: "gemini-3.0-pro",
        aliases: &["gemini-3.0-pro", "models/gemini-3.0-pro"],
        description: "Gemini 3.0 Pro: stable next‑gen Pro model with full tool support.",
        capabilities: GeminiCapabilities {
            batch_api: true,
            caching: true,
            code_execution: true,
            file_search: true,
            function_calling: true,
            grounding_with_google_maps: false,
            image_generation: false,
            live_api: false,
            search_grounding: true,
            structured_outputs: true,
            thinking: true,
            url_context: true,
        },
        endpoint_suffix_generate: ":generateContent",
        endpoint_suffix_stream: ":streamGenerateContent",
    },
];

/// Returns static metadata for a Gemini model slug, if known.
///
/// The slug may include a `models/` prefix (for example `models/gemini-2.0-flash`)
/// or be the bare model id (`gemini-2.0-flash`); both map to the same entry.
pub(crate) fn find_gemini_model(slug: &str) -> Option<&'static GeminiModelInfo> {
    let normalized = slug.strip_prefix("models/").unwrap_or(slug);
    GEMINI_MODELS.iter().find(|info| {
        info.id == normalized
            || info
                .aliases
                .iter()
                .any(|alias| *alias == slug || *alias == normalized)
    })
}

/// Build a default non‑streaming REST base URL for the given Gemini model.
///
/// Example:
/// - `model_slug = "models/gemini-2.0-flash"`
/// - returns `https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent`
pub(crate) fn default_gemini_generate_url(model_slug: &str) -> Option<String> {
    let info = find_gemini_model(model_slug)?;
    Some(format!(
        "{}/{}/models/{}{}",
        GEMINI_DEFAULT_API_ROOT, GEMINI_DEFAULT_API_VERSION, info.id, info.endpoint_suffix_generate
    ))
}

/// Build a default streaming REST base URL + query params for the given
/// Gemini model.
///
/// Example:
/// - `model_slug = "models/gemini-2.0-flash"`
/// - returns:
///   - base URL: `https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:streamGenerateContent`
///   - query params: `{ "alt": "sse" }`
pub(crate) fn default_gemini_streaming_url(
    model_slug: &str,
) -> Option<(String, std::collections::HashMap<String, String>)> {
    let info = find_gemini_model(model_slug)?;
    let mut params = std::collections::HashMap::new();
    params.insert("alt".to_string(), "sse".to_string());
    let url = format!(
        "{}/{}/models/{}{}",
        GEMINI_DEFAULT_API_ROOT, GEMINI_DEFAULT_API_VERSION, info.id, info.endpoint_suffix_stream
    );
    Some((url, params))
}

/// Convenience helper for building a `ModelProviderInfo` that targets the
/// public Gemini HTTP API for a specific model. This is not wired into the
/// configuration loader yet, but can be used by callers that want a strongly
/// typed way to construct providers for Gemini.
pub(crate) fn create_gemini_provider_for_model(
    model_slug: &str,
    provider_name: &str,
    api_key_env: &str,
    streaming: bool,
) -> Option<crate::ModelProviderInfo> {
    let (base_url, query_params) = if streaming {
        default_gemini_streaming_url(model_slug)?
    } else {
        (
            default_gemini_generate_url(model_slug)?,
            std::collections::HashMap::new(),
        )
    };

    Some(crate::ModelProviderInfo {
        name: provider_name.to_string(),
        base_url: Some(base_url),
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api: WireApi::Gemini,
        query_params: if query_params.is_empty() {
            None
        } else {
            Some(query_params)
        },
        http_headers: None,
        env_http_headers: Some(
            std::iter::once(("x-goog-api-key".to_string(), api_key_env.to_string())).collect(),
        ),
        request_max_retries: Some(0),
        stream_max_retries: Some(0),
        stream_idle_timeout_ms: Some(300_000),
        requires_openai_auth: false,
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn find_gemini_model_matches_aliases() {
        let info = find_gemini_model("models/gemini-2.5-flash").expect("model info");
        assert_eq!("gemini-2.5-flash", info.id);

        let info2 = find_gemini_model("gemini-2.5-flash").expect("model info");
        assert_eq!(info.id, info2.id);
    }

    #[test]
    fn default_urls_match_expected_shape() {
        let url = default_gemini_generate_url("models/gemini-2.5-flash").expect("url");
        assert!(
            url.ends_with("models/gemini-2.5-flash:generateContent"),
            "unexpected generateContent URL: {url}"
        );

        let (stream_url, params) =
            default_gemini_streaming_url("models/gemini-2.5-flash").expect("stream url");
        assert!(
            stream_url.ends_with("models/gemini-2.5-flash:streamGenerateContent"),
            "unexpected streamGenerateContent URL: {stream_url}"
        );
        assert_eq!(params.get("alt").map(String::as_str), Some("sse"));
    }

    #[test]
    fn default_urls_cover_gemini_three_pro() {
        let info = find_gemini_model("models/gemini-3.0-pro").expect("model info");
        assert_eq!(info.id, "gemini-3.0-pro");

        let url = default_gemini_generate_url("gemini-3.0-pro").expect("url");
        assert!(
            url.ends_with("models/gemini-3.0-pro:generateContent"),
            "unexpected generateContent URL for gemini-3.0-pro: {url}"
        );

        let (stream_url, params) =
            default_gemini_streaming_url("gemini-3.0-pro").expect("stream url");
        assert!(
            stream_url.ends_with("models/gemini-3.0-pro:streamGenerateContent"),
            "unexpected streamGenerateContent URL for gemini-3.0-pro: {stream_url}"
        );
        assert_eq!(params.get("alt").map(String::as_str), Some("sse"));
    }

    #[test]
    fn create_gemini_provider_uses_x_goog_api_key_header() {
        let provider = create_gemini_provider_for_model(
            "models/gemini-2.5-flash",
            "Google Gemini",
            "GEMINI_API_KEY",
            false,
        )
        .expect("provider");

        assert_eq!(provider.wire_api, WireApi::Gemini);
        assert_eq!(provider.name, "Google Gemini".to_string());
        assert!(
            provider
                .base_url
                .as_ref()
                .unwrap()
                .ends_with("models/gemini-2.5-flash:generateContent")
        );
        let env_headers = provider.env_http_headers.as_ref().expect("env headers");
        assert_eq!(
            env_headers.get("x-goog-api-key").map(String::as_str),
            Some("GEMINI_API_KEY")
        );
        assert!(provider.http_headers.is_none());
    }
}
