The changelog can be found on the [releases page](https://github.com/openai/codex/releases).

## [Unreleased]

### Gemini Support
- **Native Adapter**: Implemented a fully native adapter for Gemini models, bypassing OpenAI-compatible proxies for better stability and feature support.
- **Reasoning Models**: Added support for Gemini 3.0+ reasoning models, including handling of `thought_signature` and `thinking_config`.
- **Media Support**: Added native support for inline images and PDFs in prompts.
- **Strict Topology**: Enforced strict conversation topology (User -> Model -> User) and message coalescing to prevent 400 Bad Request errors.
- **Tooling**: Implemented strict JSON schema sanitization for tool definitions to match Gemini's requirements.
- **Authentication**: Improved API key handling and authentication logic.

### Refactor
- **Driver Architecture**: Introduced a pluggable `ModelDriver` trait with dedicated OpenAI and Gemini implementations so providers can select their wire API without touching core call sites.
