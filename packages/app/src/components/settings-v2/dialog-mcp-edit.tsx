import { Button } from "@opencode-ai/ui/button"
import { useDialog } from "@opencode-ai/ui/context/dialog"
import { Dialog } from "@opencode-ai/ui/dialog"
import { TextField } from "@opencode-ai/ui/text-field"
import { useMutation } from "@tanstack/solid-query"
import { Show } from "solid-js"
import { createStore } from "solid-js/store"
import type { McpLocalConfig, McpRemoteConfig } from "@opencode-ai/sdk/v2"
import { showToast } from "@/utils/toast"
import { useLanguage } from "@/context/language"
import { useServerSDK } from "@/context/server-sdk"

/** A configured MCP server: its name (the `config.mcp` key) and its raw config value. */
export type McpServerEntry = {
  name: string
  config: McpLocalConfig | McpRemoteConfig | { enabled: boolean }
}

type Props = {
  /** When provided, edits an existing server (name is fixed); otherwise creates a new one. */
  server?: McpServerEntry
  /** Called after a successful save so the caller can refetch. */
  onSaved?: () => void
}

type FormState = {
  name: string
  type: "local" | "remote"
  command: string
  cwd: string
  url: string
  headers: string
  enabled: boolean
  err: { name?: string; command?: string; url?: string }
}

/** Split a command line into argv on whitespace (no shell quoting — use the config file for that). */
function parseCommand(value: string): string[] {
  return value.trim().split(/\s+/).filter(Boolean)
}

/** Parse `Key: Value` lines into a headers object, skipping blanks and malformed lines. */
function parseHeaders(value: string): Record<string, string> {
  const out: Record<string, string> = {}
  for (const line of value.split("\n")) {
    const trimmed = line.trim()
    if (!trimmed) continue
    const colon = trimmed.indexOf(":")
    if (colon <= 0) continue
    const key = trimmed.slice(0, colon).trim()
    if (key) out[key] = trimmed.slice(colon + 1).trim()
  }
  return out
}

/** Render a headers object back to `Key: Value` lines for the editor. */
function stringifyHeaders(headers?: Record<string, string>): string {
  if (!headers) return ""
  return Object.entries(headers)
    .map(([k, v]) => `${k}: ${v}`)
    .join("\n")
}

function initialForm(server?: McpServerEntry): FormState {
  const cfg = server?.config
  const typed = cfg && "type" in cfg ? cfg : undefined
  const type: "local" | "remote" = typed?.type === "remote" ? "remote" : "local"
  return {
    name: server?.name ?? "",
    type,
    command: typed?.type === "local" ? typed.command.join(" ") : "",
    cwd: typed?.type === "local" ? (typed.cwd ?? "") : "",
    url: typed?.type === "remote" ? typed.url : "",
    headers: typed?.type === "remote" ? stringifyHeaders(typed.headers) : "",
    enabled: !cfg || cfg.enabled !== false,
    err: {},
  }
}

/**
 * Create or edit an MCP server (writes `config.mcp.<name>` via `config.update`, which deep-merges into
 * `opencode.json`). Supports local (stdio) servers — a command + optional working directory — and remote
 * (HTTP) servers — a url + optional `Key: Value` headers (e.g. a bearer token). The enabled toggle and
 * every field are merged in; removing a server (or a single header) is done by editing the file.
 */
export function DialogMcpEdit(props: Props) {
  const dialog = useDialog()
  const language = useLanguage()
  const serverSDK = useServerSDK()
  const editing = !!props.server

  const [form, setForm] = createStore<FormState>(initialForm(props.server))

  const validate = () => {
    const err: FormState["err"] = {}
    if (!form.name.trim()) err.name = language.t("settings.mcp.edit.error.name")
    if (form.type === "local" && parseCommand(form.command).length === 0)
      err.command = language.t("settings.mcp.edit.error.command")
    if (form.type === "remote" && !form.url.trim()) err.url = language.t("settings.mcp.edit.error.url")
    setForm("err", err)
    return Object.keys(err).length === 0
  }

  const saveMutation = useMutation(() => ({
    mutationFn: async () => {
      const name = form.name.trim()
      let def: McpLocalConfig | McpRemoteConfig
      if (form.type === "local") {
        const local: McpLocalConfig = { type: "local", command: parseCommand(form.command), enabled: form.enabled }
        if (form.cwd.trim()) local.cwd = form.cwd.trim()
        def = local
      } else {
        const remote: McpRemoteConfig = { type: "remote", url: form.url.trim(), enabled: form.enabled }
        const headers = parseHeaders(form.headers)
        if (Object.keys(headers).length) remote.headers = headers
        def = remote
      }
      await serverSDK.client.config.update({ config: { mcp: { [name]: def } } })
    },
    onSuccess: () => {
      dialog.close()
      showToast({
        variant: "success",
        icon: "circle-check",
        title: language.t("settings.mcp.edit.saved", { name: form.name.trim() }),
      })
      props.onSaved?.()
    },
    onError: (err) => {
      const message = err instanceof Error ? err.message : String(err)
      showToast({ title: language.t("common.requestFailed"), description: message })
    },
  }))

  const save = (e: SubmitEvent) => {
    e.preventDefault()
    if (saveMutation.isPending) return
    if (!validate()) return
    saveMutation.mutate()
  }

  return (
    <Dialog
      title={editing ? language.t("settings.mcp.edit.titleEdit") : language.t("settings.mcp.edit.titleNew")}
      transition
    >
      <form onSubmit={save} class="flex flex-col gap-5 px-2.5 pb-4 overflow-y-auto max-h-[60vh]">
        <TextField
          autofocus={!editing}
          disabled={editing}
          label={language.t("settings.mcp.edit.field.name")}
          placeholder="filesystem"
          value={form.name}
          onChange={(v) => setForm("name", v)}
          validationState={form.err.name ? "invalid" : undefined}
          error={form.err.name}
        />
        <div class="flex flex-col gap-1.5">
          <span class="text-14-regular text-text-base">{language.t("settings.mcp.edit.field.type")}</span>
          <div class="flex items-center gap-4">
            <label class="flex items-center gap-2 text-14-regular text-text-base">
              <input
                type="radio"
                name="mcp-type"
                checked={form.type === "local"}
                onChange={() => setForm("type", "local")}
              />
              {language.t("settings.mcp.edit.type.local")}
            </label>
            <label class="flex items-center gap-2 text-14-regular text-text-base">
              <input
                type="radio"
                name="mcp-type"
                checked={form.type === "remote"}
                onChange={() => setForm("type", "remote")}
              />
              {language.t("settings.mcp.edit.type.remote")}
            </label>
          </div>
        </div>
        <Show when={form.type === "local"}>
          <TextField
            label={language.t("settings.mcp.edit.field.command")}
            placeholder="npx -y @modelcontextprotocol/server-filesystem /tmp"
            value={form.command}
            onChange={(v) => setForm("command", v)}
            validationState={form.err.command ? "invalid" : undefined}
            error={form.err.command}
          />
          <TextField
            label={language.t("settings.mcp.edit.field.cwd")}
            value={form.cwd}
            onChange={(v) => setForm("cwd", v)}
          />
        </Show>
        <Show when={form.type === "remote"}>
          <TextField
            label={language.t("settings.mcp.edit.field.url")}
            placeholder="https://example.com/mcp"
            value={form.url}
            onChange={(v) => setForm("url", v)}
            validationState={form.err.url ? "invalid" : undefined}
            error={form.err.url}
          />
          <TextField
            multiline
            label={language.t("settings.mcp.edit.field.headers")}
            placeholder="Authorization: Bearer TOKEN"
            value={form.headers}
            onChange={(v) => setForm("headers", v)}
          />
        </Show>
        <label class="flex items-center gap-2 text-14-regular text-text-base">
          <input
            type="checkbox"
            checked={form.enabled}
            onChange={(e) => setForm("enabled", e.currentTarget.checked)}
          />
          {language.t("settings.mcp.edit.field.enabled")}
        </label>
        <Button type="submit" size="large" variant="primary" class="self-start" disabled={saveMutation.isPending}>
          {saveMutation.isPending ? language.t("common.saving") : language.t("common.save")}
        </Button>
      </form>
    </Dialog>
  )
}
