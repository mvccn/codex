use crate::auth::AuthManager;
use crate::client_common::Prompt;
use crate::client_common::ResponseStream;
use crate::default_client::CodexHttpClient;
use crate::driver::ModelDriver;
use crate::error::CodexErr;
use crate::error::Result;
use crate::gemini::stream_gemini;
use crate::model_family::ModelFamily;
use crate::model_provider_info::ModelProviderInfo;
use async_trait::async_trait;
use codex_otel::otel_event_manager::OtelEventManager;
use codex_protocol::config_types::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use std::sync::Arc;

pub struct GeminiDriver {
    pub model_family: ModelFamily,
    pub reasoning_effort: Option<ReasoningEffortConfig>,
    pub client: CodexHttpClient,
    pub provider: ModelProviderInfo,
    pub otel_event_manager: OtelEventManager,
    pub session_source: SessionSource,
    pub auth_manager: Option<Arc<AuthManager>>,
}

#[async_trait]
impl ModelDriver for GeminiDriver {
    async fn stream(&self, prompt: &Prompt) -> Result<ResponseStream> {
        stream_gemini(
            prompt,
            &self.model_family,
            self.reasoning_effort,
            &self.client,
            &self.provider,
            &self.otel_event_manager,
            &self.session_source,
            &self.auth_manager,
        )
        .await
    }

    async fn compact_conversation_history(&self, _prompt: &Prompt) -> Result<Vec<ResponseItem>> {
        Err(CodexErr::UnsupportedOperation(
            "Compaction not supported for Gemini".to_string(),
        ))
    }

    fn driver_name(&self) -> &'static str {
        "GeminiDriver"
    }
}
