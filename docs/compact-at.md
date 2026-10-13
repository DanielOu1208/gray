# Compaction ceilings: `--compact-at`, `GRAY_COMPACT_AT`, `compact_at_tokens`

Gray auto-compacts before the request that would overflow the context
window (`window − reserve`). Some providers also bill a higher price tier
once a prompt crosses a declared threshold — Claude Haiku 5.5 bills the
whole request at the over-tier rate past 100,000 prompt tokens, per its
public pricing page. Gray therefore supports a second trigger, the
**compaction ceiling**, which fires earlier than the window bound.

## Where the ceiling comes from

Precedence, first hit wins:

1. `--compact-at <N>` on `gray` / `gray -p`
2. `GRAY_COMPACT_AT=<N>` in the environment (the knob a supervisor sets on a
   `gray -p` child)
3. `"compact_at_tokens": <N>` in `~/.gray/config.json`
4. `auto` — the default

`<N>` accepts `90000`, `90k`, `1.5m` (the `--context-window` grammar), or
the literal word `auto` in config.json / env / flag.

`auto` means: the ceiling is derived from the model's declared **price
cliff**, when its provider plugin publishes one. An explicit value is the
user's choice — it may sit above the cliff (e.g. `--compact-at 1m` on a
100k-cliff model) — but it always clamps to `window − reserve`.

When the model declares a cliff, `auto` computes
`ceiling = cliff − max(cliff/10, reserve)` — e.g. a 100k cliff with a
12.5k reserve fires at ~87.5k, so the compaction call and the request
after it stay under the cliff. After compaction, the retained tail is
capped at `ceiling/4` so the next trigger stays far away.

## Where the cliff comes from

Provider plugins declare `price_cliff_tokens` per model row in
`provider/models` (optional, additive to the protocol). gray-claude-sub
derives it from Anthropic's public pricing page — the
`"(for prompts over N tokens)"` table rows — fetched alongside the models
page, cached on disk the same way, overridable via
`CLAUDE_SUB_PRICING_URL`. No credentials are sent. A torn download — a
table row shorter than its header — reads as a failed fetch: last-good
data stands rather than a bogus partial result overwriting it. If the page,
the parse, or the name→id mapping fails, the row carries no cliff and
auto-compaction stays on the window bound — never a guessed threshold.

## What you see

- A compaction notice names its reason: `↻ compacted 90k → 23k tok
  (… ) · price cliff 100k`, `· --compact-at`, or `· overflow`. The window
  bound still reads as before.
- `gray -p --json` emits one startup row
  `{"type":"progress","phase":"compact_ceiling","compact_ceiling":N}`
  (plus `"price_cliff"` when declared) so a supervisor can verify what
  took. The same number lands in `~/.gray/running/<pid>.json`.
- REPL auto-compaction (`maybe_threshold_compact`) and in-loop compaction
  (which is what `gray -p` runs) both honor the same ceiling — set on the
  agent at build time and read back by the threshold check.
