# Gemini Support

This document outlines the native integration of Google's Gemini models into Codex.

## Design Philosophy

We chose to implement a fully native adapter for Gemini rather than relying on an OpenAI-compatible proxy or translation layer. While a proxy approach offers a faster initial integration, it fails to address fundamental architectural differences that are critical for robustness:

1.  **Stateful Thought Signatures**: Gemini 3.0+ models emit opaque `thought_signature` tokens during reasoning and function calling. These must be preserved and accurately threaded back into specific parts of the conversation history in subsequent turns. An OpenAI-style message list has no standard slot for this data, and "hiding" it in metadata fields often leads to context loss or validation errors.
2.  **Strict Conversation Topology**: Gemini enforces a rigid `User -> Model -> User` turn structure and requires that parallel function calls be coalesced into single content blocks. OpenAI's more permissive message structure (e.g., separate tool call messages) does not map 1:1, leading to 400 Bad Request errors if the history isn't precisely reshaped.
3.  **Schema Divergence**: Gemini's tool definition schema is a strict subset of JSON Schema that explicitly rejects fields like `strict` and `additionalProperties`—defaults in the OpenAI ecosystem. A naive proxy would pass these invalid fields, causing API rejections.
4.  **Native Features**: Features like `thinking_config` (for reasoning models) and `safety_settings` have unique semantics that don't map cleanly to OpenAI's configuration parameters.

By building a native adapter, we ensure correct handling of these nuances, resulting in a more stable and capable integration.

## Implementation Details

### Native Adapter (`core/src/gemini.rs`)
- **Direct API Client**: A dedicated async client that handles authentication, request construction, and response parsing specific to the Google Generative Language API.
- **Dual Transport Support**:
  - **SSE (Server-Sent Events)**: Fully supported via `streamGenerateContent`, including handling of mixed text/tool-call chunks.
  - **Standard JSON**: Supported via `generateContent` for non-streaming use cases.
- **History Mapping**: Implements a `map_history_to_contents` transformation that converts Codex's linear conversation history into Gemini's multi-part `Content` format.
  - **Coalescing**: Adjacent messages of the same role (e.g., multiple function calls or mixed text/calls) are coalesced into a single `Content` block, which is critical for maintaining valid conversation state in Gemini.
  - **Strict Output handling**: Ensures `FunctionCallOutput` items (User role) do not carry `thought_signature` fields, as these are invalid in user messages.

### Data Models (`core/src/gemini_models.rs`)
- **Type-Safe Structs**: Comprehensive Rust structs representing the Gemini API schema (`GenerateContentRequest`, `Candidate`, `Part`, etc.).
- **Reasoning Support**: Includes fields for `thought_signature` (Gemini 3.0+) and `thinking_config` (for reasoning models), allowing Codex to support "thinking" models natively.
- **Token Usage**: Captures and reports `usageMetadata` for proper accounting.

### Tooling & Function Calling (`core/src/tools/spec.rs`)
- **Schema Sanitization**: A dedicated `create_tools_json_for_gemini_api` helper strips OpenAI-specific fields (like `strict` and `additionalProperties`) from JSON schemas, as Gemini implements a stricter subset of JSON Schema.
- **Tool Choice**: Automatically formats Codex `ToolSpec` into Gemini `function_declarations`.

### Thought Signatures & Reasoning
- **Signatures**: For models that emit `thought_signature` (like Gemini 3.0 Pro), the adapter captures these opaque strings and ensures they are attached to the correct `FunctionCall` parts in subsequent requests.
- **Validation**: Logic ensures that signatures are only sent where expected (on the first function call part of a model turn) and never where prohibited (on user tool outputs), preventing 400 Bad Request errors.

## Configuration & Usage

- **Provider**: The integration is triggered when the provider `wire_api` is set to `Gemini`.
- **Default URLs**: Canonical URLs are auto-generated for known models (e.g., `models/gemini-2.0-flash`).
- **Prompt Scaffolding**: Uses a specialized system prompt (`core/gemini_codex_prompt.md`) tuned for Gemini's instruction-following capabilities.

## Testing

### Integration Suite (`core/tests/suite/gemini.rs`)
A comprehensive `wiremock`-based test suite covers:
- Streaming and non-streaming round trips.
- Complex function calling (single, parallel, and sequential).
- Error handling in SSE streams.
- Verification of payload shapes (checking for absence of invalid fields like `strict`).
- **History Coalescing**: Tests verify that parallel calls are correctly merged and signatures are handled properly.

### Live Smoke Tests
- Opt-in tests against the real Gemini API are available for verification.
- Run with: `RUN_LIVE_GEMINI=1 GEMINI_API_KEY=... cargo test -p codex-core -- --ignored live_gemini_function_calls`
