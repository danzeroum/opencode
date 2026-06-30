import { Component, For, Show, createMemo, createResource, createSignal } from "solid-js"
import { ButtonV2 } from "@opencode-ai/ui/v2/button-v2"
import { Tag } from "@opencode-ai/ui/v2/badge-v2"
import { TextField } from "@opencode-ai/ui/text-field"
import { useParams } from "@solidjs/router"
import { showToast } from "@/utils/toast"
import { useLanguage } from "@/context/language"
import { useServerSDK } from "@/context/server-sdk"
import { useServerSync } from "@/context/server-sync"
import { decode64 } from "@/utils/base64"
import { SettingsListV2 } from "./parts/list"
import "./settings-v2.css"

/**
 * Policies management: saved permission rules (`v2.permission.saved.*`). Lists the stored
 * `{ action, resource }` rules, lets you add one (associated with the current project) and remove one.
 * A one-shot resource keeps it independent of the per-directory sync store.
 */
export const SettingsPoliciesV2: Component = () => {
  const language = useLanguage()
  const serverSdk = useServerSDK()
  const serverSync = useServerSync()
  const params = useParams()
  const [deleting, setDeleting] = createSignal<string | undefined>(undefined)
  const [saving, setSaving] = createSignal(false)
  const [action, setAction] = createSignal("")
  const [resource, setResource] = createSignal("")

  const currentProjectId = createMemo(() => {
    const dir = (() => {
      try {
        return decode64(params.dir)
      } catch {
        return ""
      }
    })()
    if (!dir) return ""
    return serverSync.data.project.find((p) => p.worktree === dir)?.id ?? ""
  })

  const [rules, { refetch }] = createResource(async () => {
    const res = await serverSdk.client.v2.permission.saved.list()
    return res.data?.data ?? []
  })

  const canAdd = () => !saving() && !!action().trim() && !!resource().trim()

  const add = async () => {
    if (!canAdd()) return
    setSaving(true)
    try {
      await serverSdk.client.v2.permission.saved.create({
        projectID: currentProjectId(),
        action: action().trim(),
        resource: resource().trim(),
      })
      setAction("")
      setResource("")
      showToast({ variant: "success", icon: "circle-check", title: language.t("settings.policies.added") })
      await refetch()
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err)
      showToast({ title: language.t("common.requestFailed"), description: message })
    } finally {
      setSaving(false)
    }
  }

  const remove = async (id: string) => {
    if (deleting()) return
    setDeleting(id)
    try {
      await serverSdk.client.v2.permission.saved.remove({ id })
      showToast({ variant: "success", icon: "circle-check", title: language.t("settings.policies.removed") })
      await refetch()
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err)
      showToast({ title: language.t("common.requestFailed"), description: message })
    } finally {
      setDeleting(undefined)
    }
  }

  return (
    <>
      <div class="settings-v2-tab-header">
        <h2 class="settings-v2-tab-title">{language.t("settings.policies.title")}</h2>
      </div>

      <div class="settings-v2-tab-body">
        <div class="settings-v2-section">
          <h3 class="settings-v2-section-title">{language.t("settings.policies.new")}</h3>
          <div class="flex flex-wrap items-end gap-2">
            <div class="flex-1 min-w-[140px]">
              <TextField
                label={language.t("settings.policies.field.action")}
                placeholder="bash"
                value={action()}
                onChange={setAction}
              />
            </div>
            <div class="flex-1 min-w-[140px]">
              <TextField
                label={language.t("settings.policies.field.resource")}
                placeholder="git *"
                value={resource()}
                onChange={setResource}
              />
            </div>
            <ButtonV2 size="normal" variant="neutral" icon="plus" disabled={!canAdd()} onClick={() => void add()}>
              {language.t("settings.policies.add")}
            </ButtonV2>
          </div>
        </div>

        <div class="settings-v2-section">
          <h3 class="settings-v2-section-title">{language.t("settings.policies.section.rules")}</h3>
          <SettingsListV2>
            <Show
              when={!rules.loading}
              fallback={<div class="settings-v2-provider-empty">{language.t("settings.config.loading")}</div>}
            >
              <Show
                when={(rules()?.length ?? 0) > 0}
                fallback={<div class="settings-v2-provider-empty">{language.t("settings.policies.empty")}</div>}
              >
                <For each={rules()}>
                  {(rule) => (
                    <div class="settings-v2-provider-row">
                      <div class="settings-v2-provider-lead">
                        <div class="settings-v2-provider-copy">
                          <div class="settings-v2-provider-main">
                            <span class="settings-v2-provider-name">{rule.action}</span>
                            <Show when={rule.projectID}>{(pid) => <Tag>{pid()}</Tag>}</Show>
                          </div>
                          <p class="settings-v2-provider-description">{rule.resource}</p>
                        </div>
                      </div>
                      <ButtonV2
                        size="normal"
                        variant="ghost-muted"
                        disabled={deleting() === rule.id}
                        onClick={() => void remove(rule.id)}
                      >
                        {language.t("common.delete")}
                      </ButtonV2>
                    </div>
                  )}
                </For>
              </Show>
            </Show>
          </SettingsListV2>
        </div>
      </div>
    </>
  )
}
