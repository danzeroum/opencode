# Roadmap: optimizing the agent runtime for smaller-capability LLMs

> Durable plan + status. GitHub Issues are disabled on this repo, so this file is the
> single source of truth — keep it updated as items land so work can resume without
> re-deriving context.

## Goal

Let a smaller / cheaper model (Claude Haiku, GPT-4o-mini, Gemini Flash, local 7–8B) run
the agent with quality closer to a frontier model, by scaling the per-turn context
footprint and scaffolding to the model's **capability** instead of treating every model
like Opus.

Two runtimes are relevant:

- **V1 (active today):** `packages/opencode/src/session`
- **V2 (emerging):** `packages/core` System-Context runtime (`system-context/`, `instruction-context.ts`, `session/runner`)

## Foundation

`ModelTier` (`packages/opencode/src/provider/model-tier.ts`) classifies a model as
`small` / `medium` / `large` from its id. **Everything below is gated on it**, so
medium/large behavior is unchanged.

## Done

| PR | Merge | Summary |
| --- | --- | --- |
| #5 | `f6d9df2` | ModelTier; lean prompt (`session/prompt/small.txt`) + terse skill list + reduced tools (drop `task`/`lsp`) for small models; fractional `compaction.threshold` + earlier default trigger; `grep` `output_mode`; optional `lsp` `line`/`character`; head+tail compaction truncation; constraint-preserving summary prompt; per-component `context.budget` debug log |
| #6 | `55e8208` | small-tier compaction tuning (fewer verbatim turns + tighter preserve budget); web-fetch `TurndownService` singleton |
| #7 | `d5155b0` | prompt-cache breakpoints for `openrouter` + `github-copilot` (generic `openai-compatible` excluded); transform contract tests updated |
| #9 | `0b8dc7b` | **R1** — per-turn, high-salience step-discipline reminder for small models (`session/reminders.ts` + `small-steps.txt`) |
| #10 | `414aa1c` | **R2** — cap oversized instruction files for small models (`capInstruction` in `session/instruction.ts`) |

## Remaining

> R1 (#9) and R2 (#10) are done — see the table above. R3 and R4 are the open items.

### R3 — Real tokenizer — BLOCKED (environment)
Goal: replace the `chars/4` estimate (`core/src/util/token.ts`) with a real BPE tokenizer for
accurate compaction/overflow/budget decisions, keeping `chars/4` as a fallback.

Status: **blocked in the web sandbox.** `bun add js-tiktoken` re-resolves the 27-package workspace
and times out (>9 min) on every attempt, so the dependency cannot be installed here and importing it
would break CI. This is purely an environment limit — land it wherever `bun install` works (local/CI).
Ready change for `core/src/util/token.ts` (plus `bun add js-tiktoken` in `packages/core`):

```ts
import { getEncoding, type Tiktoken } from "js-tiktoken"
const CHARS_PER_TOKEN = 4
let enc: Tiktoken | null | undefined
function tokenizer() {
  if (enc === undefined) try { enc = getEncoding("o200k_base") } catch { enc = null }
  return enc
}
export const estimate = (input: string) => {
  const t = tokenizer()
  if (t) try { return t.encode(input).length } catch {}
  return Math.max(0, Math.round(input.length / CHARS_PER_TOKEN))
}
```

### R4 — Wire ModelTier into the V2 runtime — follow-up (structural)
Mirror small-tier prompt/skills/tools/compaction gating into `packages/core`. **Not a clean insertion:**
the V2 runner (`session/runner/llm.ts`) loads the System Context (`systemContext.load()`,
`skillGuidance.load(agent)`, `referenceGuidance.load()`) *before* it resolves the model
(`models.resolve(session)`), and materializes tools by permission, not by model. Tier-gating therefore
needs the model resolved earlier and threaded into those producers + tool materialization — a structural
change that must respect the Context-Epoch / Safe-Provider-Turn-Boundary invariants in `CONTEXT.md`.
The one already-model-aware seam is V2 compaction (`compaction.compactIfNeeded({ model, ... })`), where
the V1 fractional/earlier-threshold gating can be mirrored cleanly first. V2 is not the active runtime today.

## Out of scope (deliberate — would be regressions)

- Discounting cache tokens in overflow detection — cached tokens still occupy the context window.
- Summing per-step token counts — per-step input overlaps heavily; the last step already reflects window size.
- Caching arbitrary `openai-compatible` endpoints — some reject `cache_control`.

## Working agreement

- Each item ships as its own PR → CI green → squash-merge → next.
- Everything tier-gated; medium/large behavior must not change.
- Verify locally (`tsgo --noEmit` + affected test files) before pushing; the `unit` job runs the full suite.
