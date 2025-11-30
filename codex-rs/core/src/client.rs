use std::fmt;
use std::sync::Arc;

use codex_otel::otel_event_manager::OtelEventManager;
use codex_protocol::ConversationId;
use codex_protocol::config_types::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;

use crate::AuthManager;
use crate::client_common::Prompt;
use crate::client_common::ResponseStream;
use crate::config::Config;
use crate::default_client::create_client;
use crate::error::Result;
use crate::model_family::ModelFamily;
use crate::model_provider_info::ModelProviderInfo;
use crate::model_provider_info::WireApi;
use crate::openai_model_info::get_model_info;

use crate::driver::ModelDriver;
use crate::gemini_driver::GeminiDriver;
use crate::openai_driver::OpenAIDriver;

#[derive(Clone)]
pub struct ModelClient {
    config: Arc<Config>,
    auth_manager: Option<Arc<AuthManager>>,
    otel_event_manager: OtelEventManager,
    provider: ModelProviderInfo,
    conversation_id: ConversationId,
    effort: Option<ReasoningEffortConfig>,
    summary: ReasoningSummaryConfig,
    session_source: SessionSource,
    driver: Arc<dyn ModelDriver>,
}

impl fmt::Debug for ModelClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelClient")
            .field("config", &self.config)
            .field("auth_manager", &self.auth_manager)
            .field("otel_event_manager", &self.otel_event_manager)
            .field("provider", &self.provider)
            .field("conversation_id", &self.conversation_id)
            .field("effort", &self.effort)
            .field("summary", &self.summary)
            .field("session_source", &self.session_source)
            .field("driver", &"<driver>")
            .finish()
    }
}

#[allow(clippy::too_many_arguments)]
impl ModelClient {
    pub fn new(
        config: Arc<Config>,
        auth_manager: Option<Arc<AuthManager>>,
        otel_event_manager: OtelEventManager,
        provider: &ModelProviderInfo,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        conversation_id: ConversationId,
        session_source: SessionSource,
    ) -> Self {
        let driver: Arc<dyn ModelDriver> = match provider.wire_api {
            WireApi::Gemini => Arc::new(GeminiDriver {
                model_family: config.model_family.clone(),
                reasoning_effort: effort.or(config.model_family.default_reasoning_effort),
                client: create_client(),
                provider: provider.clone(),
                otel_event_manager: otel_event_manager.clone(),
                session_source: session_source.clone(),
                auth_manager: auth_manager.clone(),
            }),
            WireApi::Chat | WireApi::Responses => Arc::new(OpenAIDriver {
                config: config.clone(),
                auth_manager: auth_manager.clone(),
                provider: provider.clone(),
                conversation_id,
                session_source: session_source.clone(),
                otel_event_manager: otel_event_manager.clone(),
                effort,
                summary,
            }),
        };

        Self {
            config,
            auth_manager,
            otel_event_manager,
            provider: provider.clone(),
            conversation_id,
            effort,
            summary,
            session_source,
            driver,
        }
    }

    pub fn get_model_context_window(&self) -> Option<i64> {
        let pct = self.config.model_family.effective_context_window_percent;
        self.config
            .model_context_window
            .or_else(|| get_model_info(&self.config.model_family).map(|info| info.context_window))
            .map(|w| w.saturating_mul(pct) / 100)
    }

    pub fn get_auto_compact_token_limit(&self) -> Option<i64> {
        self.config.model_auto_compact_token_limit.or_else(|| {
            get_model_info(&self.config.model_family).and_then(|info| info.auto_compact_token_limit)
        })
    }

    pub fn config(&self) -> Arc<Config> {
        Arc::clone(&self.config)
    }

    pub fn provider(&self) -> &ModelProviderInfo {
        &self.provider
    }

    pub fn get_provider(&self) -> &ModelProviderInfo {
        &self.provider
    }

    pub fn get_model(&self) -> String {
        self.config.model.clone()
    }

    pub fn get_model_family(&self) -> ModelFamily {
        self.config.model_family.clone()
    }

    pub fn get_reasoning_effort(&self) -> Option<ReasoningEffortConfig> {
        self.effort
    }

    pub fn get_reasoning_summary(&self) -> ReasoningSummaryConfig {
        self.summary
    }

    pub fn get_otel_event_manager(&self) -> OtelEventManager {
        self.otel_event_manager.clone()
    }

    pub fn get_auth_manager(&self) -> Option<Arc<AuthManager>> {
        self.auth_manager.clone()
    }

    pub fn get_session_source(&self) -> SessionSource {
        self.session_source.clone()
    }

    /// Streams a single model turn using the configured driver.
    pub async fn stream(&self, prompt: &Prompt) -> Result<ResponseStream> {
        self.driver.stream(prompt).await
    }

    pub async fn compact_conversation_history(&self, prompt: &Prompt) -> Result<Vec<ResponseItem>> {
        self.driver.compact_conversation_history(prompt).await
    }

    /// Returns the name of the underlying driver.
    pub fn driver_name(&self) -> &'static str {
        self.driver.driver_name()
    }
}
