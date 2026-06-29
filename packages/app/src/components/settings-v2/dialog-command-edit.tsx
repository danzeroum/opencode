import { Button } from "@opencode-ai/ui/button"
import { useDialog } from "@opencode-ai/ui/context/dialog"
import { Dialog } from "@opencode-ai/ui/dialog"
import { TextField } from "@opencode-ai/ui/text-field"
import { useMutation } from "@tanstack/solid-query"
import { createStore } from "solid-js/store"
import type { CommandV2Info } from "@opencode-ai/sdk/v2"
import { showToast } from "@/utils/toast"
import { useLanguage } from "@/context/language"
import { useServerSDK } from "@/context/server-sdk"

type Props = {
  /** When provided, edits an existing command (name is fixed); otherwise creates a new one. */
  command?: CommandV2Info
  /** Called after a successful save so the caller can refetch the list. */
  onSaved?: () => void
}

type FormState = {
  name: string
  description: string
  agent: string
  model: string
  template: string
  subtask: boolean
  err: { name?: string; template?: string }
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
 * Create or edit a command (`v2.command.set`). Renders the writable fields — name (the command id, fixed
 * when editing), description, agent, model (`provider/model`), the markdown template, and the subtask
 * flag — and writes them to `.opencode/command/<name>.md`.
 */
export function DialogCommandEdit(props: Props) {
  const dialog = useDialog()
  const language = useLanguage()
  const serverSDK = useServerSDK()
  const editing = !!props.command

  const [form, setForm] = createStore<FormState>({
    name: props.command?.name ?? "",
    description: props.command?.description ?? "",
    agent: props.command?.agent ?? "",
    model: props.command?.model ? `${props.command.model.providerID}/${props.command.model.id}` : "",
    template: props.command?.template ?? "",
    subtask: props.command?.subtask ?? false,
    err: {},
  })

  const validate = () => {
    const err: FormState["err"] = {}
    if (!form.name.trim()) err.name = language.t("settings.commands.edit.error.name")
    if (!form.template.trim()) err.template = language.t("settings.commands.edit.error.template")
    setForm("err", err)
    return Object.keys(err).length === 0
  }

  const saveMutation = useMutation(() => ({
    mutationFn: async () => {
      await serverSDK.client.v2.command.set({
        commandID: form.name.trim(),
        template: form.template,
        description: form.description.trim() || undefined,
        agent: form.agent.trim() || undefined,
        model: parseModel(form.model),
        subtask: form.subtask || undefined,
      })
    },
    onSuccess: () => {
      dialog.close()
      showToast({
        variant: "success",
        icon: "circle-check",
        title: language.t("settings.commands.edit.saved", { name: form.name.trim() }),
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
      title={
        editing ? language.t("settings.commands.edit.titleEdit") : language.t("settings.commands.edit.titleNew")
      }
      transition
    >
      <form onSubmit={save} class="flex flex-col gap-5 px-2.5 pb-4 overflow-y-auto max-h-[60vh]">
        <TextField
          autofocus={!editing}
          disabled={editing}
          label={language.t("settings.commands.edit.field.name")}
          placeholder="deploy"
          value={form.name}
          onChange={(v) => setForm("name", v)}
          validationState={form.err.name ? "invalid" : undefined}
          error={form.err.name}
        />
        <TextField
          label={language.t("settings.commands.edit.field.description")}
          value={form.description}
          onChange={(v) => setForm("description", v)}
        />
        <TextField
          label={language.t("settings.commands.edit.field.agent")}
          value={form.agent}
          onChange={(v) => setForm("agent", v)}
        />
        <TextField
          label={language.t("settings.commands.edit.field.model")}
          placeholder="anthropic/claude-sonnet-4-5"
          value={form.model}
          onChange={(v) => setForm("model", v)}
        />
        <TextField
          multiline
          label={language.t("settings.commands.edit.field.template")}
          placeholder="Deploy the {{thing}}."
          value={form.template}
          onChange={(v) => setForm("template", v)}
          validationState={form.err.template ? "invalid" : undefined}
          error={form.err.template}
        />
        <label class="flex items-center gap-2 text-14-regular text-text-base">
          <input
            type="checkbox"
            checked={form.subtask}
            onChange={(e) => setForm("subtask", e.currentTarget.checked)}
          />
          {language.t("settings.commands.edit.field.subtask")}
        </label>
        <Button type="submit" size="large" variant="primary" class="self-start" disabled={saveMutation.isPending}>
          {saveMutation.isPending ? language.t("common.saving") : language.t("common.save")}
        </Button>
      </form>
    </Dialog>
  )
}
