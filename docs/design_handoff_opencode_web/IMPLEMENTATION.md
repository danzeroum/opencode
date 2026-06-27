# IMPLEMENTATION.md — Guia para o Claude Code

> **Para o agente Claude Code rodando no repo `danzeroum/opencode` (branch `rust-migration`).**
> Este guia transforma o design handoff em um plano de implementação faseado, já cruzado com o estado real do frontend (`packages/app`, SolidJS) e do backend Rust (`crates/opencode-server` + contrato `packages/sdk/openapi.json`).
>
> Leia primeiro o `README.md` (sistema visual + tokens + descrição tela-a-tela). Este arquivo diz **o que construir, em que ordem, contra qual API, e onde**.

---

## 0. Regras de ouro

1. **Não porte os arquivos `.dc.html`.** São referência visual/comportamental. O runtime deles (`support.js`, sintaxe `{{ }}`/`<sc-for>`) **não** entra no codebase. Reimplemente em **SolidJS** dentro de `packages/app`.
2. **Reaproveite o que já existe.** Chat, settings, providers (API-key), terminal e file-tree **já funcionam** contra o Rust. Não reescreva — estenda.
3. **Tema é a fundação.** Implemente o sistema de tokens (CSS custom properties) antes de qualquer tela. Default = **Argila**. Suporte dark/light + os 38 presets do `desktop-theme.schema.json`.
4. **Mono em tudo** (IBM Plex Mono / Berkeley Mono). Sem sans/serif.
5. **Não invente backend.** Onde a API não existe (tabela §2), ou você implementa o endpoint no Rust (Fase 3+) ou usa o estado **"indisponível"** já desenhado (`Estados de Runtime.dc.html` → cena Indisponível). Nunca chame um endpoint stubbed e mostre tela quebrada.
6. **Valide contra o contrato:** `cargo run -p xtask -- openapi-diff`. Se a operação não está no `openapi.json`, é gap de backend.

---

## 1. Mapa de verdade (design × app × Rust)

Legenda: ✅ existe/funciona · ⚠️ parcial · ❌ não existe.

| Tela do design | FE `packages/app` | API/Rust | Ação |
|---|---|---|---|
| Workspace / chat | ✅ | ✅ `session.*`, `prompt`, `message`, `event` (SSE) | **Polir**, não reescrever |
| Permissão runtime | ⚠️ no prompt-input | ✅ `permission.list/respond` | Especificar p/ consistência (design §5.1) |
| Pergunta do agente | ✅ `session-question-dock` | ✅ `question.reply/reject` | OK |
| Terminal (PTY) | ✅ `components/terminal.tsx` | ✅ `pty.*` + WS | OK (faltava só no design) |
| File-tree / VCS | ✅ `components/file-tree.tsx` | ✅ `file.*`, `find.*`, `vcs.*` | OK |
| Diff / file viewer | ❌ | ✅ `file.read` (+ revert) | **FE novo** (design §5.4) |
| Settings (geral/keybinds/models/providers/servers) | ✅ `settings-v2/` | ✅ `config.get/update` | OK |
| Providers (API-key connect) | ⚠️ connect existe | ✅ `provider.list/auth`, `auth.set` | OK; falta **Testar** |
| Appearance (13 temas + preview) | ⚠️ só tema simples | ✅ `config.theme` | **FE rico** (picker + preview) |
| Onboarding / first-run | ❌ | ✅ `provider.list`, `auth.set` | **FE novo** (§5.5) |
| Empty / erro / conexão | ❌ | ✅ (códigos reais) | **FE novo** (§5.6/5.7) |
| Seletor de modelo mid-session | ❌ | ✅ `provider.list`, `session.update` | **FE novo** (§5.8) |
| Admin · Dashboard (cards) | ❌ | ⚠️ contagens via `*.list`; **cascata sem proveniência** | FE novo; cascata = backend novo |
| Admin · Config geral (badge precedência + 🔒) | ❌ | ⚠️ `config.get/update` sem **proveniência por campo** | FE + backend (proveniência) |
| Admin · Agentes (editor + salvar) | ❌ | ❌ só `agent.list` (read) | FE + **backend (agent write)** |
| Admin · Comandos (editor + playground) | ❌ | ❌ só `command.list` (read) | FE + backend write (playground = FE puro) |
| Admin · Providers (cards + **Testar**) | ⚠️ | ❌ **sem endpoint de teste** | FE rico + **backend (test)** |
| Admin · Políticas (CRUD) | ❌ | ⚠️ só `permission.saved.list/remove` | FE + **backend (create/update)** |
| Admin · Snapshots (timeline undo/redo) | ❌ | ⚠️ `session.revert/unrevert` (sem snapshot API) | FE + backend (ou remapear p/ revert) |
| Admin · Shared sessions | ⚠️ botão share | ❌ **stubbed 501** (removido no cutover) | Indisponível **ou** re-portar |
| Extensions · Plugins | ❌ | ❌ **sem API** (nem no TS) + host JS | Indisponível **ou** projeto grande |
| Extensions · MCP | ⚠️ `dialog-select-mcp` | ❌ **stubbed 404** (`rmcp` não portado) | Indisponível **ou** portar `rmcp` |
| Extensions · Integrações | ❌ | ❌ **stubbed 400** | Indisponível **ou** re-portar |

---

## 2. Fases de implementação

### FASE 1 — Núcleo polido + Appearance (zero backend novo) ⭐ comece aqui

**Objetivo:** app web completo e honesto sobre o binário Rust, sem depender de nenhuma API nova.

1. **Sistema de tema (fundação).**
   - Criar tokens como CSS custom properties no root (`--bg`, `--surface`, `--elev`, `--border`, `--ink`, `--secondary`, `--muted`, `--faint`, `--primary`, `--primary-ink`, `--accent`, `--green`, `--red`, `--yellow`, `--blue`, ... + `--level-0..6` para a precedência).
   - Tema = objeto de tokens. Os 13 temas estão no logic de `designs/Configuracoes Aparencia.dc.html` (método `themes()`) — copie os hex. **Default = `Argila`.** `mode` auto/dark/light (auto segue `prefers-color-scheme`).
   - Persistir no campo `theme` do `opencode.json` via `config.update`.
2. **Tela Appearance** (`designs/Configuracoes Aparencia.dc.html`): catálogo agrupado (Nativo/Claras/Nobres/Natureza) + busca + **preview ao vivo** + toggle de modo + Aplicar. Frontend puro.
3. **Estados de runtime** (`designs/Estados de Runtime.dc.html`) — implementar/alinhar:
   - Permissão inline (§5.1) — alinhar ao que já há no prompt-input.
   - Pergunta (§5.2) — já existe; garantir consistência visual.
   - **Diff/file viewer** (§5.4) — **novo**, sobre `file.read` + aceitar/rejeitar → revert.
   - **Onboarding** (§5.5) — **novo**, empty state guiado p/ conectar provedor.
   - **Empty states** (§5.6) — sessões/agentes/comandos/busca.
   - **Erro & conexão** (§5.7) — prompt falhou/retry, backend offline, 409 ocupado, SSE reconectado.
   - **Seletor de modelo** (§5.8) — **novo**, palette mid-session.
   - **Terminal** (§5.3) — já existe; só faltava no design.
4. **Polir chat/workspace/settings/file-tree** contra a API real.

**Aceite Fase 1:** `bun --cwd packages/app build` → `OPENCODE_WEB_DIR=packages/app/dist cargo run -p opencode-bin` → abrir `localhost:4096`: tema Argila default, trocar tema ao vivo, chat ponta-a-ponta, permissão/pergunta/diff/onboarding/erros, terminal, file-tree — tudo sem 4xx/5xx inesperado.

### FASE 2 — Admin "read" (backend já tem)

- **Dashboard** com contagens reais via `agent.list` / `provider.list` / `command.list` / `config.get`.
- **Config geral** leitura/edição via `config.get/update`. **Sem** a cascata de proveniência (badge de nível por campo) até a Fase 3 — por ora mostre o valor efetivo sem o "de onde veio".
- Listas read-only de Agentes / Comandos / Políticas (sem editar).

### FASE 3 — Backend novo (esforço alto; escolher itens)

Cada item = endpoint/subsistema novo no Rust + a UI de edição correspondente já desenhada:
- **Config provenance / cascata** (7 níveis) — `PENDENCIAS #1`. Precisa de um shape de resposta acordado (ver §3 abaixo). Habilita os badges de nível e a trilha do Dashboard.
- **Agent write** — editor visual/markdown salvando `.opencode/agents/*.md` (frontmatter YAML).
- **Command write** — editor + (playground é FE puro, já resolve placeholders client-side).
- **Provider test** — botão Testar → ping real ao provedor.
- **Policies CRUD** — create/update além do list/remove atual.
- **Snapshots API** — ou expor de fato, ou remapear a timeline para `session.revert/unrevert`.

### FASE 4 — Extensions / recursos dropados (decisão de produto)

`share` / `mcp` / `integrações` / `plugins` estão **stubbed**. Três caminhos por recurso:
1. **Indisponível** — usar a cena "Indisponível" de `Estados de Runtime.dc.html` (estado honesto, sem tela quebrada). **Recomendado a curto prazo.**
2. **Re-portar no Rust** — `share`/`integração` re-portar a lógica; `mcp` portar `rmcp` (com reconnect/`onsessionexpired`); `plugins` é o maior (host JS out-of-process + API nova).
3. **Dropar** as telas do design.

As telas completas (`Plugins e Integracoes.dc.html`) ficam prontas no handoff para quando/se a Fase 4 for priorizada.

---

## 3. Contrato de proveniência (cascata) — a definir antes da Fase 3

A tela de Config geral e a cascata do Dashboard precisam saber **de qual dos 7 níveis** veio cada campo. Sugestão de shape para `config.get` retornar (ou um `config.resolve` novo), a ser acordado entre design + backend:

```jsonc
// por campo resolvido:
{
  "key": "model",
  "effective": "claude-sonnet-4-5",
  "source": "project",            // remote|global|custom|project|opencode|inline|managed
  "locked": false,                // true p/ remote|inline|managed (read-only)
  "trail": [                       // todos os níveis, base→topo
    { "level": "remote",  "value": "claude-3-5-sonnet", "set": true },
    { "level": "global",  "value": "claude-sonnet-4-5", "set": true },
    { "level": "custom",  "value": null, "set": false },
    { "level": "project", "value": "claude-sonnet-4-5", "set": true, "winner": true },
    { "level": "opencode","value": null, "set": false },
    { "level": "inline",  "value": null, "set": false },
    { "level": "managed", "value": null, "set": false }
  ]
}
```

Os 7 níveis (code/cor/fonte/lock) estão tabelados no `README.md` → Design Tokens.

> 📐 **Spec visual + contrato completo:** `designs/Contrato de Proveniencia.dc.html`. Mostra lado a lado **(A) como a UI renderiza** (badge = `source`, valor = `effective`, riscado = nível sobrescrito, 🔒 = `locked`, seletor de nível-destino na escrita) e **(B) o JSON exato** que `config.resolve` devolve, com 5 casos reais: vencedor no meio da pilha (`model`), global vence (`theme`), não-definido em lugar nenhum (`small_model`), managed/locked → edição bloqueada `409` (`autoupdate`), e objeto mesclado entre níveis (`permission.bash`). Implemente o backend contra esse shape; a UI deriva tudo dele.

---

## 4. Onde fica cada coisa (paths reais)

**Frontend (SolidJS):**
- Páginas: `packages/app/src/pages/{home,session}*`
- Chat/composer: `packages/app/src/pages/session/composer/*` (inclui `session-question-dock.tsx`)
- Componentes: `packages/app/src/components/{settings*,dialog-*,prompt-input,file-tree,terminal}.tsx`
- Settings: `packages/app/src/components/settings-v2/`
- **Tokens existentes:** `packages/console/app/src/style/token/{color,font,space}.css` · componentes: `style/component/button.css`
- Temas: `desktop-theme.schema.json`

**Backend (Rust):**
- Handlers: `crates/opencode-server/src/lib.rs`
- Serve da SPA: `crates/opencode-server/src/web.rs` (via `OPENCODE_WEB_DIR`)
- Contrato golden: `packages/sdk/openapi.json`
- Plugins/MCP (Fase 4): `crates/opencode-plugin`, `crates/opencode-integration`; contrato de hooks em `packages/plugin/src/index.ts` (interface `Hooks`, `PluginInput`)

**Build & run (loop de verificação):**
```bash
bun --cwd packages/app build
OPENCODE_WEB_DIR=packages/app/dist cargo run -p opencode-bin
# abrir http://127.0.0.1:4096
cargo run -p xtask -- openapi-diff   # confirma o contrato; op ausente = gap de backend
```

---

## 5. Arquivos de design (referência)

Em `designs/` — abrir no navegador para interagir, ou ler o markup para extrair valores exatos:
- `OpenCode Web.dc.html` — Workspace + todo o Admin (dashboard, geral, agentes+editor, comandos+editor/playground, providers, políticas, servidor, tui, snapshots, shared, cascata de precedência).
- `Estados de Runtime.dc.html` — **§5**: permissão, pergunta, terminal, diff, onboarding, seletor de modelo, empty, erro/conexão, indisponível. **Foco da Fase 1.**
- `Contrato de Proveniencia.dc.html` — **§5.10**: spec visual + JSON de `config.resolve` (cascata dos 7 níveis). Contrato design ↔ backend para a Fase 3.
- `Configuracoes Aparencia.dc.html` — seletor de 13 temas + preview ao vivo (Argila default).
- `Plugins e Integracoes.dc.html` — Plugins/MCP/Integrações (Fase 4, se priorizada).
- `Paletas.dc.html` — explorador das 4 famílias de paleta (referência de tokens).
- `support.js` — runtime da ferramenta de design (**não portar**).

---

## 6. Definition of Done (por tela)

Uma tela está pronta quando:
1. Visual bate com o `.dc.html` de referência (tokens, tipografia mono, raios 3–8px, densidade).
2. Reage ao tema ativo (testar Argila + um dark) e respeita dark/light.
3. Usa **dados reais** da API (ou o estado Indisponível/Empty/Erro quando não há backend).
4. Estados de carregamento, vazio e erro tratados (ver `Estados de Runtime.dc.html`).
5. `openapi-diff` limpo e nenhum 4xx/5xx inesperado no fluxo.
6. Sem login/tenant/billing introduzidos — fora de escopo por design (local-first).
