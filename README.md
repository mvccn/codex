# Codex (Gemini Edition)

This repository is a specialized fork of the Codex project, architected to provide a fully native, robust integration with Google's Gemini models.

## Project Origin & Philosophy

The originial Codex offers a solid framework for agent coding assitents, written in the rust, it is fast. we particularly want to leverage the following:

1. Robust "Apply Patch" Logic
2. Model Context Protocol (MCP) Integration
3. Advanced Sandboxing & Security
4. Production-Grade TUI (Ratatui Implementation): although we have implements a features rich cliens on iOS and mac.

The codex however, is tied to openai's models and is not easily expandable to to support other model, that is why created the project "Open Codex"

**Why a Simple OpenAI Proxy Won't Work:**
Attempting to force Gemini into an OpenAI-shaped hole leads to significant issues:

1. **Stateful Thought Signatures**: Gemini 3.0+ models use opaque `thought_signature` tokens for reasoning. These must be preserved and threaded back into specific conversation slots. OpenAI proxies typically discard or mishandle these, breaking the reasoning chain.
2. **Strict Conversation Topology**: Gemini enforces a rigid `User -> Model -> User` flow and demands that parallel function calls be coalesced into single blocks. Proxies often fail to reshape history correctly, causing 400 Bad Request errors.
3. **Schema Divergence**: Gemini uses a stricter subset of JSON Schema. Passing OpenAI defaults (like `strict` or `additionalProperties`) causes API rejections.
4. **Native Features**: Features like `thinking_config` and `safety_settings` have no direct equivalent in the OpenAI API.

**Our Philosophy:**
We believe in a **native-first approach**. By building a dedicated adapter that speaks Gemini's native protocol (Google Generative Language API), we unlock the full capabilities of the model—including reliable reasoning, robust function calling, and multimodal inputs—without the translation loss of a proxy.

## Key Features

- **Native Gemini Adapter**: Built from the ground up to handle Gemini's unique API quirks and requirements.
- **Multimodal Support**: Native support for **PDF and Image inputs**, allowing the model to reason about documents and visuals directly.
- **Advanced Reasoning**: Full support for Gemini 3.0+ "thinking" models, including the handling of thought signatures and thinking configuration.
- **Robust Function Calling**: Correctly handles parallel tool calls, history coalescing, and schema sanitization to ensure high reliability.
- **Direct API Client**: A dedicated async client supporting both SSE (streaming) and standard JSON transports.

## Gemini Differences & Technical Implementation

This project implements several key technical differentiators to support Gemini:

### 1. Stateful Thought Signatures

Gemini 3.0+ models emit `thought_signature` tokens. Our adapter:

- Captures these opaque strings during generation.
- Threads them back into the _exact_ correct location in the conversation history for subsequent turns.
- Ensures they are never sent where prohibited (e.g., in user tool outputs), preventing API errors.

### 2. Strict Conversation Topology

Gemini is strict about turn structure. Our implementation:

- **Coalesces History**: Merges adjacent messages of the same role (e.g., multiple function calls) into single `Content` blocks.
- **Maps History**: Transforms Codex's linear history into Gemini's multi-part format dynamically.

### 3. Schema Handling

- **Sanitization**: Automatically strips OpenAI-specific fields (like `strict`) from JSON schemas before sending them to Gemini.
- **Structured Outputs**: Supports `response_json_schema` for guaranteed output structure.

### 4. Native Tooling

- Supports `ToolConfig` for fine-grained control (e.g., forcing a specific function).
- Integration with native tools like `googleSearch` and `codeExecution`.

## Configuration & Usage

- **Provider**: Set `wire_api` to `Gemini`.
- **Authentication**: Uses standard Google Generative Language API keys.
- **Models**: Canonical URLs are auto-generated (e.g., `models/gemini-2.0-flash`).

## Testing

The project maintains a rigorous test suite to ensure stability:

- **Integration Suite**: Wiremock-based tests cover streaming, function calling, and error handling.
- **Live Smoke Tests**: Opt-in tests against the real Gemini API to verify end-to-end functionality (`RUN_LIVE_GEMINI=1`).
