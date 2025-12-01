# Gemini Context Caching Design

## Goal
Reduce latency and token costs for Gemini by caching the static prompt prefix (system prompt + large read-only context such as file-search indices or opened files) and sending only the dynamic suffix on subsequent turns. Gemini supports prefix reuse via the `cachedContents/{id}` resource.

## Current State (Problems)
- We always send full `contents` to Gemini and set `cachedContent: None`.
- `ContextManager::get_partitioned_history_for_prompt()` already detects a static prefix, but Gemini does not use it.
- No cache creation, reuse, or invalidation. No cache keys or storage.
- No fallback handling when cache creation fails.
- No tests to guard cache request shape or reuse behavior.

## Proposed Solution
1. **Provider-agnostic split**: Use `ContextManager::get_partitioned_history_for_prompt()` to split `(static_prefix, dynamic_suffix)` for any model that supports caching.
2. **Cache abstraction**: Introduce a small caching trait (e.g., `PromptCacheClient`) with methods to:
   - Hash/key the static prefix (includes model slug and serialized static items).
   - Create a provider-specific cache entry and return a handle (Gemini: `cachedContents/{id}`).
   - Store/retrieve handles in a cache map (per-session or persisted if desired).
3. **Gemini implementation**:
   - When static prefix exceeds thresholds, compute cache key.
   - Reuse existing handle if key matches; set `cachedContent` and send only the dynamic suffix.
   - If no handle, call `cachedContents.create` with the static parts, store the returned handle, then send dynamic suffix + `cachedContent`.
   - On failure, fall back to full payload (no `cachedContent`) and optionally retry once.
4. **Invalidation**:
   - Cache key includes the serialized static prefix and model slug; any change invalidates reuse.
   - Add expiry handling if the provider returns TTL metadata (Gemini caches have lifetimes).
5. **Telemetry and safety**:
   - Log cache hits/misses and creation failures.
   - Respect size thresholds (skip caches smaller than the current `MIN_STATIC_PREFIX_BYTES` logic).
6. **Testing**:
   - Integration tests with mocked Gemini endpoints:
     - Cache creation path: `cachedContents.create` -> subsequent `generateContent` includes `cachedContent` and omits static parts.
     - Reuse path: existing handle is used; dynamic-only `contents`.
     - Fallback path: creation failure -> full contents, no `cachedContent`.
   - Unit tests for cache keying/invalidation.

## Extension to Other Providers
- Keep the split/key/storage logic reusable; only the cache client and request field are provider-specific.
- Providers without caching simply skip the cache client and send full contents.

## Expected Impact
- Lower time-to-first-token (TTFT) and reduced token spend on turns after the static prefix is cached, especially with large repositories or documentation contexts.
