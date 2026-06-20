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
- **Decisão necessária:** quais providers você realmente vai usar? Isso define o quanto precisamos portar/cobrir nativamente antes do cutover.
- **Status:** sigo com o write-path usando os protocolos nativos existentes; cobertura ampla fica para a Fase 4.

### #6 — Write-path do runner (épico — o linchpin do chat real)
A rota `v2.session.messages` lê `session_message`, mas **nada grava ali ainda**: o runner Phase 4 é spike puro (sem persistência). Para o chat funcionar de verdade, falta o caminho de escrita:
1. Runner executa o turno e **emite eventos de sessão** no event store (existe).
2. **Projector** (porta `packages/core/src/session/projector.ts`, ~451 linhas) faz o fold dos eventos → linhas em `session_message` (+ `message`/`part`).
- **Tamanho:** grande, multi-fatia. É efetivamente o coração da engine de execução.
- **Plano:** vou portar incrementalmente (começando por user + assistant message events, que é o que o chat precisa) quando as leituras tratáveis estiverem cobertas. Não bloqueia as demais rotas.
- **Status:** épico aberto; priorizando leituras/rotas independentes primeiro para maximizar progresso mergeado.

## Resolvidas

### #6 — Write-path do runner ✅ (resolvido — #80/#81/#82)
Resolvido com uma abordagem **Rust-native** (em vez de portar o `projector.ts` acoplado): o runner já produz a conversa (`Vec<Message>`), então:
- **#80** `SessionMessageStore::append` (escrita com auto-seq).
- **#81** `session_timeline::project_turn` (transform puro `Message → SessionMessage`).
- **#82** `drive_one_turn` projeta o turno e persiste → `v2.session.messages` retorna dados reais.

**Caveat de coexistência (atenção):** se um servidor **TS também estiver projetando** o mesmo event stream, ambos gravariam `session_message` (duplicação). Rode **apenas um projector** quando os dois servidores estiverem vivos — o alvo é deploy **Rust-only**. (Se precisar de coexistência real, dá pra gatear a projeção Rust atrás de uma env flag; hoje ela roda sempre que o runner Rust executa o turno.)

Refinos futuros (não bloqueiam): `cost` por turno (precisa de pricing do catálogo), `finish` reason por step, blocos de `reasoning`.
