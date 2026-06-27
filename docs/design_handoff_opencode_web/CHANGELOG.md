# CHANGELOG — Handoff OpenCode Web

Registro do que cada entrega adicionou, para o dev identificar rápido o que é novo desde a primeira validação de gaps.

---

## v3 — Contrato de proveniência (atual)

**Novo arquivo**
- `designs/Contrato de Proveniencia.dc.html` — fecha o **§5.10** do relatório de gaps (o último item de design pendente).

**O que é:** a spec dupla design ↔ backend para a cascata de configuração dos 7 níveis.
- Painel **A (UI)**: como cada campo renderiza a partir da resposta resolvida — badge = `source`, valor = `effective`, riscado = nível sobrescrito, 🔒 = `locked`, e o seletor de nível-destino ao gravar.
- Painel **B (dados)**: o JSON exato que `config.resolve` deve devolver, com 5 casos navegáveis: `model` (vencedor no meio), `theme` (global vence), `small_model` (não-definido), `autoupdate` (managed/locked → `409`), `permission.bash` (objeto mesclado).

**Docs atualizados**
- `IMPLEMENTATION.md` §3 — aponta para a spec visual como contrato definitivo da Fase 3.
- `README.md` — índice de arquivos com a nova tela.

**Cobertura:** com isto, **100% dos itens do §5 do relatório estão entregues**. Auth/login (§5.11) permanece fora de escopo por design (local-first).

---

## v2 — Estados de runtime (§5.1–5.9)

**Novo arquivo**
- `designs/Estados de Runtime.dc.html` — as telas/estados que faltavam, foco da Fase 1: permissão runtime, pergunta do agente, terminal (PTY), diff/file viewer, onboarding/first-run, seletor de modelo mid-session, empty states, erro/conexão (retry, offline, 409, SSE) e estado "indisponível" para recursos stubbed.

**Novo arquivo**
- `IMPLEMENTATION.md` — guia faseado para o Claude Code: regras de ouro, matriz de verdade (design × `packages/app` × Rust), Fases F1–F4, contrato de proveniência (shape) e paths reais de FE/BE.

---

## v1 — Pacote inicial

**Arquivos**
- `README.md` — sistema visual, tokens completos, descrição tela-a-tela.
- `designs/OpenCode Web.dc.html` — Workspace + Admin completo (dashboard, geral, agentes+editor, comandos+editor/playground, providers, políticas, servidor, tui, snapshots, shared, cascata de precedência).
- `designs/Plugins e Integracoes.dc.html` — Plugins/MCP/Integrações + pipeline de hooks + drawer + install.
- `designs/Configuracoes Aparencia.dc.html` — seletor de 13 temas + preview ao vivo (Argila default).
- `designs/Paletas.dc.html` — explorador das 4 famílias de paleta.
- `designs/support.js` — runtime da ferramenta de design (**não portar**).

---

## Como ler este handoff (ordem sugerida)

1. **`IMPLEMENTATION.md`** — o plano: o que construir, em que ordem, contra qual API.
2. **`README.md`** — a referência de design: tokens, telas, comportamento.
3. **`designs/*.dc.html`** — abrir no navegador para interagir; ler o markup para extrair valores exatos.
4. **`CHANGELOG.md`** (este) — o que é novo em cada rodada.
