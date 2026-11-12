# Proposal 25: Provider Token Usage Decoding & Output/Cache Cost Metadata (`src/api.rs`, `src/llm/chat_completions.rs`, `src/llm/responses_api.rs`, `src/llm/anthropic.rs`, `src/llm/factory.rs`, `src/config.rs`)

## Prerequisite

None (foundation slice `T29`).

## Problem

`ejaj` currently estimates context size only from outgoing byte length (`context_bytes / 4` in `src/status.rs:79-80`) and reads only input-token pricing (`LLM_{MID,GIGA,SMOLL}_INPUT_PRICE_PER_MILLION_USD` in `src/llm/factory.rs:195-305`):

1. **Actual provider `usage` payloads are discarded (`src/llm/chat_completions.rs:88-127`, `src/llm/responses_api.rs:28-36, 102-193`, `src/llm/anthropic.rs:110-200`):**
   - All three wire formats return exact token counts on successful responses (`prompt_tokens` / `input_tokens`, cached prompt tokens, `completion_tokens` / `output_tokens`, and reasoning tokens), yet `decode(...)` drops the `usage` object completely.
   - Without actual usage counts, `ejaj` cannot measure prompt-cache hit rates across multi-step tool loops, output/reasoning token burn by `reasoning_effort`, or actual token-to-byte ratios per wire format.
2. **No output-token or cached-input-token price metadata (`src/llm/factory.rs:32-80`, `src/config.rs:284-298`):**
   - Output and reasoning tokens routinely cost 3x–8x more per million tokens than uncached input tokens, while cached input tokens cost 50%–90% less. Input-only rates cannot estimate true ticket cost or compare `hybrid` mode (`mid` + `giga` escalation) against pure `giga` mode.

---

## Guardrails for Existing `ejaj` Tests & Clippy Policy

- **Keep `LlmProvider::decode` signature unchanged (`src/llm/mod.rs:107`):**
  - Attach `#[serde(skip)] pub usage: Option<TokenUsage>` directly onto `crate::api::Message` (`src/api.rs:24-41`), mirroring `#[serde(skip)] pub truncated: bool`.
  - Because `#[serde(skip)]` excludes `usage` from `serde_json` serialization and deserialization:
    - Outgoing provider requests (`ChatCompletionRequest`, `ResponsesProvider::prepare`, `AnthropicProvider::prepare`) never send `usage` on the wire.
    - `ChatCompletionRequest::context_len()` (`src/api.rs:154-158`) is unaffected.
    - `Message` deserialization on `Choice.message` in `ChatCompletionResponse` (`src/api.rs:169`) defaults `usage` to `None` before `ChatProvider::decode` populates it from the top-level response `usage`.
    - `trajectories/*.traj.json` message arrays keep their exact wire shape.
- **Initialize `usage: None` in every `Message { ... }` struct literal across the crate:**
  - Production literals:
    - `src/api.rs:59` (`Message::text`) and `src/api.rs:72` (`Message::tool`)
    - `src/llm/anthropic.rs:189` (`AnthropicProvider::decode`) and `src/llm/anthropic.rs:205` (`empty_assistant`)
    - `src/llm/responses_api.rs:185` (`ResponsesProvider::decode`)
    - `src/loops/agent.rs:1984` (`request_handover_summary` — carry over `usage: message.usage` from the decoded handover summary `message`)
  - Unit-test literals (`usage: None`):
    - `src/llm/anthropic.rs:584`
    - `src/llm/responses_api.rs:398, 427, 512`
    - `src/loops/plan.rs:268`
    - `src/loops/summary.rs:1223, 1249, 1258, 1267, 1276`
    - `src/loops/agent.rs:2514, 2536, 2598, 7248, 7288, 7451, 9085, 9099`
- **Tolerant `usage` decoding — never fail a response because of malformed `usage`:**
  - Existing fixtures and `ScriptedTransport` responses in `tests/` omit `"usage"` (or may carry partial/null/non-integer metadata). Absent, `null`, negative, fractional, string, or non-object `usage` fields must never produce `CallError::Transport` or `CallError::Malformed`; they simply leave `message.usage == None` (or `0` for an individual sub-counter when other counters are valid).
- **Preserve `status::thinking_label` and input-price error messages byte-for-byte (`src/status.rs:133-160`, `src/llm/factory.rs:117-127`, `src/config.rs:1495-1610`):**
  - Do **not** alter `status::thinking_label` (it estimates pre-send input cost before the response exists).
  - Keep the exact error message for invalid `LLM_*_INPUT_PRICE_PER_MILLION_USD`:
    `"{variable} must be a finite, non-negative number of USD per million input tokens."`
- **Enforce Clippy complexity and lint gates (`Cargo.toml`, `clippy.toml`, `tests/guidance.rs`):**
  - Every function must stay at or below `cognitive-complexity-threshold = 15`.
  - Never trigger `collapsible_if`, `collapsible_match`, `collapsible_else_if`, `match_bool`, or `manual_let_else` (all denied/warned under `cargo clippy --all-targets -- -D warnings`). Decompose wire-specific usage extractors into small flat helpers using `let Some(...) = ... else { return None; };`.
- **Enforce caller-intent test-title contract (`tests/test_titles.rs`):**
  - Every test in `src/llm/factory.rs`, `src/llm/anthropic.rs`, and `src/config.rs` (`when_configuring_*`) is checked by `tests/test_titles.rs`!
  - Every new `#[test]` across all touched files must follow `when_<action>_given_<context>_then_<outcome>` (lowercase snake_case, non-empty segments, single `_then_`) and must **never** contain any forbidden substring in `INTERNAL_IDENTIFIERS` (`mode_from_value`, `mode_from_raw`, `missing_giga_variable`, `giga_requirement`, `chat_provider`, `responses_provider`, `mid_from_values`, `giga_from_values`, `mid_from_env`, `giga_from_env`, `thinking_label`, `group_digits`, `provider_role`, `summarize`, `anthropic_provider`, `native_tools`, `tool_call_from_block`, `nested_error_message`, `call_time`, `measured`) or `CONFIGURATION_METHOD_IDENTIFIERS` (`new`, `with_giga`).
- **Scope tracker updates strictly to `TODO.md`:**
  - Mark `T29` `[x]` in `TODO.md` right before running the final verification gate; do not edit `.plans/PROPOSAL-25-provider-token-usage-and-pricing-telemetry.md`.

---

## Proposed Design

### 1. Shared `TokenUsage` Type & Cost Estimator (`src/api.rs`)

1. Add `TokenUsage` in `src/api.rs`:
   ```rust
   /// Provider-reported token counts for one completed model response, or an
   /// accumulated sum across multiple turns.
   #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
   pub struct TokenUsage {
       /// Total input/prompt tokens billed or processed for the request
       /// (cached tokens included).
       pub input_tokens: u64,
       /// Subset of `input_tokens` served from the provider's prompt cache.
       pub cached_input_tokens: u64,
       /// Total output/completion tokens generated (reasoning tokens included
       /// where the wire includes them in output).
       pub output_tokens: u64,
       /// Reasoning/thinking tokens reported separately by the provider, when
       /// present.
       pub reasoning_tokens: u64,
   }
   ```
2. Implement pure saturation and pricing helpers on `TokenUsage`:
   - `pub fn add_assign(&mut self, other: TokenUsage)`: saturating addition (`saturating_add`) on all four `u64` fields.
   - `pub fn uncached_input_tokens(&self) -> u64`: `self.input_tokens.saturating_sub(self.cached_input_tokens)`.
   - `pub fn estimate_cost_usd(&self, input_rate: Option<f64>, cached_input_rate: Option<f64>, output_rate: Option<f64>) -> Option<f64>`:
     - Returns `None` when both `input_rate` and `output_rate` are `None` (an unpriced provider stays `None` rather than `$0.00`).
     - When priced:
       - Uncached input tokens (`self.uncached_input_tokens()`) use `input_rate.unwrap_or(0.0)`.
       - Cached input tokens (`self.cached_input_tokens`) use `cached_input_rate.or(input_rate).unwrap_or(0.0)` (falls back to `input_rate` when no separate cached-input discount rate is configured).
       - Output tokens (`self.output_tokens`) use `output_rate.unwrap_or(0.0)`.
       - Divides the weighted token sum by `1_000_000.0`.
3. Add `#[serde(skip)] pub usage: Option<TokenUsage>` to `Message` (`src/api.rs:40`).

### 2. Tolerant Wire-Format `usage` Extraction (`src/llm/chat_completions.rs`, `src/llm/responses_api.rs`, `src/llm/anthropic.rs`)

Use raw `Option<serde_json::Value>` for `usage` on wire structs so malformed `usage` JSON never fails response deserialization. Define a small helper `u64_field(object: &Map<String, Value>, key: &str) -> Option<u64>` (`object.get(key).and_then(Value::as_u64)`):

1. **Chat Completions (`src/llm/chat_completions.rs` & `src/api.rs`):**
   - Add `#[serde(default)] pub usage: Option<serde_json::Value>` to `ChatCompletionResponse` (`src/api.rs:163-167`).
   - Extract `decode_chat_usage(raw_usage: Option<&serde_json::Value>) -> Option<TokenUsage>`:
     - Require `let Some(obj) = raw_usage.and_then(Value::as_object) else { return None; };`.
     - `prompt = u64_field(obj, "prompt_tokens")`
     - `completion = u64_field(obj, "completion_tokens")`
     - `cached = obj.get("prompt_tokens_details").and_then(Value::as_object).and_then(|d| u64_field(d, "cached_tokens")).or_else(|| u64_field(obj, "prompt_cache_hit_tokens"))` (supports both OpenAI `prompt_tokens_details.cached_tokens` and DeepSeek `prompt_cache_hit_tokens`).
     - `reasoning = obj.get("completion_tokens_details").and_then(Value::as_object).and_then(|d| u64_field(d, "reasoning_tokens"))`.
     - If `prompt.is_none() && completion.is_none() && cached.is_none() && reasoning.is_none()`, return `None`.
     - Otherwise return `Some(TokenUsage { input_tokens: prompt.unwrap_or(0), cached_input_tokens: cached.unwrap_or(0), output_tokens: completion.unwrap_or(0), reasoning_tokens: reasoning.unwrap_or(0) })`.
   - Assign `message.usage = decode_chat_usage(response.usage.as_ref());` in `ChatProvider::decode` (`src/llm/chat_completions.rs:120`).
2. **Responses API (`src/llm/responses_api.rs`):**
   - Add `#[serde(default)] usage: Option<Value>` to `ResponsesResponse` (`src/llm/responses_api.rs:28-36`).
   - Extract `decode_responses_usage(raw_usage: Option<&Value>) -> Option<TokenUsage>`:
     - Require `let Some(obj) = raw_usage.and_then(Value::as_object) else { return None; };`.
     - `input = u64_field(obj, "input_tokens")`
     - `output = u64_field(obj, "output_tokens")`
     - `cached = obj.get("input_tokens_details").and_then(Value::as_object).and_then(|d| u64_field(d, "cached_tokens"))`
     - `reasoning = obj.get("output_tokens_details").and_then(Value::as_object).and_then(|d| u64_field(d, "reasoning_tokens"))`
     - Return `None` if all four are `None`; otherwise return `Some(TokenUsage { ... })` with `unwrap_or(0)` defaults.
   - Set `usage: decode_responses_usage(response.usage.as_ref())` on the returned `Message` in `ResponsesProvider::decode` (`src/llm/responses_api.rs:185`), including when `truncated` returns an empty `output` vector (`src/llm/responses_api.rs:146-149`).
3. **Anthropic Messages (`src/llm/anthropic.rs`):**
   - Extract `decode_anthropic_usage(object: &Map<String, Value>) -> Option<TokenUsage>` from `object.get("usage")`:
     - Require `let Some(obj) = object.get("usage").and_then(Value::as_object) else { return None; };`.
     - Read `uncached = u64_field(obj, "input_tokens")`, `cache_read = u64_field(obj, "cache_read_input_tokens")`, `cache_write = u64_field(obj, "cache_creation_input_tokens")`, `output = u64_field(obj, "output_tokens")`.
     - Return `None` if all four are `None`.
     - Note Anthropic's native billing definition: `usage.input_tokens` counts only tokens *after* the last cache breakpoint, so total `input_tokens` is `uncached.unwrap_or(0).saturating_add(cache_read.unwrap_or(0)).saturating_add(cache_write.unwrap_or(0))`, `cached_input_tokens` is `cache_read.unwrap_or(0)`, `output_tokens` is `output.unwrap_or(0)`, and `reasoning_tokens` is `0`.
   - Pass `usage` into both `Ok(Message { ..., usage })` (`src/llm/anthropic.rs:189`) and `empty_assistant(true, usage)` (`src/llm/anthropic.rs:155, 183`).

### 3. Optional Output & Cached-Input Price Settings (`src/llm/factory.rs`, `src/config.rs`, `.env.example`)

1. **`ProviderSettings` and `SmollSettings` (`src/llm/factory.rs:32-80`):**
   - Add `pub cached_input_price_per_million_usd: Option<f64>` and `pub output_price_per_million_usd: Option<f64>` to both `ProviderSettings` and `SmollSettings` (defaulting to `None` in `Config::new`, `Config::with_giga`, `SmollSettings::new`, and `settings(...)`).
2. **Environment parsing in `src/llm/factory.rs`:**
   - Generalize `input_price` (`src/llm/factory.rs:117-127`) via a shared helper:
     ```rust
     fn token_price(variable: &str, token_kind: &str, raw: Option<String>) -> Result<Option<f64>, String> {
         let Some(raw) = non_blank(raw) else {
             return Ok(None);
         };
         match raw.trim().parse::<f64>() {
             Ok(rate) if rate.is_finite() && rate >= 0.0 => Ok(Some(rate)),
             _ => Err(format!(
                 "{variable} must be a finite, non-negative number of USD per million {token_kind} tokens."
             )),
         }
     }
     ```
     Call `token_price(var, "input", ...)` for `LLM_{MID,GIGA,SMOLL}_INPUT_PRICE_PER_MILLION_USD`, `token_price(var, "cached input", ...)` for `LLM_{MID,GIGA,SMOLL}_CACHED_INPUT_PRICE_PER_MILLION_USD`, and `token_price(var, "output", ...)` for `LLM_{MID,GIGA,SMOLL}_OUTPUT_PRICE_PER_MILLION_USD` in `mid_from_environment`, `giga_from_environment`, and `smoll_from_environment`.
   - Note: in `giga_from_environment` (`src/llm/factory.rs:242-256`), validate all three `LLM_GIGA_*_PRICE_PER_MILLION_USD` variables **before** the `let Some(mut resolved) = giga_from_values(...) else { return Ok(None); };` early return, matching how `LLM_GIGA_INPUT_PRICE_PER_MILLION_USD` is validated today!
3. **Phase-routed rate lookups on `Config` (`src/config.rs:284-298`):**
   - Alongside `pub fn input_price(&self, phase: tools::Phase) -> Option<f64>`, add `pub fn cached_input_price(&self, phase: tools::Phase) -> Option<f64>` and `pub fn output_price(&self, phase: tools::Phase) -> Option<f64>` using `match self.provider_role(phase)` (`"smoll"`, `"mid"`, `"giga"`).
4. **Commented examples in `.env.example`:**
   - Add commented `# LLM_{MID,GIGA,SMOLL}_CACHED_INPUT_PRICE_PER_MILLION_USD=...` and `# LLM_{MID,GIGA,SMOLL}_OUTPUT_PRICE_PER_MILLION_USD=...` lines below each role's existing `# LLM_*_INPUT_PRICE_PER_MILLION_USD` comment (never adding key-shaped strings).

### 4. Test Coverage

Add caller-intent (`when_<action>_given_<context>_then_<outcome>`) unit tests:
- `src/api.rs`: `TokenUsage::add_assign` saturation, `uncached_input_tokens`, and `estimate_cost_usd` (unpriced `None`, input+output only with cached fallback to input rate, explicit discounted cached-input rate, and zero usage).
- `src/llm/chat_completions.rs`: decoding OpenAI `prompt_tokens_details.cached_tokens` + `completion_tokens_details.reasoning_tokens`, DeepSeek `prompt_cache_hit_tokens`, and tolerating absent/null/malformed `usage` without failing `decode`.
- `src/llm/responses_api.rs`: decoding `input_tokens`, `input_tokens_details.cached_tokens`, `output_tokens`, `output_tokens_details.reasoning_tokens`, and tolerating absent/malformed `usage`.
- `src/llm/anthropic.rs`: summing `input_tokens + cache_read_input_tokens + cache_creation_input_tokens` into `input_tokens` with `cached_input_tokens = cache_read_input_tokens`, preserving usage on `max_tokens` empty truncated turns, and tolerating absent/malformed `usage`.
- `src/llm/factory.rs` & `src/config.rs`: resolving `LLM_{MID,GIGA,SMOLL}_{CACHED_INPUT,OUTPUT}_PRICE_PER_MILLION_USD` per phase across `mid`, `giga`, `hybrid` (and `code_retry_config` escalating `Phase::Code` rates to `giga`), plus rejecting negative/`NaN`/`inf`/non-numeric values with the variable name in the error.

---

## Affected Files

- `src/api.rs`
- `src/llm/chat_completions.rs`
- `src/llm/responses_api.rs`
- `src/llm/anthropic.rs`
- `src/llm/factory.rs`
- `src/config.rs`
- `src/loops/agent.rs`
- `src/loops/plan.rs`
- `src/loops/summary.rs`
- `.env.example`
- `TODO.md`

## Verification

```bash
cargo test --lib api && cargo test --lib llm && cargo test --lib config && cargo test --test config_cli && cargo test --test secret_guard && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
