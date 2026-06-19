# Pendências — decisões/itens para o humano

> Coisas que precisam da sua decisão ou atenção. Não bloqueiam o avanço geral — quando bato numa, registro aqui e sigo para a próxima fatia tratável. Resolva quando puder; eu consulto este doc.

## Abertas

### #1 — Config com proveniência (cascata de 7 níveis) — Fase 2
O design põe a **precedência (REMOTE…MANAGED) por campo** como núcleo do Admin, mas o contrato atual (`config.get`) devolve um `Config` **plano, sem origem por campo**. Para renderizar a cascata, o backend precisa expor a origem de cada valor.
- **Decisão necessária:** (a) novo endpoint Rust que anota proveniência por campo (paridade+, eu defino o shape), ou (b) expor os configs crus por nível e o front mescla. Recomendo (a).
- **Status:** vou implementar `config.get/update` planos primeiro (2a) e deixar a proveniência (2b) para depois desta decisão.

### #2 — Escrita de agents/commands — Fase 2
Editar agents/commands = escrever arquivos `.opencode/agents/*.md` e de comandos. O contrato é **read-only** (não há `agent.update`/`command.update`).
- **Decisão necessária:** criar endpoints de escrita nativos (eu proponho o shape) OU um endpoint genérico de file-write. Recomendo endpoints dedicados.
- **Status:** farei list/leitura primeiro; escrita aguarda decisão.

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

## Resolvidas
(nenhuma ainda)
