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

## Features

### Programmatic Tool Calling (Unified Exec)

**Why we did this**:
Standard agent interactions often require multiple round-trips to perform complex tasks. For example, if the model needs to query a list of users, filter them based on some logic, and then fetch details for the filtered users, it would typically need to:

1. Call a tool to get users.
2. Wait for the response.
3. Process the list internally (consuming tokens).
4. Call another tool for each user.

This "chatty" protocol is slow and expensive. Programmatic Tool Calling allows the model to write a small script (e.g., Python) that executes these steps in a single turn, running locally within the agent's environment.

**How is it implemented**:
The feature is built on top of the `unified_exec` runtime.

1. **Helper Injection**: When a `unified_exec` session starts, the agent injects a `codex_tools.py` module into the environment. This module contains Python stubs for all tools registered with the agent (e.g., `shell`, `web_search`, etc.).
2. **IPC Protocol**: These stubs do not execute the tools directly. Instead, they print a special IPC message to stdout: `<<TOOL_CALL>>{...}<<END_TOOL_CALL>>`.
3. **Interception**: The `UnifiedExecSessionManager` monitors the process output. When it detects this pattern, it pauses the process, parses the tool request, and executes the actual tool (which might be a Rust function or an MCP tool).
4. **Result Injection**: The result is serialized and written back to the process's stdin using `<<TOOL_RESULT>>...<<END_TOOL_RESULT>>`. The Python stub reads this result and returns it to the script as a native object.

**How to use it**:
The model can generate a Python script that imports `codex_tools` and calls available tools as functions.

```python
import codex_tools
import json

# Example: Using the shell tool programmatically
# The 'command' argument must match the tool's schema
result = codex_tools.shell(command=["echo", "Hello from subtool"])
print(f"Tool returned: {result}")
```

Enable this feature by setting `experimental_use_unified_exec_tool = true` in your `config.toml` or passing `--enable unified_exec`.

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
