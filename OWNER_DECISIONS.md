# Rust migration — pendências e decisões para o dono (@danzeroum)

> GitHub Issues está desabilitado no repo, então este arquivo serve como o documento de
> **pendências/decisões** para você revisar quando voltar. Trabalho autônomo em andamento no
> **PR #14** (branch `claude/affectionate-davinci-05ifmg`). Roadmap: [`RUST_MIGRATION.md`](./RUST_MIGRATION.md). Prévia do EventStore (Fase 2): [`EVENTSTORE_PLAN.md`](./EVENTSTORE_PLAN.md).

## Estado atual (CI verde ✅)
- **Fase 0** completa: Cargo workspace (`crates/`), seam strangler-fig (proxy reverso gated por `OPENCODE_RUST_ROUTES`), OpenAPI code-first (utoipa), `xtask`, workflow `rust.yml`.
- **Fase 1** em andamento: `opencode-config` (JSONC + `Config` tipado), `opencode-tools` (`read`/`glob`/`grep` nas libs do ripgrep + `process::run_command`), e o **gate de contrato `openapi-diff`** real.
- Cada push verificado localmente (check / clippy -D warnings / fmt / testes) e verde no `rust` CI.

## Pendências / decisões que precisam de você

1. **Merge / cadência — DECIDIDO (2026-06-15).** ✅ Você aprovou o merge do PR #14 e cadência de **merge semanal** (um PR de cutover por semana, salvo urgência), com o CI como gate definitivo. **⚠️ Descoberta importante antes de mergear:** `push`/merge em `dev` dispara `publish.yml` (release: version-bump + build + publish npm) **e** `deploy.yml` (SST/AWS deploy). Ou seja, mergear cutovers direto em `dev` acionaria release+deploy a cada semana. **Recomendação:** usar uma branch de integração dedicada (ex.: `rust-migration`) como alvo dos PRs de cutover e promover para `dev` só em marcos deliberados. Aguardando sua escolha de alvo de merge (pergunta no chat) antes de mergear o #14.

2. **CI `check-duplicates` travado (infra do fork).** `pr-management.yml` roda em runner self-hosted `blacksmith-4vcpu-ubuntu-2404`, ausente neste fork → check fica *queued* (só no 1º commit, não no HEAD). Não é código. **Recomendo** trocar esse job para `ubuntu-latest` ou desabilitá-lo no fork.

3. **Escopo do CI (paths-ignore).** Adicionei `paths-ignore` (incl. `.github/workflows/**`, `crates/**`, `Cargo.*`, `.config/**`) aos workflows TS (`test`/`typecheck`/`security`/`nix-eval`) para que PRs só-Rust disparem **apenas** o `rust`. Efeito colateral: mudanças só em YAML de workflow não rodam a suíte TS. Confirme se está OK.

4. **TLS removido na Fase 0.** `reqwest` sem `rustls-tls` (o proxy só fala HTTP com o upstream local) — mantém `ring`/`rustls` fora da árvore (deps enxutos, licenças limpas, menos C no cross-compile). **Decisão p/ Fase 3** (provedores LLM): `rustls`+`ring` (melhor p/ musl/windows-arm) vs `aws-lc-rs`. Recomendo `rustls`+`ring`.

5. **Patch do MCP (401 linhas).** `patches/@modelcontextprotocol%2Fsdk@1.29.0.patch` adiciona reconexão/`onsessionexpired`. Auditar se `rmcp` (Rust) tem paridade ou se precisa fork. **Recomendo** auditar antes da Fase 5.

6. **Refactor V2 em andamento.** Há `specs/v2` + migrations recentes e `effect@4.0.0-beta.74` (API `unstable`) → risco de alvo móvel. Decida congelar uma versão do TS para portar contra, ou alinhar as fases aos milestones do V2.

7. **Fidelidade de contrato no cutover.** O gate `openapi-diff` está pronto e só *exige* uma rota quando ela entra em `CUTOVER_PATHS`. Cutover real exige casar exatamente operationId/params/responses/nomes de schema do TS (talvez `#[schema(as = ...)]`). Superfície grande: **17 grupos** em `packages/server` + **21 grupos** de instância em `packages/opencode/.../httpapi/groups` (~38 no total).

8. **Rota `/health` nativa** não existe no contrato golden (é demonstração + `/_rust/health` interno de liveness). Decida manter como endpoint interno ou remover antes do 1º cutover real.

9. **Confirmação de escopo (verificado no disco).** Permanecem em TS: `tui`/`ui`/`app`/`web`/`desktop`/`storybook` (frontend), `enterprise` (app web SolidJS), `function` (Cloudflare Worker), `slack` (bot via SDK), `plugin` (SDK de autoria), `sdk` (cliente gerado), `identity` (só assets). Confirme.

10. **Esforço.** ~2–3× a estimativa inicial. Centro de gravidade: Fase 3 (roteador LLM, ~6 protocolos, ~3.959 L) e Fase 4 (runner; `die`/`catchDefect`/`FiberSet` → enum `TurnOutcome` + `ToolExecutor`). Risco técnico nº 1: o fluxo de controle do runner.

## Como estou tocando
Continuo a implementação autonomamente o máximo possível, mantendo o `rust` CI verde a cada push, atualizando o checklist em `RUST_MIGRATION.md`. Novas pendências/ambiguidades entram **neste arquivo**. Não mergeio o PR sem você.
