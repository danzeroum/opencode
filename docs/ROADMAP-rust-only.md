# Roadmap — Backend Rust-Only (GUI web)

> Documento vivo. Objetivo: tornar o backend **100% Rust** (deletar o servidor TS `packages/server` + `packages/core`) servindo a **GUI web** (`packages/app` + `ui`, SolidJS) via o contrato OpenAPI existente. Atualizado a cada fatia mergeada.
>
> **Última atualização:** 2026-06-19 · **Foco atual:** superfície de **leitura lean coberta** (rotas GET sobre dado existente); restam **épicos** (ver "Estado & próximos épicos").
>
> **Rotas nativas contrato-enforçadas: 23** · PRs desta rodada autônoma: #69–#78.

## Como trabalho
- Uma **fatia por PR**, contrato-enforçado (`xtask openapi-diff`), `cargo test` + `fmt` + `clippy -D warnings` verdes antes de mergear.
- Merge na `rust-migration` a cada fatia. Branch de dev: `claude/affectionate-davinci-05ifmg`.
- Decisões/bloqueios que precisam do humano → `docs/PENDENCIAS.md` (não paro; sigo pra próxima fatia tratável).

## Legenda
✅ feito · 🔄 em andamento · ⏳ pendente · ⛔ bloqueado (ver PENDENCIAS) · ➖ fora do escopo web-only

---

## Já entregue antes deste roadmap (contexto)
- ✅ Fases 1–5 base: event store, DB pool, migration-verify, runner spike (Phase 4), proxy strangler-fig.
- ✅ Trilha provider/model (14 PRs, #56–#68): tipos V2, transform `catalog_v2` (incl. variants), catálogo no `AppContext`, população + refresh em background, gating `available()` (env + **credential store** completo), rotas `model.list`/`provider.list`/`provider.get`/`location.get`.

---

## Fase 0 — Fundação backend
- ⏳ **Schema ownership**: Rust passa a **aplicar** migrações (hoje só verifica). Bloqueador pra desligar o TS. (ver PENDENCIAS #4)

## Fase 1 — Session core (chat) 🔄
- ✅ 1a — proto `SessionMessage` (união 8 variantes + content + tool-state) — **#69**
- ✅ 1b — read-store `session_message` (seq-window + cursor) — **#70**
- ✅ 1c — rota `GET /api/session/{id}/message` (cursor, 404, reconstrução) — **#71**
- ✅ 1d — **write-path**: runner projeta o turno → `session_message` (dados reais no chat!) — **#80 append, #81 projector, #82 wiring**
- ⏳ 1e — `session.update` (metadata: title, etc.)
- ⏳ 1f — `session.revert` / `session.unrevert`
- ⏳ 1g — `session.share` / `session.unshare`
- ⏳ 1h — `session.command`
- ⏳ 1i — `session.diff`
- ⏳ 1j — `session.status`
- ⏳ 1k — `session.summarize`
- ✅ 1l — `session.todo` (read store `todo` + rota `GET /session/{id}/todo`) — **#73**
- ✅ 1o — `global.dispose` + `instance.dispose` (lifecycle ack, 200 `true`) — **#74**
- ✅ 1r — `file.list` (`GET /file`, listagem de diretório com flag gitignore) — **#76**
- ✅ 1s — `permission.list` + `question.list` (vazios até a engine) — **#77**
- ✅ 1t — `vcs.get` (`GET /vcs`, branch/default_branch via git best-effort) — **#78**
- ⏳ 1p — `session.status` / `session.diff` — **dependem da engine de execução** (estado live / snapshots), não enxutas
- 🔄 1q — mutações de sessão (tipo `Session` v1 #83; `SessionStore::get_full`+`update` aditivos #85):
  - ✅ `session.update` (`PATCH /session/{id}`) — **#85**
  - ✅ `session.revert` / `session.unrevert` (set/clear revert pointer) — **#86**
  - ⏳ `share`/`unshare` — precisa do **serviço externo de URL** (não é só projeção); ver nota
  - ⏳ `command`/`summarize` — usam a execução (write-path pronto)
- 🔄 1m — `permission.list` ✅ (#77, vazio até a engine produzir) · `permission.respond` ⏳ (precisa engine)
- 🔄 1n — `question.list` ✅ (#77, idem) · `question.reply`/`reject` ⏳ (precisa engine)

## Fase 2 — Admin
- ⏳ 2a — `config.get` / `config.update`
- ⏳ 2b — **config com proveniência** (cascata 7 níveis) — capacidade nova (ver PENDENCIAS #1)
- ⏳ 2c — `provider.list` admin + `provider.auth` (escrita de credencial) + teste de conexão
- 🔄 2d — agents: `app.agents` rota wirada + tipos `Agent`/`AgentModel` (#87, retorna `[]`); falta **loading** (built-in + `.opencode/agents/*.md`) + escrita (PENDENCIAS #2)
- 🔄 2e — commands: `command.list` rota wirada + tipo `Command` (#87, retorna `[]`); falta **loading** (built-in + `.opencode/command/*.md` + MCP/skills) + escrita
- ⏳ 2f — políticas (permission rules)
- ⏳ 2g — snapshots timeline (`session.revert` ligado à UI)
- ⏳ 2h — shared list
- ⏳ 2i — aparência (`config.theme` persistência) — UI-driven, backend mínimo

## Fase 3 — Extensões (greenfield em Rust)
- ⏳ 3a — plugin host JS out-of-process (RPC/JSON, Bun) + ciclo de hooks
- ⏳ 3b — MCP via `rmcp` (add/connect/status + reconnect)
- ⏳ 3c — integrações in-core (GitHub/GitLab/Slack adapters)

## Fase 4 — Cutover Rust-only
- ⏳ 4a — cobertura de providers (execução LLM real) — ver PENDENCIAS #5
- ⏳ 4b — auth-write / OAuth backend completo
- ⏳ 4c — habilitar grupos nativos por padrão + **remover proxy/upstream**
- ⏳ 4d — deletar `packages/server` + `packages/core` TS

## Fora do escopo web-only ➖
- ➖ `tui.*`, `pty.*` (exceto stub `pty.remove`), `sync.*`, maior parte de `experimental.*`, `mcp.auth` avançado, `formatter`, `lsp.*` além de status, `vcs.*` além de get, billing/login.

## Frontend (parallel track — não é "backend Rust")
- ⏳ Recriar o design handoff (`docs/design_handoff_opencode_web`) em SolidJS. Track separado; ver PENDENCIAS #3.

---

## Estado & próximos épicos (para a próxima sessão)

**Feito nesta rodada autônoma (mergeado):** #69 tipos, #70 store, #71 rota `messages`, #72 docs, #73 `session.todo`, #74 `dispose`, #76 `file.list`, #77 `permission.list`+`question.list`, #78 `vcs.get`. As **fatias enxutas** (read sobre dado existente) estão **cobertas**. Restantes precisam de: type-modeling grande (Config/agents), escrita+coexistência (auth), subsistema greenfield (mcp/lsp), ou a engine de execução (mutações/status/diff).

O que resta são **épicos** — cada um é multi-fatia e merece uma sessão focada com contexto cheio. Sequência recomendada (maior valor primeiro):

1. **Write-path / projector** (PENDENCIAS #6) — *linchpin do chat real*. Plano de fatias:
   - 1a. Portar `projector.ts` como **função pura** em `opencode-core` (eventos → linhas `session_message`), com testes — **contrato-neutro, mergeável sozinho**. Começar pelo subconjunto user+assistant message.
   - 1b. Runner emite eventos de sessão no event store durante a execução do turno.
   - 1c. Ligar projector ao stream de eventos → grava `session_message` (+ `message`/`part`).
   - Resultado: `messages` passa a retornar dados reais → **chat funciona**.
2. **Config** (Fase 2a/2b) — modelar o tipo `Config` (35 props de topo, ~19 tipos no closure) + `config.get`/`update`. Grande, mas tratável (sem engine). Proveniência (cascata) depende de PENDENCIAS #1.
3. **agents/commands** — `app.agents`/`command.list` (parse de `.opencode/*.md` + defaults) e escrita (PENDENCIAS #2).
4. **Mutações de sessão** (`update/revert/share/command/summarize`) — dependem do write-path (épico 1).
5. **`session.status`/`diff`** — dependem do estado de execução / snapshots.
6. **Extensões** (Fase 3) — plugin host + MCP `rmcp` + integrações (greenfield, grande).
7. **Cutover** (Fase 4) — schema-apply (PENDENCIAS #4), cobertura de providers (#5), remover proxy, deletar TS.

**Nota de integridade:** não meio-implemento épicos para "parecer pronto" — um projector que compila mas não bate o comportamento do TS seria pior que não-feito. Cada épico será portado fielmente e testado.
