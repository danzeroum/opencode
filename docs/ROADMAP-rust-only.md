# Roadmap — Backend Rust-Only (GUI web)

> Documento vivo. Objetivo: tornar o backend **100% Rust** (deletar o servidor TS `packages/server` + `packages/core`) servindo a **GUI web** (`packages/app` + `ui`, SolidJS) via o contrato OpenAPI existente. Atualizado a cada fatia mergeada.
>
> **Última atualização:** 2026-06-27 · **Decisão do dono:** **cutover TOTAL (alvo B)**. **Cobertura: 168/168 (100%) nativo, gate ESTRITO verde.** **Cutover Tier 8 — parte reversível FEITA:** o default agora é **Rust-only** (`OPENCODE_RUST_ROUTES` unset ⇒ todos os grupos nativos; sem upstream TS). O binário roda 100% em Rust por padrão; `=none` volta a proxiar (escape hatch) e um subset fica híbrido. **Falta a parte IRREVERSÍVEL** (deletar `packages/{server,core,llm}` ~414 arquivos + reapontar npm/publish p/ o binário cargo-dist + desligar CI do TS) — ⚠️ **aguarda confirmação explícita do dono** (a ferramenta de pergunta falhou 2×; descrevi as opções em texto no chat).
>

## Plano de cutover total (alvo B) — ordem de execução

> Ordenado por facilidade de desenvolvimento + dependências (fundações antes de dependentes, p/ não retrabalhar). ~95 ops em ~20 fatias batched. Marco a cada PR mergeado.

**Tier 1 — Reads triviais (dado já existe, sem subsistema novo):**
- ✅ T1.1 `tool.list` + `tool.ids` (`/experimental/tool[/ids]`, reusa `tool_definitions()`) — **#143**
- ✅ T1.2 `app.skills` (`/skill`), `formatter.status` (`/formatter`, vazio até config de formatters), `event.subscribe` (V1 `/event`, SSE reusa o bus) — **#144**
- ⏳ T1.3 `provider.list` (`/provider`) + `provider.auth` (`/provider/auth`) — precisam do fecho `Model` (catálogo) + `ProviderAuthMethod` (união de prompts); movido p/ junto do épico de providers

**Tier 2 — Git/VCS (git via binário, reusa `git_raw`/`parse_file_status`):**
- ✅ T2 `vcs.status`/`vcs.diff`/`vcs.diff.raw`/`vcs.apply` (status/numstat→`VcsFileStatus`, per-file patch→`VcsFileDiff`, raw `git diff` SSE-like, `git apply` stdin) — **#145**. `project.initGit` → junto de `project.update` (T3.1, grupo project).

**Tier 3 — CRUD sobre stores existentes:**
- ✅ T3.1 `project.update` (PATCH project) + `project.initGit` (`git init` + marca initialized) — **#146**. `ProjectStore` ganhou `get`/`put`.
- ✅ T3.2 `session.deleteMessage` (real — `SessionMessageStore::delete`) + `part.update`/`part.delete` (wired-404: parts são projetados read-time, sem store V1 dedicado — ver PENDENCIAS #7) — **#147**
- ✅ T3.3 `v2.session.create` (POST /api/session → `{data: SessionV2Info}`, reusa store create + `session_record_to_info`) + `v2.session.wait` (204 idle / 404; stub na referência) — **#148**. **Tier 3 completo.**

**Tier 4 — Ações de sessão na engine (reusa Message/Part + `drive_one_turn`):**
- ✅ T4.2 `session.fork` (cria sessão + copia timeline, trunca em `messageID?`) + `session.diff` (working-tree dir diff → `SnapshotFileDiff`, reusa VCS) — **#149**
- ✅ T4.1/4.3 `session.command`/`session.shell` (404/400 — stub na referência; novo wrapper `AssistantMessageWithParts`), `session.summarize` (404/200 false), `v2.session.compact` (404/503) — fiação fiel de contrato — **#150**
- ✅ T4.4 `session.prompt` (V1 sync, POST /session/{id}/message) — **real**: resolve modelo → `drive_one_turn` (history-seeded, persiste) → projeta o último assistant → `{info: AssistantMessage, parts}` — **#151**. **Tier 4 completo.**

**Tier 5 — Auth/credenciais/integrações (épico OAuth):**
- ✅ T5.1 `auth.set`/`auth.remove` (escreve/remove no `auth.json` — inverso do reader do #141; `provider::{set,remove}_auth_entry`) — **#152**
- ✅ T5.2a `provider.list` (`GET /provider`, `{all, default, connected}` — reusa proto `Provider` + `catalog_v2::{available_providers, models}`; `connected` = mesma lógica de enable do v2) — **#153**
- ✅ T5.2b `provider.auth` (`GET /provider/auth`, `{[id]:[ProviderAuthMethod]}`) — modelado o fecho `ProviderAuthMethod`/`AuthPrompt`(text/select)/`AuthSelectOption`/`AuthPromptWhen`; advertise método API-key p/ providers com env (OAuth via plugin = follow-up) — **#154**
- ⏳ T5.2 `v2.credential.*` (tabela `credential`)
- ✅ T5.3a deletes idempotentes (204 sobre stores vazios — fiéis): `v2.permission.saved.remove`, `v2.credential.remove`, `v2.integration.attempt.cancel` — **#155**
- ⏳ T5.3b fluxos OAuth/credencial **reais** (store DB `credential` + máquina de estado): `provider.oauth.authorize/callback`, `v2.credential.update`, `v2.integration.{get,connect.key,connect.oauth,attempt.status,attempt.complete}`

**Tier 5b — OAuth de provider:**
- ✅ `provider.oauth.{authorize,callback}` → 400 fiel `ProviderAuthError1` (`name="ProviderAuthOauthMissing"`). É a resposta **correta** para os providers do dono (deepseek/glm/ollama são API-key/keyless e não suportam OAuth). Modelados `ProviderOauthAuthorization`/`ProviderOauthError`/`ProviderOauthErrorData`/`ProviderOauthRequestError` (prefixo `Oauth` p/ não colidir com o componente `ProviderAuthErrorData` do Part de mensagem — colisão pega pelo `openapi-diff`). — **(este PR)**

**Tier 6 — Subsistemas novos:**
- ✅ T6.1 MCP runtime (`rmcp`) **— superfície de contrato completa (fiel-404)**: `mcp.connect`/`disconnect` — **#160**; e agora `mcp.auth.{start,callback,authenticate,remove}` → 404 `McpServerNotFoundError` (nenhum server MCP configurado nativamente, coerente com connect/disconnect). Modelados `McpAuthStart`/`McpAuthRemoved`/`McpUnsupportedOAuthError`/`McpAuthOAuthError`. Falta a conexão `rmcp` real + `mcp.add` (não estão no contrato OpenAPI atual). — **(este PR)**
- ✅ T6.2 PTY (`portable-pty`) **— grupo completo**: `pty.shells` (enumera `/etc/shells`+`$SHELL`) + `pty.list`/`get`/`remove`/`update` ✅ **#162**; e agora `pty.create` (spawn real: `openpty`+`CommandBuilder`+thread leitora drenando scrollback de 2 MiB), `pty.connect` (upgrade WebSocket — replay por cursor + frame meta `0x00`+JSON + I/O ao vivo bidirecional), `pty.connectToken` (ticket single-use 60 s gated por header `x-opencode-ticket` + Origin) — `PtyManager` global em `crates/opencode-server/src/pty.rs` — **(este PR)**
- ⏳ T6.3 LSP runtime (`lsp.status` real)

**Tier 7 — TUI + sync + experimental:**
- ✅ T7.1 `tui.*` (13 ops) — endpoints de controle: ações acками `200 true`; `control.next` retorna request vazio (sem TUI conectada = web-GUI). Macro `tui_ack!`; 400 por-op (`BadRequestError` vs `RequestError`). — **#156**
- ✅ T7.2/7.3a stubs honestos (empty/204/false) de experimental+sync+worktree+projectCopy (12 ops): `worktree.list/remove/reset`, `experimental.{resource.list,session.background,console.switchOrg,controlPlane.moveSession,workspace.syncList,workspace.warp}`, `v2.projectCopy.remove/refresh`, `sync.start` — modelados os unions de erro `NamedMessageError`/`ProjectCopyError`/`WorkspaceWarpRequestError` — **#157**
- ✅ T7.3b-console `experimental.console.get` (default `ConsoleState`) + `console.listOrgs` (`{orgs:[]}`) — modelados `ConsoleState`/`ConsoleOrg`/`EffectHttpApiInternalServerError` — **#158**
- ✅ T7.3c-reads `experimental.workspace.{list,status,adapter.list}` (empties; modelado `Workspace`+`WorkspaceTimeUsed` union, `WorkspaceStatus`, `WorkspaceAdapter`), `sync.history.list` (empty, `SyncEvent`), `experimental.projectCopy.generateName` (`{name}` derivado) — **#159**
- ✅ T7.3d-session `experimental.session.list` (empty; modelado `GlobalSession` + `GlobalSessionProject` reduzido) — **#161**
- ✅ T7.3e-infra **erros fiéis** para os subsistemas que dependem de **infra hospedada da opencode** (ausente num build self-hosted): `sync.replay`/`sync.steal` → 400 `RequestError` ("sync not available"); `experimental.workspace.remove` → 400 ("workspaces not available", coerente com a lista vazia); `session.share`/`session.unshare` → 404 se a sessão não existe, senão **500** `effect_HttpApiError_InternalServerError` (igual ao TS, que roteia qualquer falha de share para 500). Não são successes falsos — reportam fielmente "não dá aqui". Modelado `SyncSessionResult`. — **(este PR)**
- ⏳ T7.3e restantes (criam/retornam objeto, precisam de subsistema real): `worktree.create`(Worktree), `experimental.workspace.create`(Workspace), `v2.projectCopy.create`(ProjectCopyCopy), `global.upgrade` (união `{success}` — diff de ordem de membros finicky, revisitar)

**Tier 8 — Cutover (Fase 6):**
- ⏳ T8.1 Schema ownership (Rust aplica a migração consolidada)
- ⏳ T8.2 Remover `proxy::proxy_handler` fallback + habilitar todos os grupos
- ⏳ T8.3 Release: `cargo-dist` + reapontar wrapper npm `opencode-ai`; parar de publicar `core/server/llm`; CI TS off
- ⏳ T8.4 Host de plugins JS de terceiros (se necessário)

---
> **Rotas nativas contrato-enforçadas: 68 paths** (várias com múltiplos métodos) · PRs desta rodada autônoma: #69–#139. **Engine**: runner loop + permission gating (#129–#131) + question flow fim-a-fim (#133–#134) + `prompt_async` (#135) + HITL V1 (#136) + `session.init` (#137). **Fundação V1 `Message`/`Part`** (40 tipos) + `session.message` (#138). **Histórico de chat V1 real** — `session.messages`/`session.message` projetam o timeline V2 → V1 `{info,parts}` (é o endpoint que a GUI web usa) (#139). **Continuidade de contexto** — o runner semeia cada turno com o histórico da conversa (o modelo lembra dos turnos anteriores; antes via só o prompt novo) (#140). **Credenciais reais** — `OpencodeCredentials` lê `auth.json`/`OPENCODE_AUTH_CONTENT` (chaves do `opencode auth login`, keyed por providerID) com fallback p/ env, então o LLM real funciona com as chaves que a CLI/GUI já guardam (#141). **Multi-provider OpenAI-compatible** — `ProtocolKind` + `OpenAiCompatibleEngine` servem deepseek/glm/ollama/openai/groq/… (não só Anthropic) (#142). CI `rust` verde (#119).

## Como configurar providers (deepseek, glm, ollama, …)

O backend Rust resolve o provider a partir do prefixo do `model` (`provider/model`). Protocolos: `anthropic` = Messages; **todo o resto = OpenAI Chat Completions-compatible**. baseURL = override env `OPENCODE_<ID>_BASE_URL` → default built-in; key = `auth.json[id]` (via `opencode auth login`) → env var.

| Provider | `model` | Chave | baseURL |
|---|---|---|---|
| **DeepSeek** | `deepseek/deepseek-chat`, `deepseek/deepseek-reasoner` | `DEEPSEEK_API_KEY` (ou `auth.json["deepseek"]`) | built-in `https://api.deepseek.com` |
| **GLM (Zhipu)** | `zhipuai/glm-4.6` ou `glm/glm-4.6` | `ZHIPUAI_API_KEY` (ou `auth.json["zhipuai"]`) | built-in `https://open.bigmodel.cn/api/paas/v4` (China). **Internacional (z.ai):** `OPENCODE_ZHIPUAI_BASE_URL=https://api.z.ai/api/paas/v4` |
| **Ollama externa** | `ollama/llama3`, `ollama/qwen2.5`, … | nenhuma (keyless) | **set `OPENCODE_OLLAMA_BASE_URL=http://SEU_HOST:11434/v1`** |

> Follow-ups (PENDENCIAS #5): lookup de baseURL/env pelo catálogo models.dev (p/ providers sem default built-in) e override via `opencode.json` (`provider.<id>.options.baseURL`). **Loading real**: `command`/`skill`/`agent` lêem `.opencode/**.md`. **Escrita real**: session CRUD (`/session`, `/session/{id}`) persiste no DB (#125–#127). **Engine — permission gating completo**: gate bloqueante `StorePermissionGate` (write-class pede aprovação) + store de pendências + listas + `permission.respond` resolve e acorda o run (#129–#130).

## Como trabalho
- Uma **fatia por PR**, contrato-enforçado (`xtask openapi-diff`), `cargo test` + `fmt` + `clippy -D warnings` verdes antes de mergear.
- ⚠️ **Clippy deve ser full-workspace no toolchain do CI**: rode `rustup update stable` e `cargo clippy --all-targets -- -D warnings` (sem `-p`), porque o CI usa o **stable mais novo** (lints mais estritos, ex.: `unnecessary_sort_by` no 1.96). Clippy escopado por crate **mascara** erros de outras crates (foi o que deixou o job `rust` vermelho até #119).
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
- ✅ 1D — **`session.create`** (`POST /session`, **1ª escrita real**): novo `SessionStore::create` (trait + INSERT sqlx casando o schema da tabela `session` + double em memória com round-trip completo); o handler gera id/slug, resolve o projeto, aplica o body e persiste — **#125**
- ✅ 1E — `session.list` (V1, `GET /session`, `[Session]`): novo `SessionStore::list_full` (método default = `list`+`get_full`, sem duplicar sqlx); filtros directory/workspace/project/search/limit — **#126** (`/session` agora GET+POST nativos; `scope`/`roots`/`path`/`start` são refino)
- ✅ 1F — `session.get` (V1, `GET /session/{id}`, `Session`) + `session.delete` (`DELETE /session/{id}`, `true`, novo `SessionStore::delete` com `ON DELETE CASCADE`) — **#127** (`/session/{id}` agora GET+PATCH+DELETE nativos)
- ✅ 1G — `session.prompt_async` (`POST /session/{id}/prompt_async`, 204): admite o prompt no coordinator (mesmo caminho do V2 prompt → roda em background, eventos via SSE); resolve o model do body ou do registro da sessão; 404 sessão inexistente — **#135**
- ✅ 1H — `session.init` (`POST /session/{id}/init`, 200 `true`): admite o turno de setup do `AGENTS.md` (mesmo caminho do prompt_async, prompt nativo equivalente ao `init` built-in) — **#137**
- ✅ 1u — `v2.health.get` (`GET /api/health`, `{ healthy: true }`, #103) — liveness V2 do GUI
- ✅ 1a — proto `SessionMessage` (união 8 variantes + content + tool-state) — **#69**
- ✅ 1b — read-store `session_message` (seq-window + cursor) — **#70**
- ✅ 1c — rota `GET /api/session/{id}/message` (cursor, 404, reconstrução) — **#71**
- ✅ 1d — **write-path**: runner projeta o turno → `session_message` (dados reais no chat!) — **#80 append, #81 projector, #82 wiring**
- ⏳ 1e — `session.update` (metadata: title, etc.)
- ⏳ 1f — `session.revert` / `session.unrevert`
- ⏳ 1g — `session.share` / `session.unshare`
- ⏳ 1h — `session.command`
- ✅ 1A — `session.children` (`GET /session/{id}/children`, `[Session]`, 404; vazio até o runner criar sub-sessões) — **#112**
- ✅ 1B — **fundação V1 `Message`/`Part`** (`crates/opencode-proto/src/message.rs`, 40 tipos: `Message`=user|assistant, união `error` de 8 erros tipados `{name,data}`, 12 `Part` + uniões `ToolState`/`FilePartSource`/`OutputFormat`; reusa `Range`/`SnapshotFileDiff`/`TokenCache`) + `session.message` (`GET /session/{id}/message/{messageID}`, `{info,parts}`, 404 até projeção V1) — **#138**. Normalizer do `openapi-diff` agora canoniza objeto-sem-properties → `properties:{}` (free-form/map/`{}` viram uma classe estrutural).
- ✅ 1C — **histórico de chat V1 real**: `session.messages` (`GET /session/{id}/message`, `[{info,parts}]`, `before`/`limit`) + `session.message` (single) **servem dados reais** projetando o timeline V2 `session_message` → V1 em tempo de leitura (`opencode-core/session_timeline_v1::timeline_to_v1`, back-fill de `agent`/`model`/`path` da sessão). **É o endpoint que a GUI web (`@opencode-ai/sdk/v2`) usa pro chat** — descoberta: `session.messages` do v2-SDK aponta pro path V1, não `/api/*`. — **#139**. Lossiness/mutações pendentes em PENDENCIAS #7.
- ⏳ 1i — `session.diff`
- ✅ 1y — `v2.session.context` (`GET /api/session/{id}/context`, `{ data: [SessionMessage] }`, 404/500; reusa `SessionMessage` + novo `TaggedUnknownError`=`UnknownError1`) — **#109** (vazio até a engine preparar o contexto)
- ✅ 1j — `session.status` (`GET /session/status`, `{ [id]: SessionStatus }` idle/retry/busy, mapa vazio até a engine) — **#115**
- ⏳ 1k — `session.summarize`
- ✅ 1l — `session.todo` (read store `todo` + rota `GET /session/{id}/todo`) — **#73**
- ✅ 1o — `global.dispose` + `instance.dispose` (lifecycle ack, 200 `true`) — **#74**
- ✅ 1C — `global.event` (`GET /global/event`, SSE `text/event-stream`, compartilha o event bus do `v2.event.subscribe`) — **#118**
- ✅ 1r — `file.list` (`GET /file`, listagem de diretório com flag gitignore) — **#76**
- ✅ 1v — `v2.fs.list` (`GET /api/fs/list`, dados reais: `{ path, type, mime }` via `opencode_tools`, mime best-effort sem dep nova) — **#106**
- ✅ 1w — `v2.fs.find` (`GET /api/fs/find`, busca real arquivos+dirs via `opencode_tools::find_entries`, filtro `type` + `limit`) — **#107**
- ✅ 1x — `v2.fs.read` (`GET /api/fs/read/*`, leitura real de bytes, `application/octet-stream`, path wildcard + guarda anti-traversal) — **#108** · **grupo `fs` completo (list/find/read)**
- ✅ 1z — `file.read` (`GET /file/content`, conteúdo real `FileContent` text/binary+base64, tipos `FilePatch`/`FilePatchHunk`, guarda anti-traversal) — **#111**
- ✅ 1B — `file.status` (`GET /file/status`, dados reais via `git status --porcelain` + `git diff --numstat`, tipo `File`, parser puro testado) — **#113**
- ✅ 1s — `permission.list` + `question.list` (vazios até a engine) — **#77**
- ✅ 1t — `vcs.get` (`GET /vcs`, branch/default_branch via git best-effort) — **#78**
- ⏳ 1p — `session.status` / `session.diff` — **dependem da engine de execução** (estado live / snapshots), não enxutas
- 🔄 1q — mutações de sessão (tipo `Session` v1 #83; `SessionStore::get_full`+`update` aditivos #85):
  - ✅ `session.update` (`PATCH /session/{id}`) — **#85**
  - ✅ `session.revert` / `session.unrevert` (set/clear revert pointer) — **#86**
  - ⏳ `share`/`unshare` — precisa do **serviço externo de URL** (não é só projeção); ver nota
  - ⏳ `command`/`summarize` — usam a execução (write-path pronto)
- ✅ 1m — **permission gating (engine) — fim-a-fim**: gate bloqueante `StorePermissionGate` (#130: read-class roda direto; write-class `write`/`edit`/`patch`/`bash` registra pedido e **parqueia o run** até resolver; wired no `from_env` de produção) + store `PendingPermissions` (register/list/resolve via oneshot) + as 3 listas lêem o store + **`permission.respond`** (`POST /session/{id}/permissions/{permID}`, once/always→Allow, reject→Deny) acorda o run — **#129–#130**. `v2.session.permission.reply` (`POST /api/session/{id}/permission/{reqID}/reply`, 204, GUI-facing) ✅ **#131** (reusa o store). **Falta** (refino): semântica de `always` (salvar regra) e política dirigida por config/ruleset (hoje lista fixa de tools write/edit/patch/bash)
- 🔄 1n — question flow (engine): listas ✅ (#77/#105) · store `PendingQuestions` (register/list/reply/reject via oneshot) + `v2.session.question.list` lê o store + **`v2.session.question.reply`** (`{answers}`, 204) + **`v2.session.question.reject`** (204) resolvem e acordam o run — **#133**. **Tool produtor** `question` ✅ **#134**: seam `QuestionAsker` (opencode-core) + o `question` tool no `NativeToolBox` (oferecido quando o asker está presente, parqueia no store via oneshot e retorna as respostas) + `StoreQuestionAsker` no `drive_one_turn` — espelha o permission gate. **Fim-a-fim**: o agente pergunta → aparece em `v2.session.question.list` → GUI responde (`reply`/`reject`) → o run continua. Resta (refino): `question.reply`/`reject` V1

## Fase 2 — Admin
- ✅ 2a — `config.get`/`global.config.get` (tipo `Config` + loading deep-merge #96) + `config.update`/`global.config.update` (#97, escreve `opencode.json` com merge preservando campos)
- ⏳ 2b — **config com proveniência** (cascata 7 níveis) — capacidade nova (ver PENDENCIAS #1); + os 5 níveis restantes do merge (remote/custom/.opencode/inline/managed) + `.jsonc`
- 🔄 2c — `config.providers` (`GET /config/providers`, tipo V1 `Provider`, #117) rota wirada (retorna `{providers:[], default:{}}`; o merge fiel config+catálogo é refino — superfície real já em `v2.provider.list`/`v2.model.list`); faltam `provider.auth` (escrita de credencial) + teste de conexão
- 🔄 2d — agents: `v2.agent.list` (`/api/agent`) **carrega dados reais** de `{agent,agents}/**/*.md` + `{mode,modes}/*.md` (#122); o V1 `app.agents` (`/agent`) também (mapeado p/ `Agent` V1, #123). Faltam: expansão `permission`→ruleset (default `[]`), built-ins, `topP`/`temperature`/`native` (V1), e escrita (PENDENCIAS #2)
- 🔄 2e — commands: `v2.command.list` (`/api/command`, #120) e o V1 `command.list` (`/command`, mapeado p/ `Command` V1, #123) **carregam dados reais** de `{command,commands}/**/*.md` (global + projeto `.opencode`; parser de frontmatter flat + body, sem dep YAML); faltam built-ins (`init`/`review`), comandos via MCP/skills, `hints`, e escrita
- 🔄 2j — skills/references: `v2.skill.list` (`/api/skill`) agora **carrega dados reais** de `{skill,skills}` (glob `{*.md, **/SKILL.md}`, global + projeto `.opencode`) — **#121**; falta sources via URL + built-ins. `v2.reference.list` (`/api/reference`, #101) ainda `[]` (falta `config.references` + discovery)
- ⏳ 2f — políticas (permission rules)
- ✅ 2k — `project.directories` (`GET /project/{id}/directories`, dados reais: worktree + sandboxes, tipo `ProjectDirectory`) — **#116**; faltam `project.update`/`project.initGit` (escrita/ação)
- ⏳ 2g — snapshots timeline (`session.revert` ligado à UI)
- ⏳ 2h — shared list
- ⏳ 2i — aparência (`config.theme` persistência) — UI-driven, backend mínimo

## Fase 3 — Extensões (greenfield em Rust)
- ⏳ 3a — plugin host JS out-of-process (RPC/JSON, Bun) + ciclo de hooks
- 🔄 3b — MCP via `rmcp` (add/connect/status + reconnect):
  - ✅ `mcp.status` (`GET /mcp`) rota wirada + tipo `McpStatus` (#98, retorna `{}` até o host); falta o **runtime** (`rmcp`) + `mcp.add`/`connect`/`disconnect`
- 🔄 3e — LSP symbols: `find.symbols` (`GET /find/symbol`, tipos `Symbol`/`SymbolLocation`/`Range`/`Position`, #114, retorna `[]` até o host LSP)
- 🔄 3c — integrações in-core (GitHub/GitLab/Slack adapters):
  - ✅ `v2.integration.list` (`GET /api/integration`) rota wirada + closure completo (`IntegrationInfo`/`IntegrationMethod`/`IntegrationPrompt`/`ConnectionInfo`/`IntegrationWhen`/…, #110, retorna `[]` até o runtime); falta o **runtime** + `connect`/`attempt.*`
- 🔄 3d — LSP: `lsp.status` (`GET /lsp`) rota wirada + tipos `LspStatus`/`LspServerStatus` (#99, retorna `[]` até o host LSP); resto de `lsp.*` fora do escopo web-only

## Fase 4 — Cutover Rust-only
- ⏳ 4a — cobertura de providers (execução LLM real) — ver PENDENCIAS #5
- ⏳ 4b — auth-write / OAuth backend completo
- ⏳ 4c — habilitar grupos nativos por padrão + **remover proxy/upstream**
- ⏳ 4d — deletar `packages/server` + `packages/core` TS

## Fora do escopo web-only ➖
- ➖ `sync.*` (replay/steal), parte de `experimental.*` (worktree/workspace/projectCopy create-ops), `mcp.add`/`mcp.auth` avançado, OAuth de provider/credential, billing/login. (`pty.*` e `tui.*` já nativos.)

## Frontend (parallel track — não é "backend Rust")
- ⏳ Recriar o design handoff (`docs/design_handoff_opencode_web`) em SolidJS. Track separado; ver PENDENCIAS #3.

---

## Estado & próximos épicos (para a próxima sessão)

**Marco atingido nesta rodada (mergeado, CI `rust` verde):** a **superfície de leitura do GUI web está nativa em Rust** — chat (session list/get/messages + write-path do runner), catálogo (model/provider/location), **agents/commands/skills com dados reais** (carregados de `.opencode/**.md`, global + projeto), config admin (get/update), file browser (`fs.list/find/read`, `file.read/status`), eventos (SSE `api`+`global`), e os reads wired-empty até a engine (permissions/questions/mcp/lsp/integration/session.status/context). **56 paths contrato-enforçados.** PRs desta rodada: #69–#123. **#119 consertou o job `rust` do CI** (clippy full-workspace no stable do CI — antes mascarado por clippy escopado por crate).

O que resta são **épicos** — cada um multi-fatia, e vários **bloqueados em decisões** (ver PENDENCIAS). Sequência recomendada (maior valor primeiro):

1. **Engine de execução (runner loop) — ✅ NÚCLEO COMPLETO + continuidade.** O loop `run_gated` (tool-calling multi-step + emissão de eventos + projeção pro `session_message`) já existia (write-path #80–#82). Permission gating real fim-a-fim (#129–#131). **Continuidade de contexto (#140):** `drive_one_turn` agora carrega o histórico (`session_timeline::timeline_to_llm` lê o timeline → `Vec<llm::Message>`), semeia o turno com `histórico + prompt`, e persiste **só o tail novo** (sem duplicar). Antes cada turno via só o prompt (sem memória). O **CRUD de sessão** persiste no DB (#125–#127). Restam **ações/extensões que usam a engine** (abaixo).
2. **Ações de sessão.** ✅ **Fundação `Message`/`Part` V1** (#138) + **histórico de chat V1 real** via projeção read-time do timeline V2 (#139 — `session.messages`/`session.message` servem dados reais; é o que a GUI usa). **`session.command`/`session.shell` são STUBS no backend de referência** (o `V2Session` retorna `OperationUnavailableError` p/ shell/skill/compact/switchAgent e nem tem `command`; o handler de `command` mapeia tudo p/ `BadRequest`) → implementá-los por inteiro seria especulativo; quando entrarem, casar o comportamento de referência (404 sessão / 400). Restam, dependendo de store V1 raw + run: `session.deleteMessage`, `part.update`/`part.delete`. `session.summarize`/`compact` (semântica de `compaction.ts`). `session.fork` (cópia de histórico). ✅ `session.init`/`prompt_async` (#135/#137). ✅ Question flow fim-a-fim (#133–#134). Detalhes em PENDENCIAS #7.
3. **Auth/credenciais (Fase 4b)** — `auth.set`/`remove`, `provider.auth` (GET, tipo `ProviderAuthMethod`), `provider.oauth.*`. PENDENCIAS: local de storage de credencial + coexistência.
4. **MCP/LSP runtime (Fase 3b/3d)** — `rmcp` (mcp.connect/add/disconnect/auth) + LSP host; hoje `mcp.status`/`lsp.status`/`find.symbols` retornam vazio.
5. **references loading** — `v2.reference.list` lê `config.references` (local resolve direto; **git refs exigem clone/materialização** — runtime).
6. **Config proveniência + níveis restantes** (PENDENCIAS #1) — cascata 7 níveis + `.jsonc`.
7. **`project.update`/`initGit`, session.share/unshare** — escrita de projeto (store precisa de `update`/create) + serviço externo de share-URL.
8. **Cutover (Fase 4)** — schema-apply (PENDENCIAS #4), cobertura de providers (#5), habilitar grupos por padrão + remover proxy, deletar TS.

**Padrões reutilizáveis desta rodada (para continuar rápido):**
- **Rotas wired-empty**: modelar o tipo + retornar `[]`/`{}`/404 até a engine (ex.: permission/question/mcp/lsp/integration).
- **Maps no contrato** (`additionalProperties`) → o normalizador do `openapi-diff` os descarta → modelar como `Value`/`HashMap` (não precisa do tipo aninhado).
- **Uniões** `anyOf[$ref...]` → enum serde internamente-tagueado em `type`; o normalizador resolve+colapsa. `anyOf[string,string]`→`String`. `anyOf[X,X]` (404) → dedup → `X`.
- **Respostas não-JSON** (octet-stream/SSE): declarar via `content_type`; o diff só compara `application/json` (200 vira `Null`==`Null`).
- **Loaders `.opencode/**.md`**: `parse_md_frontmatter` (frontmatter flat sem dep YAML) + `collect_md_files`; merge global→projeto.
- **CI**: rodar `cargo clippy --all-targets` (full workspace) no **stable atualizado** (`rustup update stable`), nunca escopado por crate.

**Nota de integridade:** não meio-implemento épicos para "parecer pronto". O que está marcado ✅ bate o contrato (`openapi-diff`) e tem teste; o que retorna vazio está **rotulado** como tal (aguardando engine/decisão), nunca disfarçado de completo.
