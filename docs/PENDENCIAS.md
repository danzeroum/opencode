# Pendências — decisões/itens para o humano

> Coisas que precisam da sua decisão ou atenção. Não bloqueiam o avanço geral — quando bato numa, registro aqui e sigo para a próxima fatia tratável. Resolva quando puder; eu consulto este doc.

## Abertas

### #1 — Config com proveniência (cascata de 7 níveis) — Fase 2
O design põe a **precedência (REMOTE…MANAGED) por campo** como núcleo do Admin, mas o contrato atual (`config.get`) devolve um `Config` **plano, sem origem por campo**. Para renderizar a cascata, o backend precisa expor a origem de cada valor.
- **Decisão necessária:** (a) novo endpoint Rust que anota proveniência por campo (paridade+, eu defino o shape), ou (b) expor os configs crus por nível e o front mescla. Recomendo (a).
- **Status:** vou implementar `config.get/update` planos primeiro (2a) e deixar a proveniência (2b) para depois desta decisão.
- **⚠️ Achado de escopo (importante):** o tipo `Config` é o **mais complexo do contrato** — 35 props de topo + ~19 tipos no closure, com **maps** (`additionalProperties`: command/provider/mcp/tools/references) e **uniões inline** (`mcp` = Local|Remote|{enabled}, `formatter` = bool|obj, `lsp` = bool|obj, `autoupdate` = bool|"notify", `plugin` = string|[string,obj]). O `openapi-diff` exige a estrutura exata (não dá pra usar `serde_json::Value` como atalho). Então o **tipo `Config` sozinho é um épico multi-fatia**, não uma fatia única. Plano de sub-fatias:
  1. Leaves simples: `ServerConfig`, `LogLevel`, `LayoutConfig`, `AttachmentConfig`/`ImageAttachmentConfig`, `ConfigV2ReferenceGit`/`Local`, `ConfigV2ExperimentalPolicy`, `PolicyEffect`.
  2. Médios: `AgentConfig` (15 props), `ProviderConfig` (9), `McpLocalConfig`/`RemoteConfig`/`OAuthConfig`, `PermissionConfig`/`RuleConfig`/`ActionConfig`/`ObjectConfig`.
  3. O struct `Config` (com os maps + uniões inline) + `config.get`/`update` (leitura/merge).
- **✅ Simplificação-chave (de-risca o resto):** o normalizador do `openapi-diff` **descarta `additionalProperties`**, então qualquer **campo-map normaliza pra `{type:object}`** → modelo esses como `serde_json::Value` (`#[schema(value_type=Object)]`) em vez de portar a estrutura aninhada. Isso elimina o pior do `ProviderConfig` (o map `models` com config de modelo completa), `AgentConfig.tools`, e os maps do topo do `Config` (`command`/`provider`/`mcp`/`tools`/`references`). **Só as uniões inline** precisam de modelagem real: `autoupdate` (bool|"notify"), `formatter`/`lsp` (bool|obj), `mcp` value (Local|Remote|{enabled}), `plugin` item (string|[string,obj]), `oauth` (McpOAuthConfig|false), `timeout` (int|false), `PermissionConfig`/`PermissionRuleConfig`. Atenção: objetos com `properties` tipadas (ex.: `ProviderConfig.options`) **não** colapsam — esses precisam ser modelados.
  - Sub-fatias 1 ✅ (#88 leaves). 2 (Permission* + Mcp* + AgentConfig + ProviderConfig.options) e 3 (struct `Config` + rota) seguem.

### #2 — Escrita de agents/commands — Fase 2
Editar agents/commands = escrever arquivos `.opencode/agents/*.md` e de comandos. O contrato é **read-only** (não há `agent.update`/`command.update`).
- **Decisão necessária:** criar endpoints de escrita nativos (eu proponho o shape) OU um endpoint genérico de file-write. Recomendo endpoints dedicados.
- **✅ Leitura RESOLVIDA (#120–#123):** `command`/`skill`/`agent` (V2 `/api/*` e V1 `/agent`,`/command`) agora **carregam dados reais** de `{command,commands,skill,skills,agent,agents,mode,modes}/**/*.md` (global config dir + projeto `.opencode`, projeto sobrescreve), com parser de frontmatter próprio (sem dep YAML). **Só a escrita aguarda esta decisão.**
- **Lacunas de leitura conhecidas (não bloqueiam):** expansão `permission`→ruleset (hoje `[]`), built-ins (`init`/`review`, agentes default), comandos via MCP/skills, `hints`/`topP`/`temperature`/`native` (campos V1), e `v2.reference.list` (git refs precisam de clone).

### #3 — Local do frontend
O README do handoff cita `packages/console/app` (SolidJS), mas esse diretório está **vazio**; a GUI web real é `packages/app` (+ `ui`). 
- **Decisão necessária:** recriar o design em `packages/app`/`ui` existentes (recomendado) ou criar `packages/console/app` novo?
- **Status:** o frontend é track separado do backend Rust; não estou priorizando, foco no backend. Quando chegar a hora, sigo o que decidir.

### #4 — Schema ownership (Rust aplica migrações) — Fase 0/4
Hoje a política é "TS migra, Rust verifica". Para **desligar o TS**, o Rust precisa **aplicar** as ~35 migrações e ser dono do schema.
- **Risco:** durante a coexistência, ter os dois aplicando migrações pode conflitar. 
- **Decisão necessária:** ok para o Rust assumir a aplicação de migrações (e o TS parar de migrar)? Isso normalmente é o passo final antes do cutover.
- **Status:** deixo para a Fase 4 (perto do cutover). Não bloqueia as rotas.

### #5 — Cobertura de providers (execução LLM) — Fase 4
A web dispara LLM real via `session.prompt`. Há 5 protocolos nativos (anthropic, openai-chat, gemini, bedrock, openai-responses). O TS cobre a cauda longa via `@ai-sdk/*`.
- **✅ Multi-provider OpenAI-compatible RESOLVIDO (#142):** o registry deixou de ser hardcoded em Anthropic. Agora `ProtocolKind` (anthropic vs openai-compatible) + `OpenAiCompatibleEngine` (protocolo `openai_chat` já existente) servem **qualquer provider compatível com OpenAI** — `deepseek`, `zhipuai`/`glm`, `ollama`, `openai`, `groq`, … Resolução de baseURL: override `OPENCODE_<ID>_BASE_URL` → (catálogo `api`, follow-up) → default built-in; key: `auth.json[id]` → env (`DEEPSEEK_API_KEY`/`ZHIPUAI_API_KEY`/…); **ollama é keyless** (localhost ou ids locais). **Como usar deepseek/glm/ollama:** ver "Como configurar providers" no ROADMAP.
- **Pendente:** (a) **lookup no catálogo models.dev** para baseURL/env de *qualquer* provider sem default built-in (hoje built-ins cobrem os comuns; o resto precisa de `OPENCODE_<ID>_BASE_URL`); (b) **config `provider.<id>.options.baseURL`** (hoje o override é por env var, não pelo `opencode.json`); (c) protocolos **gemini/bedrock** (existem em `opencode-llm` mas faltam arms no registry — não são openai-compatible); (d) **OAuth** (refresh) — só API key/keyless hoje.
- **Decisão necessária:** confirmar endpoints específicos — GLM é China (bigmodel.cn, default) ou internacional (z.ai)? Qual o host do Ollama externo? (Ambos configuráveis por env var sem mudar código.)

### #6 — Write-path do runner (épico — o linchpin do chat real)
A rota `v2.session.messages` lê `session_message`, mas **nada grava ali ainda**: o runner Phase 4 é spike puro (sem persistência). Para o chat funcionar de verdade, falta o caminho de escrita:
1. Runner executa o turno e **emite eventos de sessão** no event store (existe).
2. **Projector** (porta `packages/core/src/session/projector.ts`, ~451 linhas) faz o fold dos eventos → linhas em `session_message` (+ `message`/`part`).
- **Tamanho:** grande, multi-fatia. É efetivamente o coração da engine de execução.
- **Plano:** vou portar incrementalmente (começando por user + assistant message events, que é o que o chat precisa) quando as leituras tratáveis estiverem cobertas. Não bloqueia as demais rotas.
- **Status:** épico aberto; priorizando leituras/rotas independentes primeiro para maximizar progresso mergeado.

### #7 — Projeção V1 de mensagens (read-time) — fidelidade e mutações
A GUI web lê o histórico do chat via **`session.messages` V1** (`GET /session/{id}/message`; o `@opencode-ai/sdk/v2` mapeia `session.messages` para o path V1, com `directory`/`before`). Isso agora **retorna dados reais** (#139): projeto o timeline V2 `session_message` (fonte única) → V1 `{info, parts}` em tempo de leitura (`session_timeline_v1::timeline_to_v1`), back-fillando `agent`/`model`/`path` do registro da sessão (o timeline V2 não os carrega no nível `user`).
- **Lossiness conhecida (aceita por ora):** ids de `Part` são sintetizados deterministicamente (`prt_{messageID}_{i}`); entradas-marcador que o V1 não modela são **puladas** (`agent-switched`/`model-switched`/`system`/`shell`/`compaction`). Inócuo hoje porque os **produtores** dessas entradas (troca de agente/modelo mid-run, `session.shell`, `session.summarize`) ainda não estão na engine Rust. Quando entrarem, decidir: (a) mapear essas entradas no projetor V1, ou (b) trocar para um **store V1 raw** de `message`/`part` (mais fiel, mais código + storage duplo).
- **Mutações V1 ainda pendentes** (precisam de run síncrono + escrita no timeline + projeção): `session.command` (`{info,parts}`), `session.shell` (idem + 409), `session.deleteMessage` (bool), `part.update` (Part), `part.delete` (bool). `session.command`/`shell` são os próximos pontos da engine.

### #8 — Eventos de ciclo de vida do PTY no barramento SSE — follow-up
O grupo `pty` está **funcionalmente completo** (spawn/list/get/update/remove + streaming WebSocket via `connect`). A única lacuna deliberada: o TS publica `pty.created`/`pty.updated`/`pty.exited`/`pty.deleted` no `EventV2` (consumidos pelo SSE `/event`), enquanto o `PtyManager` Rust (registry global em `OnceLock`, sem handle do event store) **ainda não publica** esses eventos.
- **Impacto:** nenhum no contrato HTTP/OpenAPI (o `openapi-diff` passa com as 8 ops) nem na interação real do terminal (o streaming é via WebSocket, não via SSE). Só afeta clientes que queiram reagir a criação/saída de PTY pelo stream `/event` global.
- **Plano:** quando o barramento de eventos for acessível ao subsistema PTY (passar o handle do `EventStore`/bus ao `PtyManager`, ou movê-lo para o `AppContext`), emitir os 4 eventos no `create`/`update`/exit(reader-thread)/`remove`. Não bloqueia nada.

### #9 — Estratégia dos ~13% finais (subsistemas opcionais) — DECISÃO do dono
As ops restantes são todas de **subsistemas opcionais** que um setup local deepseek/glm/ollama **não usa** (todos os providers do dono são API-key/keyless, já funcionando): **OAuth** de provider/MCP, **integrações** github/slack, **share** público de sessão, **sync** multi-device, **workspaces** cloud, **self-upgrade**. Tentei perguntar a estratégia (a ferramenta de pergunta falhou); segui pela minha recomendação e o sinal repetido de "continue".
- **Decisão que tomei (revisável):** **portar de verdade** o que é self-contained (OAuth/integração falam com endpoints padrão dos providers) e **erro fiel** o que precisa de infra hospedada da opencode (share→500, sync/workspace→400). Isso chega a 100% de cobertura de contrato sem successes falsos, permitindo remover o proxy e (com sua confirmação) deletar o TS.
- **Já feito sob essa decisão (este PR):** `sync.replay/steal`, `experimental.workspace.remove`, `session.share/unshare` como erros fiéis (ver T7.3e-infra no ROADMAP).
- **Alternativas se preferir:** (a) **portar tudo** (OAuth/share/sync/integrações/workspaces reais — bem mais trabalho para features fora do seu uso); (b) **portar só OAuth** (caso queira logar em Claude/Copilot depois) e deixar o resto como erro fiel; (c) **manter um sidecar TS mínimo** só para esses grupos (não deletar o TS por completo). Me avise e eu ajusto — trocar um erro fiel por um port real é só o corpo do handler (baixo retrabalho).
- **⚠️ Antes do passo irreversível** (deletar `packages/{server,core,llm}` + parar de publicar o TS) eu **paro e confirmo** com você.

## Resolvidas

### #6 — Write-path do runner ✅ (resolvido — #80/#81/#82)
Resolvido com uma abordagem **Rust-native** (em vez de portar o `projector.ts` acoplado): o runner já produz a conversa (`Vec<Message>`), então:
- **#80** `SessionMessageStore::append` (escrita com auto-seq).
- **#81** `session_timeline::project_turn` (transform puro `Message → SessionMessage`).
- **#82** `drive_one_turn` projeta o turno e persiste → `v2.session.messages` retorna dados reais.

**Caveat de coexistência (atenção):** se um servidor **TS também estiver projetando** o mesmo event stream, ambos gravariam `session_message` (duplicação). Rode **apenas um projector** quando os dois servidores estiverem vivos — o alvo é deploy **Rust-only**. (Se precisar de coexistência real, dá pra gatear a projeção Rust atrás de uma env flag; hoje ela roda sempre que o runner Rust executa o turno.)

Refinos futuros (não bloqueiam): `cost` por turno (precisa de pricing do catálogo), `finish` reason por step, blocos de `reasoning`.
