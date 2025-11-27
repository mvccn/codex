# Gemini Support

## Design

we do not chose the proxy route which is basically do a open ai compatible transform for gemini calls. We choose to natively support GEMINI model.

## Changes for Gemini support

- Added Gemini-specific prompt scaffolding and routed Gemini slugs to it so Codex emits tuned base instructions when targeting Google models
  (core/gemini_codex_prompt.md:1, core/src/model_family.rs:13-245, core/src/lib.rs:14).
- Introduced a fully native Gemini adapter that picks streaming vs non-streaming transports, parses SSE payloads into Codex ResponseEvents,
  and handles tool/function calls (core/src/gemini.rs:1). ModelClient::stream now dispatches to this adapter whenever a provider declares the
  new WireApi::Gemini (core/src/client.rs:139).
- Extended the provider registry with WireApi::Gemini, including URL construction, API key header wiring, and conversion to the shared API
  layer; Gemini providers without explicit URLs now auto-inherit canonical generateContent endpoints derived from the active model (core/src/
  model_provider_info.rs:41-217, core/src/config/mod.rs:1070-1131, core/src/gemini_models.rs:1-210).
- Catalog now includes `gemini-3.0-pro` so Codex can build default URLs and headers for the stable 3.x Pro release.
- Added tooling glue so existing ToolSpec definitions serialize into Gemini’s function_declarations format with OpenAI-only flags stripped,
  keeping delimiter compatibility for function calls (core/src/tools/spec.rs:830-905).
- Expanded the public interface to expose the Gemini metadata module for future callers while keeping adapter internals crate-private (core/
  src/lib.rs:14-82).
- Landed a dedicated integration test suite that exercises non-streaming and SSE flows plus tool-call translation against a mock Gemini
  server; the test harness registers the new module in the suite (core/tests/suite/gemini.rs:1-214, core/tests/suite/mod.rs:29).

## Live function-calling smoke tests

- Opt-in, ignored by default: `RUN_LIVE_GEMINI=1 GEMINI_API_KEY=... cargo test -p codex-core -- --ignored live_gemini_function_calls`
- Defaults to `models/gemini-2.5-flash`; override with `GEMINI_MODEL=...` if needed.
- Covers four prompts to verify real tool calls: weather lookup, stock-vs-weather tool choice, nested create_order payload, and typo-normalized timers.
- Tests live in `core/src/gemini.rs` under `live_tests`; they short-circuit if credentials or network access are missing.
