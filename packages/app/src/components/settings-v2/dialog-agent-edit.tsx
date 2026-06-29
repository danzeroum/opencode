import { Button } from "@opencode-ai/ui/button"
import { useDialog } from "@opencode-ai/ui/context/dialog"
import { Dialog } from "@opencode-ai/ui/dialog"
import { TextField } from "@opencode-ai/ui/text-field"
import { useMutation } from "@tanstack/solid-query"
import { For } from "solid-js"
import { createStore } from "solid-js/store"
import type { AgentV2Info } from "@opencode-ai/sdk/v2"
import { showToast } from "@/utils/toast"
import { useLanguage } from "@/context/language"
import { useServerSDK } from "@/context/server-sdk"

type AgentMode = "subagent" | "primary" | "all"
const MODES: AgentMode[] = ["all", "primary", "subagent"]

type Props = {
  /** When provided, edits an existing agent (id is fixed); otherwise creates a new one. */
  agent?: AgentV2Info
  /** Called after a successful save so the caller can refetch the list. */
  onSaved?: () => void
}

type FormState = {
  name: string
  description: string
  mode: AgentMode
  model: string
  system: string
  color: string
  steps: string
  hidden: boolean
  err: { name?: string }
}

/** Parse a `provider/model` string into the SDK's `{ providerID, id }`, or `undefined` if blank/invalid. */
function parseModel(value: string): { providerID: string; id: string } | undefined {
  const trimmed = value.trim()
  if (!trimmed) return undefined
  const slash = trimmed.indexOf("/")
  if (slash <= 0 || slash >= trimmed.length - 1) return undefined
  return { providerID: trimmed.slice(0, slash), id: trimmed.slice(slash + 1) }
}

/**
 * Create or edit an agent (`v2.agent.set`). Renders the writable fields — name (the agent id, fixed when
 * editing), description, mode, model (`provider/model`), the system prompt, display color, step limit,
 * and the hidden flag — and writes them to `.opencode/agent/<name>.md`.
 */
export function DialogAgentEdit(props: Props) {
  const dialog = useDialog()
  const language = useLanguage()
  const serverSDK = useServerSDK()
  const editing = !!props.agent

  const [form, setForm] = createStore<FormState>({
    name: props.agent?.id ?? "",
    description: props.agent?.description ?? "",
    mode: (props.agent?.mode as AgentMode) ?? "all",
    model: props.agent?.model ? `${props.agent.model.providerID}/${props.agent.model.id}` : "",
    system: props.agent?.system ?? "",
    color: props.agent?.color ?? "",
    steps: props.agent?.steps != null ? String(props.agent.steps) : "",
    hidden: props.agent?.hidden ?? false,
    err: {},
  })

  const validate = () => {
    const err: FormState["err"] = {}
    if (!form.name.trim()) err.name = language.t("settings.agents.edit.error.name")
    setForm("err", err)
    return Object.keys(err).length === 0
  }

  const saveMutation = useMutation(() => ({
    mutationFn: async () => {
      const steps = form.steps.trim() ? Number(form.steps.trim()) : undefined
      await serverSDK.client.v2.agent.set({
        agentID: form.name.trim(),
        description: form.description.trim() || undefined,
        mode: form.mode,
        model: parseModel(form.model),
        system: form.system.trim() || undefined,
        color: form.color.trim() || undefined,
        steps: steps != null && Number.isFinite(steps) ? steps : undefined,
        hidden: form.hidden || undefined,
      })
    },
    onSuccess: () => {
      dialog.close()
      showToast({
        variant: "success",
        icon: "circle-check",
        title: language.t("settings.agents.edit.saved", { name: form.name.trim() }),
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
      title={editing ? language.t("settings.agents.edit.titleEdit") : language.t("settings.agents.edit.titleNew")}
      transition
    >
      <form onSubmit={save} class="flex flex-col gap-5 px-2.5 pb-4 overflow-y-auto max-h-[60vh]">
        <TextField
          autofocus={!editing}
          disabled={editing}
          label={language.t("settings.agents.edit.field.name")}
          placeholder="reviewer"
          value={form.name}
          onChange={(v) => setForm("name", v)}
          validationState={form.err.name ? "invalid" : undefined}
          error={form.err.name}
        />
        <TextField
          label={language.t("settings.agents.edit.field.description")}
          value={form.description}
          onChange={(v) => setForm("description", v)}
        />
        <label class="flex flex-col gap-1.5">
          <span class="text-12-medium text-text-weak">{language.t("settings.agents.edit.field.mode")}</span>
          <select
            class="w-full rounded-md border px-3 py-2 text-14-regular bg-transparent"
            value={form.mode}
            onChange={(e) => setForm("mode", e.currentTarget.value as AgentMode)}
          >
            <For each={MODES}>{(m) => <option value={m}>{language.t(`settings.agents.mode.${m}`)}</option>}</For>
          </select>
        </label>
        <TextField
          label={language.t("settings.agents.edit.field.model")}
          placeholder="anthropic/claude-sonnet-4-5"
          value={form.model}
          onChange={(v) => setForm("model", v)}
        />
        <TextField
          multiline
          label={language.t("settings.agents.edit.field.system")}
          placeholder="You are a careful code reviewer."
          value={form.system}
          onChange={(v) => setForm("system", v)}
        />
        <TextField
          label={language.t("settings.agents.edit.field.color")}
          placeholder="#bf6a3c"
          value={form.color}
          onChange={(v) => setForm("color", v)}
        />
        <TextField
          label={language.t("settings.agents.edit.field.steps")}
          placeholder="20"
          value={form.steps}
          onChange={(v) => setForm("steps", v)}
        />
        <label class="flex items-center gap-2 text-14-regular text-text-base">
          <input
            type="checkbox"
            checked={form.hidden}
            onChange={(e) => setForm("hidden", e.currentTarget.checked)}
          />
          {language.t("settings.agents.edit.field.hidden")}
        </label>
        <Button type="submit" size="large" variant="primary" class="self-start" disabled={saveMutation.isPending}>
          {saveMutation.isPending ? language.t("common.saving") : language.t("common.save")}
        </Button>
      </form>
    </Dialog>
  )
}
