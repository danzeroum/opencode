# Plano: Plataforma Unificada "Forge" — CLI/TUI de Coding Agent em Rust + Python

> **Repositório-sede da implementação**: `danzeroum/BuildToValue_AI_Agent_Specialization`,
> workspace [`platform/`](../platform/). Este documento também está publicado
> nos repositórios `danzeroum/opencode` e `danzeroum/prompte`, cujas ideias a
> plataforma unifica.

## Contexto

Três repositórios com ideias complementares são unificados num sistema
construído por design em **Python e Rust**:

1. **opencode** (fork TypeScript) — coding agent de terminal: sessões duráveis
   (System Context/Epochs/compaction), agentes selecionáveis, permissões,
   ferramentas (grep/edit/bash/LSP/MCP), TUI. O fork adiciona **ModelTier**
   (comportamento tier-gated para modelos small/medium/large) e o **pipeline de
   verificação determinística** ("o LLM orquestra; ferramentas determinísticas
   verificam").
2. **prompte** (JS/Node) — engenharia de prompts: geradores declarativos, base
   de conhecimento aditiva, quality linter ("ESLint de prompts"), cache por
   hash sha256, rate limiting, proxy LLM com fallback, biblioteca de prompts,
   telemetria offline-first.
3. **BuildToValue** (Python) — squad multi-agente: UnifiedOrchestrator,
   consenso ponderado, planejamento adaptativo, LearningRouter, memória,
   HITL/autonomia progressiva, fallback em 3 níveis, ledger append-only,
   review orientado a ROI, quality gates.

**Produto final**: CLI/TUI de coding agent (`forge`) cujo motor é o squad
multi-agente, com a camada de prompts/qualidade do prompte. **Escopo**:
roadmap completo cobrindo 100% das ideias, em fases longas.

## Arquitetura

### Divisão de linguagens (regra de fronteira)

- **Rust**: tudo que toca disco/rede/processo/segredo ou roda a cada
  keystroke — CLI/TUI, runtime de sessão, gateway LLM (keys de API só aqui),
  ferramentas determinísticas, permissões, pipeline `/verify`, skill-vetter,
  storage SQLite/ledger/telemetria.
- **Python**: tudo que decide "o que fazer a seguir" por raciocínio de
  agente — squad multi-agente completo, PromptForge
  (geradores/knowledge/linter/glossário), review system, avaliação/RAG.

### Integração Rust↔Python: gRPC bidirecional sobre Unix Domain Socket

- `tonic`/`prost` (Rust) × `betterproto`/`grpclib` (Python). Sem PyO3 no
  caminho principal (evita conflito tokio×asyncio; isolamento de falhas:
  crash do sidecar Python aciona o fallback progressivo naturalmente).
- O binário `forge` spawna e supervisiona o sidecar `forge-squadd`; sem
  sidecar → degrada para agente-único → safe-mode read-only.
- Direções: Rust→Python `SquadService.ExecuteTask(SquadTask) → stream
  SquadEvent`; Python→Rust `CoreService`: `Generate` (LLM), `RunTool`,
  `AppendLedger`, `Recall/Remember`, `RequestPermission`. **Python nunca
  chama provedores LLM diretamente** — tudo via gateway Rust.

### Estrutura do workspace `platform/`

```
platform/
├── Cargo.toml                  # cargo workspace
├── justfile                    # just build/test/verify/gen-proto
├── crates/
│   ├── forge-cli/              # bin `forge` (clap), spawn/supervisão do sidecar
│   ├── forge-tui/              # ratatui: chat, diff, permissões, painel do squad
│   ├── forge-core/             # sessões (System Context/Epochs/compaction), agentes
│   │                           #   build/plan/general, permissões, event bus
│   ├── forge-llm/              # gateway: providers (Anthropic/OpenAI/DeepSeek),
│   │                           #   streaming, fallback, ModelTier, prompt caching,
│   │                           #   cache por hash, rate limiting
│   ├── forge-tools/            # grep (crates grep/ignore), edit/patch, bash/PTY,
│   │                           #   webfetch, LSP, MCP (rmcp), sandbox (bollard)
│   ├── forge-verify/           # pipeline /verify + skill-vetter + evidência JSON
│   ├── forge-store/            # rusqlite: sessões, ledger hash-chain, biblioteca
│   │                           #   de prompts, memória do squad, telemetria
│   ├── forge-schemas/          # tipos serde + schemars + canonicalização
│   ├── forge-proto/            # tonic-build sobre ../schemas/proto
│   └── forge-server/           # axum: API local + dashboard de métricas
├── python/  (uv workspace)
│   └── packages/
│       ├── forge-proto-py/     # stubs gerados — nunca editados à mão
│       ├── forge-squad/        # orquestrador, agentes, consenso, planning,
│       │                       #   routing, memory, hitl, fallback, parallel
│       ├── forge-promptforge/  # geradores, knowledge base, quality linter, glossário
│       ├── forge-review/       # 4 reviewers, value_score, quality gates, certificação
│       └── forge-eval/         # avaliação contínua, A/B, RAG tools
├── schemas/
│   ├── proto/                  # core.proto, squad.proto, llm.proto (fonte única)
│   ├── json/                   # *.v1.schema.json (evidência, ledger, handoff,
│   │                           #   cache-key, telemetria, prompt-template)
│   └── fixtures/               # golden files p/ testes de contrato cross-language
├── skills/                     # skills built-in + padrão de autoria
├── docs/adr/                   # ADRs (governança BuildToValue)
├── infra/                      # docker-compose, terraform, ansible, k6 (adaptados)
└── .github/workflows/          # cargo test, pytest, contrato, gitleaks, semgrep, buf
```

## Mapeamento ideia → componente (cobertura 100%)

| Origem | Ideia | Destino | Fase |
|---|---|---|---|
| opencode | Permissões por ferramenta/escopo | `forge-core::permission` | 1 |
| opencode | Providers múltiplos + catálogo + troca em sessão | `forge-llm::catalog` | 1–2 |
| opencode | grep, edit/patch, bash, webfetch | `forge-tools` | 1 |
| opencode | Sessões duráveis (System Context, Epochs, compaction) | `forge-core::context` (spec: `CONTEXT.md`) | 2 |
| opencode | Agentes build/plan/general | `forge-core::agent` | 2 |
| opencode | TUI rica | `forge-tui` | 2 |
| opencode | AGENTS.md, prompt caching, truncamento gerenciado | `forge-core` + `forge-llm` | 2 |
| fork | **ModelTier** tier-gated (prompt enxuto, menos tools, compaction ~75%, step-discipline) | `forge-llm::model_tier` (porta de `model-tier.ts`) | 2 |
| prompte | 25 geradores declarativos, knowledge base, quality linter, glossário | `forge_promptforge` | 3 |
| prompte | Cache por hash (canônico + sha256, paridade Rust×Python) | `forge-llm::prompt_cache` + `forge_promptforge.hashing` | 3 |
| prompte | Rate limiting por tier, proxy seguro com fallback | `forge-llm` | 1/3 |
| prompte | Biblioteca de prompts, histórico, telemetria + dashboard | `forge-store` + `forge-server` | 3 |
| BTV | Squad (Architect/Dev/Auditor/Designer/Ops/Supervisor/Exploration/Recovery) | `forge_squad.agents` | 4 |
| BTV | UnifiedOrchestrator, consenso ponderado, planning, routing, memória, paralelo | `forge_squad.*` (migração de `src/`) | 4 |
| BTV | HITL/autonomia progressiva, fallback 3 níveis, handoff events | `forge_squad.hitl/.fallback` + `squad.proto` | 4 |
| fork | Pipeline `/verify` + evidência JSON + skill-vetter | `forge-verify` | 5 |
| BTV | Review (4 reviewers, value_score >0.7), quality gates, certificação | `forge_review` | 5 |
| BTV | Governança: ADRs, ledger hash-chain, overrides, "Nada Fake" | `forge-store::ledger` + `docs/adr/` | 1/5 |
| fork | CI segurança (gitleaks bloqueante, semgrep), commit trailers, bench | `.github/workflows/` + criterion | 5–6 |
| opencode | LSP, MCP, plugins/skills de terceiros | `forge-tools` + `skills/` | 6 |
| BTV | Sandbox Docker, RAG, avaliação contínua/A-B, infra (terraform/ansible/k6/grafana) | `forge-tools::sandbox`, `forge_eval`, `infra/` | 6 |

## Roadmap (fases longas)

- **Fase 1 — Fundação executável (~6–8 sem)**: workspaces cargo+uv, schemas
  iniciais, `forge-llm` (3 providers, streaming, fallback), `forge-tools`
  básico, loop de agente único com permissões ask/allow/deny, sessões+ledger
  no SQLite, `forge run`/`forge chat`. *Critério: editar código num repo real
  com permissão interativa; ledger registra; `just test` verde.*
- **Fase 2 — Sessões duráveis + TUI + ModelTier (~8–10 sem)**: System Context
  completo (Epochs, compaction em fronteiras seguras, baseline p/ prompt
  caching), agentes build/plan/general, ModelTier tier-gated, AGENTS.md, TUI
  ratatui completa. *Critério: sessão sobrevive a restart; compaction sem
  quebrar cache; snapshot tests da TUI (insta + TestBackend).*
- **Fase 3 — PromptForge + gateway completo (~6–8 sem)**: geradores/knowledge/
  linter/glossário no sidecar (primeira ativação do gRPC), cache por hash com
  fixtures de paridade, rate limiting, biblioteca de prompts, telemetria +
  dashboard. *Critério: hash idêntico Rust×Python nas fixtures; degradação
  graciosa sem sidecar.*
- **Fase 4 — Squad multi-agente como motor (~10–12 sem)**: migração do
  UnifiedOrchestrator/consenso/planning/routing/memória/paralelo, agentes com
  LLM real via gateway, HITL na TUI, fallback 3 níveis (incl. supervisão do
  sidecar), `forge squad "..."` + painel ao vivo. *Critério: tarefa
  multi-arquivo com consenso no ledger; kill -9 do sidecar aciona fallback;
  e2e cobre handoff.start/ack/complete/error.*
- **Fase 5 — Verificação, review e governança (~6–8 sem)**: `/verify` com
  evidência `verification-evidence.v1`, auditor consome evidência real,
  skill-vetter, `forge_review` + quality gates + certificação, CI de
  segurança, ADRs. *Critério: a plataforma passa no próprio `/verify`
  (self-hosting); PR sem evidência bloqueado.*
- **Fase 6 — Ecossistema e escala (~8–10 sem)**: LSP/MCP completos, plugins de
  terceiros vetados, sandbox Docker, RAG, A/B testing via telemetria, bench
  criterion, `infra/` completa + k6. *Critério: skill de terceiro roda após
  vetting; A/B gera relatório; k6 valida P95 do gateway.*

## Contratos-chave

- **Protobuf** (`schemas/proto/`) para o wire gRPC; geração via
  `just gen-proto`; `buf breaking` no CI.
- **JSON Schema** (`schemas/json/*.v1.schema.json`) para documentos
  persistidos/auditáveis: `verification-evidence.v1`, `handoff-event.v1`,
  `ledger-entry.v1` (hash-chain, campos `override` e `fake_marker` de 1ª
  classe), `prompt-cache-key.v1` (JSON canônico de chaves ordenadas + sha256),
  `telemetry-event.v1`, `prompt-template.v1`.
- Rust: `schemars` deriva schema dos tipos; Python: pydantic. Golden fixtures
  round-trip nos dois lados no CI. Breaking → novo `.v2` + ADR.

## Reuso do código existente

- **Migrar** (BuildToValue `src/` → `python/packages/forge-squad/`):
  `consensus/weighted_voting.py` (✅ migrado no scaffold),
  `hitl/progressive_autonomy.py`, `planning/`, `routing/learning_router.py`,
  `memory/agent_memory.py`, `parallel/resource_manager.py`, `safety/`,
  estrutura do `orchestration/unified_orchestrator.py`;
  `.buildtovalue/review/orchestrator.py` → `forge_review` (quase direta).
- **Reescrever**: `src/agents/*.py` (hoje stubs heurísticos — "Nada Fake":
  agentes reais chamam LLM via gateway; mantêm-se as interfaces);
  `secure_executor.py` → `forge-tools::sandbox` em Rust.
- **Especificação de referência** (portar ideias, não código):
  `opencode/CONTEXT.md` (spec do `forge-core::context`),
  `opencode/packages/opencode/src/provider/model-tier.ts` (✅ portado),
  `opencode/script/verify.ts` + `docs/adr/0001`, `prompte/api/src/hash.js`
  (✅ portado com paridade testada), `prompte/api/src/llm.js` (fallback),
  geradores/linter do frontend do prompte.

## Verificação

1. **Unit**: `cargo test` + `insta` (snapshots TUI/prompts); `pytest` +
   `hypothesis` (consenso/planner); `clippy -D warnings`, `ruff`,
   `mypy --strict`.
2. **Contrato cross-language**: golden fixtures round-trip; paridade de hash;
   `buf breaking`.
3. **Integração Rust↔Python**: sidecar real via UDS efêmero; injeção de falha
   (kill -9) valida fallback.
4. **LLM sem custo**: gravação/replay de chamadas (modo cassette, inspirado no
   `http-recorder` do opencode); job noturno opcional contra APIs reais.
5. **E2E**: `expectrl`/PTY dirigindo o binário em repos-fixture;
   `ratatui::TestBackend`.
6. **Self-hosting** (Fase 5+): `forge verify` roda no próprio workspace;
   evidência JSON obrigatória em PR.

## Riscos principais

| Risco | Mitigação |
|---|---|
| Complexidade do runtime de sessão subestimada | `CONTEXT.md` como spec formal; invariantes como tipos + property tests; marcos internos na Fase 2 |
| Drift de contrato Rust×Python | Fonte única em `schemas/`, geração de código, fixtures no CI, `buf breaking` |
| tokio×asyncio no mesmo processo | Resolvido por arquitetura (sidecar gRPC, nunca embedding) |
| Sidecar Python cai | Fallback progressivo de 1ª classe: agente-único → safe-mode read-only |
| Escopo 100% = nunca entregar | Cada fase termina em software usável; Fase 1 já é um coding agent funcional |
| Keys de API vazarem | Keys só no processo Rust; Python conhece só o UDS (princípio do proxy do prompte) |
| Segurança de bash/skills de terceiros | Permissões no core Rust (não contornáveis), skill-vetter bloqueante, sandbox Docker, gitleaks |

## Estado atual (scaffold da Fase 1 entregue)

O workspace `platform/` já contém, compilando e testado (26 testes Rust +
13 Python, clippy/fmt limpos):

- **Contratos**: 3 protos gRPC (`core`, `squad`, `llm`), 6 JSON Schemas
  versionados e fixtures de paridade do hash de cache validadas pelos dois
  lados (Rust `forge-schemas/tests/parity.rs` × Python
  `forge_promptforge/tests/test_hashing.py`).
- **Rust**: ModelTier portado (com tier-gating de compaction), motor de
  permissões com perfis build/plan/general, ledger append-only com
  hash-chain e detecção de adulteração, pipeline `/verify` mínimo com
  evidência JSON, contrato de ferramenta com truncamento gerenciado, CLI
  `forge` (clap) com subcomandos run/chat/verify/squad.
- **Python**: consenso ponderado migrado e tipado (pydantic) com gatilho
  HITL < 0.7, primeiros geradores declarativos, quality linter, value_score
  do review system.
- **Operação**: justfile, CI GitHub Actions (cargo/pytest/gitleaks
  bloqueante), ADR 0001, script de regeneração de fixtures.
