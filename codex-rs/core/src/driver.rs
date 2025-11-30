use crate::client_common::Prompt;
use crate::client_common::ResponseStream;
use crate::error::Result;
use async_trait::async_trait;
use codex_protocol::models::ResponseItem;

/// A pluggable adapter that knows how to translate Codex prompts into a
/// model-specific wire protocol.
#[async_trait]
pub trait ModelDriver: Send + Sync {
    /// Stream a single turn through the underlying provider.
    async fn stream(&self, prompt: &Prompt) -> Result<ResponseStream>;
    /// Compact a conversation using the provider's native compaction endpoint.
    async fn compact_conversation_history(&self, prompt: &Prompt) -> Result<Vec<ResponseItem>>;
    /// Human‑readable name used for diagnostics and metrics.
    fn driver_name(&self) -> &'static str;
}
