# Reasoning replay inflates prompts on OpenAI-compatible providers

## Summary

Gray unconditionally replays assistant `reasoning_content` back into every
subsequent request on the OpenAI-compatible chat path. Combined with a
`high` → 16,384-token thinking budget, a single first-turn reasoning blob
is re-billed as input on every turn for the rest of the session.

Measured on a trivial task (`openssl-selfsigned-cert`): first turn emits
~15.9k reasoning tokens; subsequent turns show input jumping 911 → 18,633 →
19,377 → 20,889 — the reasoning is being re-sent as prompt tokens even though
the provider (z.ai/GLM) ignores `reasoning_content` on input but still bills it.

## Evidence

Same task, same model, different harnesses:

| harness          | wall  | total reasoning tokens | turn-2 input |
|------------------|-------|------------------------|--------------|
| gray (high)      | 239s  | 16,239                 | 18,633       |
| gray (low)       | ~13s  | 0                      | 1,230        |
| gray v2          | 84s   | 5,003                  | 1,577        |
| claude-code      | 136s  | ~600                   | —            |
| hermes           | 178s  | 3,580                  | —            |
| codex            | 122s† | 258                    | —            |

† failed task; token count still illustrative.

Long-session profile (gray, high): one failed task accumulated 52 turns /
7.6M input tokens because reasoning + tool output compound each turn.

## Root cause

`crates/gray-provider/src/openai.rs` — assistant-message serialization:

```rust
let reasoning = if thinking_parts.is_empty() {
    caps.replay_reasoning_content.then(String::new)   // flag respected
} else {
    Some(thinking_parts.join("\n"))                    // flag ignored
};
```

`caps.replay_reasoning_content` exists precisely to gate this
(currently `is_deepseek_model(model)`), but the non-empty branch bypasses it,
so GLM/Qwen/etc. get replay too.

## Fix

Gate the non-empty branch on the same flag:

```rust
} else {
    caps.replay_reasoning_content.then(|| thinking_parts.join("\n"))
}
```

Keep thinking in internal transcript state; only omit it from the wire
payload. Anthropic keeps its own replay path (required for interleaved
thinking signatures); DeepSeek keeps the flag.

## Test

- GLM/z.ai model: turn 2+ request must not contain `reasoning_content`.
- DeepSeek model: replay still present.
- Cache-warm path unchanged (warm replay is separately gated).

## Not in scope

- Re-tuning the `chat_thinking_budget` table (`high` = 16,384 is 4–40x what
  comparable harnesses emit at "high") — separate decision, measure first.
- Context compaction / step limits — out of scope for this fix.
