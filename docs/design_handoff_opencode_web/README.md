# Handoff: OpenCode Web Console (workspace + admin + extensões + aparência)

> Projeto: **Danzeroum OpenCode** — fork de `github.com/danzeroum/opencode` (branch `rust-migration`).
> Console web que dá interface gráfica ao backend local do OpenCode (o engine que hoje roda como TUI/servidor em `127.0.0.1:4096`).

> 👉 **Vai implementar?** Comece pelo **`IMPLEMENTATION.md`** — plano faseado para o Claude Code, com a matriz de gaps (design × `packages/app` × backend Rust), ordem de execução e paths reais. Este README é a referência de design (visual, tokens, telas).

---

## Overview

Este pacote documenta a interface web do OpenCode: um **console operador** para conversar com agentes de código (workspace) e administrar toda a configuração do sistema (admin), incluindo extensões (plugins/MCP/integrações) e temas.

O produto é **local-first**: fala com o servidor OpenCode local pela API HTTP/OpenAPI existente (`/openapi.json`). **Não há** login, multi-tenant ou billing — isso é por design. O console é uma camada de UI sobre o `config.json`/`opencode.json` e os endpoints já existentes.

Telas cobertas neste handoff:
1. **Workspace** — conversa com agente, command palette (`/`), quick actions, snapshots.
2. **Admin** — Dashboard, Configuração geral, Agentes (+ editor visual/markdown), Comandos (+ editor com playground de placeholders), Provedores, Políticas, Servidor, TUI/Keybinds, Snapshots, Compartilhados.
3. **Extensões** — Plugins, MCP servers, Integrações; drawer de detalhe do plugin; fluxo de install com gate de capacidades.
4. **Configurações › Aparência** — seletor de tema com preview ao vivo (13 temas, **Argila é o default**).

---

## About the Design Files

Os arquivos em `designs/` são **referências de design feitas em HTML** — protótipos que mostram o visual e o comportamento pretendidos. **Não são código de produção para copiar literalmente.**

> ⚠️ Formato: os arquivos têm extensão `.dc.html` ("Design Component") e dependem de um runtime próprio (`support.js`) usado na ferramenta de design. **Não porte esse runtime nem a sintaxe de template (`{{ }}`, `<sc-for>`, `<sc-if>`).** Use os arquivos apenas como referência visual/comportamental — abra no navegador para interagir, ou leia o markup para extrair valores exatos.

A tarefa é **recriar esses designs no ambiente do codebase OpenCode**. O frontend de referência do repo é **SolidJS** (`packages/console/app`, com tokens em `src/style/token/*.css`). Se for implementar lá, use os tokens e componentes existentes (`button.css`, etc.). Se optar por outro stack (React/Vue), replique os tokens abaixo. Reaproveite o que já existe — **dark/light e os 38 temas já vivem no `desktop-theme.schema.json`**; este handoff só adiciona a UI de seleção e 4 famílias novas de paleta.

---

## Fidelity

**Alta fidelidade (hi-fi).** Cores, tipografia, espaçamento e interações são finais. Recrie pixel-a-pixel usando as bibliotecas/padrões do codebase. Os valores exatos (hex, px, pesos) estão em **Design Tokens**.

---

## Sistema visual (resumo)

- **Tipografia:** `IBM Plex Mono` em **tudo** (o produto é mono-first, herdado do Berkeley Mono do TUI). Pesos 400/500/600/700.
- **Raios:** 3–8 px (terminal-inspired). Cards 9–14px, botões 6–7px, chips/badges 4–6px, swatches 2–5px.
- **Densidade:** alta. Fonte base 13px; secundário 11–11.5px; labels/eyebrows 9.5–10.5px uppercase com `letter-spacing` 0.6–1.2px.
- **Marca:** logo "nested square" (dois quadrados concêntricos) em SVG — ver **Assets**.
- **Layout de chrome:** topbar fixa 48–50px + sidebar contextual 248px (workspace) / nav admin. Conteúdo em coluna central com `max-width` (780px conversa, 860–1040px admin).
- **Tema base do app:** `opencode` (dark) hoje; **o default solicitado para a nova UI de aparência é `Argila` (clara)**.

---

## Screens / Views

### 1. Shell (topbar + sidebar)

**Topbar** (altura 48px, `background:#0c0c0c`, `border-bottom:1px solid #1c1c1c`):
- Esquerda: logo SVG (17×21) + wordmark `opencode` (weight 600) + chip `web` (10px, borda `#262626`, radius 4px).
- Centro-esq: segmented control **Workspace | Admin** (container `#141414`, borda `#242424`, radius 7px, padding 2px; aba ativa `background:#262626`, texto `#fab283`; inativa texto `#8a8a8a`).
- Direita: status `localhost:4096` (dot verde `#7fb069` com glow), modelo ativo (`◍` accent + nome), botão de tema (30×30, `#141414`).

**Sidebar Workspace** (248px, `#0c0c0c`):
- Seção "Agentes primários" (eyebrow 10px uppercase `#6a6a6a` + hint "Tab ⇄"). Linhas de agente: dot de status, nome (600), tag (`primary`/`subagent`), descrição truncada. Ativo: borda `rgba(250,178,131,0.35)`, fundo `rgba(250,178,131,0.07)`, dot `#fab283` com glow.
- Seção "Subagentes · @menção": linha compacta com `@` accent, nome, tag on/off (verde `#7fb069` / faint).
- Rodapé "Sessões": lista com dot, título truncado, timestamp.

**Sidebar Admin** (nav vertical, padding 14px 10px): itens com ícone (glyph), label, badge opcional (ex.: Provedores com badge `1` amarelo). Item ativo: borda `#262626`, fundo `#161616`. Card fixo no rodapé "Precedência" com a legenda dos 7 níveis (quadradinho colorido + label + 🔒 para read-only).

---

### 2. Workspace

**Propósito:** o usuário conversa com o agente ativo; vê tool-calls, status de snapshot; dispara comandos.

**Context strip** (46px, abaixo da topbar): dot do agente, nome (600), tag, descrição, modelo (`◍ accent`), status de snapshot (`⊟ snapshot · N checkpoints`, verde quando on / vermelho quando off).

**Lista de mensagens** (coluna central `max-width:780px`, gap 18px):
- **User:** avatar 24×24 (`VC`, `#1c1c1c`), bolha `#141414` borda `#232323` radius 8px.
- **Assistant:** avatar com mini-logo âmbar; texto `#d6d6d6`; checklist opcional (✓ verde + item).
- **Tool card:** indentado 35px, `#0e0e0e` borda `#1e1e1e` radius 7px; badge da tool colorido (`READ` accent, `EDIT` primary, `BASH` green — texto `#0a0a0a` sobre a cor), alvo (arquivo), meta (`+9 −0`, `2 passed`), ✓ verde.

**Composer** (rodapé, `max-width:780px`):
- Caixa `#111111` borda dinâmica (`#242424` → `#3a3a3a` quando há texto), radius 11px. Prompt glyph `›` (primary) ou `/` (accent quando comando). Input transparente 13.5px.
- Barra inferior: quick actions (`/init`, `/share`, `/undo`, `/redo` — pílulas borda `#262626`), botão anexar (`⊕`), info de contexto (`context 18.2k / 200k`), botão **Enviar** (`#fab283`, texto `#1a1206`, ⏎).

**Command palette** (aparece ao digitar `/`): popover acima do composer, `#121212` borda `#2c2c2c` radius 9px, sombra `0 14px 40px rgba(0,0,0,.6)`. Cabeçalho com dica "↑↓ navegar · ⏎ executar · Tab completar". Itens: `/nome` (primary, ou accent se custom + badge `custom`), descrição, filtrados pelo que foi digitado.

---

### 3. Admin — Dashboard

**Stat cards** (grid 4 colunas, gap 12px): label uppercase, valor grande (24px, cor temática), unidade, sub-linha. Ex.: Agentes `5 · 2 primary`; Provedores `N conectados`; Snapshots `7 checkpoints` (verde/vermelho); Comandos `12 · 4 custom`.

**Provedores** (card 1.3fr): linhas com dot de status (verde conectado / faint sem credencial), nome, modelos, status textual.

**Avisos & conflitos** (card 1fr): alertas com ícone colorido, título, corpo. Tipos: ▲ amarelo (sem credencial), ◆ vermelho (política conflitante), ■ accent (valor managed read-only).

**Cascata de configuração** (destaque): trilha vertical dos 7 níveis para um campo (ex.: `model`). Cada nível: trilho conector + nó quadrado colorido + card com code (REMOTE…MANAGED), label, valor e tag. O nível vencedor: borda/fundo verde, "✓ valor efetivo"; níveis sobrescritos: valor riscado, "sobrescrito"; não definidos: faint.

---

### 4. Admin — Configuração geral / Servidor / TUI

Formulário em card (`#0f0f0f` borda `#1f1f1f` radius 11px). Cada linha: label + hint à esquerda; **badge de nível** (quadradinho + code colorido) no meio; valor (pílula `#0c0c0c` borda `#242424`) + 🔒 se managed. Campos fiéis ao schema: `model`, `small_model`, `theme`, `autoupdate` (managed/read-only), `snapshot`, `share`, `formatter`, `instructions`; servidor: `port 4096`, `hostname`, `mdns`, `cors.origins`, `auth`; tui: `tui.theme`, `keybinds.leader`, etc.

---

### 5. Admin — Agentes + Editor

**Lista:** linhas com dot de status, nome + tag (`primary`/`subagent`), descrição, modelo (`◍`), badge de nível de origem, chevron. Botão "+ Novo agente".

**Editor** (abas **Visual | Markdown**):
- *Visual* — coluna esq: Identidade & modelo (modelo, **temperatura** slider `accent-color:#fab283` 0–1, max steps, toggles Subtask/Habilitado). Coluna dir: **Permissões** (linhas clicáveis que ciclam `allow → ask → deny`, com cor verde/amarelo/vermelho) e **Ferramentas** (chips on/off). Abaixo: **System prompt** em bloco.
- *Markdown* — render do arquivo `.opencode/agents/<nome>.md` com frontmatter YAML (`description`, `mode`, `model`, `temperature`, `tools`, `permission`) + corpo. Header do arquivo com caminho.

Permissões/temperatura/enabled são **estado editável** e refletem no markdown.

---

### 6. Admin — Comandos + Editor (com Playground)

**Lista:** `/nome`, descrição, agente executante, badge de nível. Botão "+ Novo comando".

**Editor** (abas Visual | Markdown), layout 2 colunas:
- Esq: Execução (agente, modelo override, subtask) + **Template** (bloco mono).
- Dir: **Playground** (destaque, borda âmbar). Input de "argumentos de exemplo" com prefixo `/nome`. Abaixo, **Prompt resolvido** — o template com placeholders substituídos e **destacados por cor**:
  - `$ARGUMENTS` / `$1 $2…` → verde (`#7fb069`, fundo `rgba(127,176,105,0.14)`); posicional ausente → vermelho.
  - `@arquivo` → azul (`#5a9cf8`).
  - `` !`comando` `` → accent (`#9d7cd8`).
- Card "Placeholders" com a legenda dos 4 tipos.

A resolução é dinâmica conforme o usuário digita os argumentos.

---

### 7. Admin — Provedores

Cards expansíveis: header com dot de status (conectado/sem credencial/inválido/testando), nome, badge transport-nível, modelos, botão **Testar** (simula → conecta). Corpo: grid de campos por provedor (`apiKey` mascarada `•••• ····9a2f`, `timeout`, `region`, `profile`, `endpoint`, `oauth`). Provedores reais: Anthropic, GitHub Copilot, OpenAI, Amazon Bedrock, Google Vertex, OpenRouter.

---

### 8. Admin — Políticas

Tabela (grid `90px 90px 1fr 70px`): **Efeito** (allow/deny/ask — pílula colorida), **Ação** (`bash`/`edit`/`webfetch`), **Recurso** (glob), **Prioridade**. Maior prioridade vence.

---

### 9. Admin — Snapshots (timeline undo/redo)

Banner de estado (verde habilitado / vermelho desabilitado) com toggle. **Timeline vertical**: cada checkpoint com conector, glyph de status (● aplicado / ⊘ revertido), `#id`, label, agente · arquivos, tag de status, tempo. Botões **Undo** (↶ primary) e **Redo** (↷). Undo reverte o último aplicado (decrementa checkpoints, incrementa redo); Redo reaplica.

---

### 10. Admin — Compartilhados

Lista de conversas com link `/share`: dot, título + URL (`opencode.ai/s/…`), acessos, status (público/revogado/privado), tempo, botões Copiar/Revogar.

---

### 11. Extensões — Plugins / MCP / Integrações

Arquivo: `designs/Plugins e Integracoes.dc.html`. **Fiel à arquitetura da Fase 5 do `rust-migration`:** core Rust ⇄ **RPC/JSON local** ⇄ **JS plugin host out-of-process (Bun)**; MCP via crate `rmcp`; integrações como adapters in-core.

**Strip de arquitetura** (topo): `opencode-core (Rust)` — `local RPC/JSON` — `JS plugin host` — `MCP · rmcp`.

**Aba Plugins:**
- **Pipeline de hooks** (assinatura visual): um turno de sessão com os hooks reais em sequência — `chat.message → chat.params → chat.headers → system.transform → [LLM stream] → tool.definition → permission.ask → tool.execute.before → tool.execute.after → event`. Cada plugin "tapa" pontos do pipeline (marcadores coloridos). **Selecionar um plugin destaca onde ele injeta** e atenua o resto.
- **Cards de plugin:** ícone, nome, badge de source (npm/local), descrição, **toggle**, chips dos **hooks** que registra (coloridos por família), status do host, options resumidas, botão **Detalhes ›**.

**Aba MCP:** servers com dot de status, **transport** (stdio/http/sse), comando, status; expansível mostrando **tools expostas** e estado de **reconnect/`onsessionexpired`** (animado) — o patch portado do TS.

**Aba Integrações:** cards GitHub/GitLab/Slack (adapters in-core) com status, capabilities, botão Conectar/Gerenciar. **Nota explícita:** o bot `packages/slack` standalone permanece em TS e fala pelo mesmo `/openapi.json`.

**Drawer de detalhe do plugin** (desliza da direita, 472px): header (status/versão/`dispose()`), **Origem** (entry exata do array `plugin`: `"name"` ou `["name", options]` + caminho resolvido), **PluginOptions** editáveis (toggle p/ bool, valor p/ texto), abas **Hooks** (cada hook com assinatura `INPUT`/`OUTPUT` real) e **Log de execução**, e **PluginInput · acesso concedido** (`client`, `$`, `project`, `directory`, `worktree`, `serverUrl` — ✓/· por uso). Footer: Remover · Recarregar host · Salvar.

**Fluxo de Install** (modal 3 passos): **① Origem** (toggle npm/local + sugestões do registry) → **② Resolvendo** (spinner) → **③ Revisão** com **gate de capacidades** (cada capacidade pedida classificada por risco: `$`/BunShell = ALTO vermelho, `client`/`directory` = médio, `serverUrl` = baixo, transform puro = seguro). Aviso vermelho se pedir shell. Mostra hooks que vai registrar, options e o resultado no config. Confirmar → "⎉ Instalar & recarregar host" adiciona o card.

---

### 12. Configurações › Aparência (seletor de tema)

Arquivo: `designs/Configuracoes Aparencia.dc.html`. Persiste no campo `theme` do `opencode.json`.

**Catálogo (esquerda, 430px):** busca + temas agrupados em **Nativo · Claras · Nobres · Natureza**. Cada linha: mini-swatch (faixa bg + primary/accent), nome, badge de modo (dark/light), dots de cor, ✓ se selecionado. **Argila** marcado `default`.

**Preview ao vivo (direita):** aplica o tema selecionado a um recorte real da UI (topbar, sidebar de agentes, **régua dos 7 níveis de precedência**, mensagem + tool card, status, composer, faixa de tokens). Controles: toggle **Modo (Auto/Dark/Light)** e botão **Aplicar tema** (→ "✓ Aplicado").

---

## Interactions & Behavior

- **Navegação:** topbar Workspace↔Admin; nav admin troca a view; editores têm botão "← voltar".
- **Command palette:** abre ao digitar `/`; filtra por substring; clicar/Tab completa; `/undo`,`/redo`,`/share` têm efeito.
- **Permissões (agente):** clique cicla `allow → ask → deny` (verde→amarelo→vermelho).
- **Temperatura:** slider 0–1 step 0.1, atualiza valor e markdown.
- **Playground de comando:** digitar argumentos re-resolve o template com destaque por cor em tempo real.
- **Testar provedor:** estado `testando…` (amarelo) por ~1.1s → `conectado` (verde).
- **Snapshots:** Undo/Redo mutam a timeline e os contadores.
- **Pipeline de hooks:** selecionar plugin destaca seus taps; toggles habilitam/desabilitam.
- **MCP:** toggle conecta/desconecta; server com `reconnecting` anima o aviso de sessão expirada.
- **Install:** Resolver (desabilitado sem origem) → spinner 900ms → Revisão → Instalar adiciona card e fecha modal; contador da aba incrementa.
- **Tema:** selecionar re-tematiza o preview instantaneamente; Aplicar fixa como ativo.
- **Animações:** fade/rise sutis (120–250ms, `cubic-bezier(.4,0,.2,1)`); spinners `rotate` 0.8–1.8s; pulse de status 1.4–2s. Sem transições longas.

---

## State Management

Estado por tela (recriar com o gerenciador do codebase — signals no Solid, hooks no React):
- **Shell:** `mode` (workspace/admin), `adminView`, `theme`.
- **Workspace:** `activeAgent`, `input`, `snapshotsEnabled`, `checkpoints`, `redo`, `shareActive`, lista de mensagens (seed + extras), `timeline`.
- **Agentes:** `editingAgentId`, `agentTab`, `perms[agent][tool]`, `temp[agent]`, `enabled[agent]`.
- **Comandos:** `editingCmdId`, `cmdTab`, `cmdArgs` (resolução do template é derivada).
- **Provedores:** `providers[id]` (connected/idle/error), `testingProvider`.
- **Extensões:** `tab`, `selected`, `pluginEnabled{}`, `mcpEnabled{}`, `drawerOpen/drawerPlugin/drawerTab`, `opts{}`, `installOpen/installStep/installKind/installSrc/installTarget`, `installed[]`.
- **Aparência:** `selected`, `mode`, `applied`, `query`.

**Data fetching (na implementação real):** ler/escrever `opencode.json`/config via API; listar agentes/comandos/provedores/plugins/MCP; stream de eventos por SSE `GET /api/event` (atualiza status, snapshots, tool-calls); `POST /api/session/{id}/prompt` para enviar mensagens.

---

## Design Tokens

### Tipografia
- Família: `IBM Plex Mono` (fallback `ui-monospace, SFMono-Regular, Menlo, monospace`). Pesos 400/500/600/700.
- Escala (px): 24 (stat values) · 21/18/17 (títulos) · 14/13.5/13 (corpo/labels fortes) · 12.5/12/11.5 (secundário) · 10.5/10/9.5 (eyebrows, metas). `line-height` 1.5 base. Eyebrows uppercase com `letter-spacing` 0.6–1.2px.

### Raios
- Cards 9–14px · botões/inputs 6–7px · chips/badges 4–6px · swatches 2–5px · dots 50%.

### Sombras
- Popover/palette: `0 14px 40px rgba(0,0,0,.6)`.
- Drawer: `-30px 0 60px rgba(0,0,0,.5)`. Modal: `0 30px 70px rgba(0,0,0,.6)`. Preview card: `0 20px 60px rgba(0,0,0,.5)`.

### Tema base `opencode` (dark) — usado no OpenCode Web.dc.html
| Token | Hex |
|---|---|
| bg | `#0a0a0a` |
| surface | `#101010` |
| elev | `#141414` |
| border | `#242424` / `#1c1c1c` (muted) |
| ink | `#eeeeee` |
| secondary | `#cfcfcf` |
| muted | `#9a9a9a` |
| faint | `#5a5a5a` |
| **primary** | `#fab283` (ink sobre primary: `#1a1206`) |
| **accent** | `#9d7cd8` |
| green | `#7fb069` |
| red | `#e06c75` |
| yellow | `#e6c07b` |
| blue | `#5a9cf8` |
| orange | `#e09a4b` |
| brown | `#ac8e68` |

### 7 níveis de precedência (ordem base→topo) — cores e semântica
| Code | Label | Cor | Fonte | Lock |
|---|---|---|---|---|
| REMOTE | Remote | `#5a9cf8` (blue) | `.well-known/opencode` | read-only |
| GLOBAL | Global | `#e6c07b` (yellow) | `~/.config/opencode` | — |
| CUSTOM | Custom path | `#e09a4b` (orange) | `OPENCODE_CONFIG` | — |
| PROJECT | Projeto | `#7fb069` (green) | `opencode.json` | — |
| .OPENCODE | .opencode/ | `#9d7cd8` (accent) | agents · commands | — |
| INLINE | Inline | `#ac8e68` (brown) | `OPENCODE_CONFIG_CONTENT` | read-only |
| MANAGED | Managed | `#e06c75` (red) | `/etc/opencode · MDM` | read-only |

> Esses códigos/cores são reutilizados como **badge de origem** em todos os formulários (de onde cada valor vem) e na trilha de cascata do Dashboard.

### Catálogo de temas (13) — cada um define dark **ou** light
Cada tema tem o conjunto completo: `bg, surf, elev, bd, ink, sec, mut, faint, primary, primaryInk, accent, green, red, yellow, markDim` + array `levels[7]` (cores dos níveis de precedência). Valores completos estão no logic de `Configuracoes Aparencia.dc.html` (método `themes()`). Resumo por família:

**Nativo**
- `opencode` (dark) — `bg #0a0a0a`, primary `#fab283`, accent `#9d7cd8`.

**Claras**
- `Paper` (light) — `bg #ffffff`, primary `#3b7dd8`, accent `#d68c27`.
- `Pergaminho` (light) — `bg #faf7f0`, primary `#b5762e`, accent `#7a5ba8`.
- `Ardósia` (light) — `bg #f5f7fa`, primary `#5e81ac`, accent `#9b6fa6`.

**Nobres**
- `Midnight Royal` (dark) — `bg #0c1322`, primary `#d4af37`, accent `#8d7ddb`.
- `Emerald Noir` (dark) — `bg #08120e`, primary `#c9a85c`, accent `#5fc596`.
- `Bordeaux` (dark) — `bg #160a10`, primary `#c9a25f`, accent `#cf6f87`.
- `Platinum Graphite` (dark) — `bg #16181b`, primary `#cdd4dc`, accent `#9bb0c4`.

**Natureza**
- `Floresta` (dark) — `bg #0b1410`, primary `#d9a94e`, accent `#a884c8`.
- `Micélio` (dark) — `bg #13100b`, primary `#e0a85c`, accent `#b08cd0`.
- `Recife` (dark) — `bg #07141a`, primary `#df8a5c`, accent `#4fc2cb`.
- `Musgo` (light) — `bg #edf0e4`, primary `#6d7a36`, accent `#8a6aa6`.
- **`Argila` (light) — DEFAULT** — `bg #f4ede2`, surf `#ece2d2`, elev `#fdf8ef`, bd `#e0d3bf`, ink `#382c20`, sec `#54442f`, mut `#837058`, faint `#ab9a80`, primary `#bf6a3c` (ink `#fdf6ec`), accent `#6e8a5a`, green `#5e8a48`, red `#b1503c`, yellow `#b0852c`. levels: `['#4a78a0','#b0852c','#bf6a3c','#5e8a48','#7d6aa0','#8a6e48','#b1503c']`.

> Implementação sugerida: tema = objeto de tokens; aplicar via CSS custom properties (`--bg`, `--primary`, …) no root. O `mode` (auto/dark/light) seleciona a variante; `auto` segue `prefers-color-scheme`. **Default do app = `Argila`.**

---

## Assets

- **Logo "nested square"** (inline SVG, sem arquivo externo) — dois quadrados concêntricos:
  ```html
  <svg viewBox="0 0 24 30">
    <path d="M18 24H6V12H18V24Z" fill="<cor-fraca>"></path>
    <path d="M18 6H6V24H18V6ZM24 30H0V0H24V30Z" fill="<primary>"></path>
  </svg>
  ```
  O quadrado interno usa uma cor fraca (`markDim`/`#3a3a3a`), o externo usa o `primary` do tema. No repo, ver `packages/console/app/src/asset/brand/` e `logo.tsx`.
- **Ícones:** todos são **glyphs Unicode** (`◍ ◇ ◳ ▦ ⊟ ↗ ⎉ ⇄ ◈ ↶ ↷ ✓ ⚠ ▷` etc.), não há set de ícones bitmap. Substituir pelo icon set do codebase mantendo o peso "linha fina/mono".
- **Sem imagens raster.** Nenhuma foto/screenshot usada na UI.
- **Fonte:** IBM Plex Mono (Google Fonts no protótipo; no codebase usar Berkeley Mono se licenciado, senão IBM Plex Mono).

---

## Files

Em `designs/` (referências — abrir no navegador para interagir):
- `OpenCode Web.dc.html` — Workspace + Admin completo (dashboard, geral, agentes+editor, comandos+editor/playground, provedores, políticas, servidor, tui, snapshots, compartilhados, cascata de precedência).
- `Estados de Runtime.dc.html` — §5 do relatório de gaps: permissão, pergunta, terminal, diff, onboarding, seletor de modelo, empty states, erro/conexão, indisponível. **Foco da Fase 1.**
- `Contrato de Proveniencia.dc.html` — §5.10: spec visual + JSON de `config.resolve` (cascata dos 7 níveis). Contrato design ↔ backend.
- `Plugins e Integracoes.dc.html` — Plugins/MCP/Integrações + pipeline de hooks + drawer de detalhe + fluxo de install.
- `Configuracoes Aparencia.dc.html` — seletor de tema com preview ao vivo (13 temas, Argila default).
- `Paletas.dc.html` — explorador das 4 famílias de paleta em mini-mockups (referência de tokens).
- `support.js` — runtime da ferramenta de design (**não portar**; necessário só para abrir os `.dc.html`).

### Referência do codebase real (branch `rust-migration`)
- Tokens existentes: `packages/console/app/src/style/token/{color,font,space}.css`.
- Componentes: `packages/console/app/src/style/component/button.css`.
- Schema de temas: `desktop-theme.schema.json` (dark/light + 38 presets).
- Contrato de plugins: `packages/plugin/src/index.ts` (interface `Hooks`, `PluginInput`).
- Crates Fase 5: `crates/opencode-plugin`, `crates/opencode-integration` (host JS out-of-process, `rmcp`).
- API: `/openapi.json`, `GET /api/event` (SSE), `POST /api/session/{id}/prompt`.

---

## Notas finais para o desenvolvedor

1. **Não copie o HTML/runtime.** Recrie no stack do codebase (SolidJS de referência) usando tokens/componentes existentes.
2. **Tema é a fundação.** Implemente o sistema de tokens via CSS custom properties primeiro; tudo deriva dele. Default = **Argila**; suporte dark/light e os 38 presets existentes.
3. **Mono em tudo.** Não introduza fontes sans/serif.
4. **Precedência é o coração do produto.** Os 7 níveis (cores + badges + cascata) aparecem em várias telas — centralize num componente reutilizável.
5. **Extensões = arquitetura real.** Plugins de terceiros rodam no host JS out-of-process; o gate de capacidades no install é um requisito de confiança, não enfeite. MCP usa `rmcp` com reconnect.
6. **Sem login/tenant/billing** — fora de escopo por design. Se um dia virar "Cloud", é um track separado.
