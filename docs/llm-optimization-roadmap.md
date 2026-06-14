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

## Remaining

### R1 — Code-side scaffolding / stepping for small models
Inject a tier-gated "operating procedure" reminder (plan → act one step → verify) and
progress nudges, reusing `session/reminders.ts`. Optional follow-ups: dependency-aware
tool-call ordering, doom-loop delegation hint.
Files: `session/reminders.ts`, `session/prompt/*.txt`.

### R2 — Relevance / section-based instruction loading
`session/instruction.ts#system()` injects the full `AGENTS.md` / `CLAUDE.md` every turn.
Add opt-in section selection (split on `##`, keep sections matching touched paths /
prompt keywords) and/or a size cap with summary fallback, gated by config + tier.
Files: `session/instruction.ts`, V2 `core/src/instruction-context.ts`.

### R3 — Real tokenizer
Replace the `chars/4` estimate (`core/src/util/token.ts`) with a real tokenizer
(js-tiktoken `o200k_base` / `cl100k_base`) for accurate compaction/overflow/budget
decisions; keep `chars/4` as fallback. Adds a dependency.

### R4 — Wire ModelTier into the V2 runtime
Mirror the small-tier prompt/skills/tools/compaction gating into `packages/core`
(System-Context registry, `skill/guidance.ts`, runner) so tiering applies once V2
becomes the active path.

## Out of scope (deliberate — would be regressions)

- Discounting cache tokens in overflow detection — cached tokens still occupy the context window.
- Summing per-step token counts — per-step input overlaps heavily; the last step already reflects window size.
- Caching arbitrary `openai-compatible` endpoints — some reject `cache_control`.

## Working agreement

- Each item ships as its own PR → CI green → squash-merge → next.
- Everything tier-gated; medium/large behavior must not change.
- Verify locally (`tsgo --noEmit` + affected test files) before pushing; the `unit` job runs the full suite.
