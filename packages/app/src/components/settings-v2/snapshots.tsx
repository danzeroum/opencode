import { Component, For, Show, createResource, createSignal } from "solid-js"
import { ButtonV2 } from "@opencode-ai/ui/v2/button-v2"
import { TextField } from "@opencode-ai/ui/text-field"
import { showToast } from "@/utils/toast"
import { useLanguage } from "@/context/language"
import { useServerSDK } from "@/context/server-sdk"
import { SettingsListV2 } from "./parts/list"
import "./settings-v2.css"

/**
 * Snapshots management: working-tree checkpoints (`v2.snapshot.*`). Lists the current repo's snapshots,
 * creates one (optionally labelled), and restores one. Restore is guarded server-side by a clean working
 * tree, so a dirty tree surfaces as a "restore failed" toast rather than clobbering work.
 */
export const SettingsSnapshotsV2: Component = () => {
  const language = useLanguage()
  const serverSdk = useServerSDK()
  const [creating, setCreating] = createSignal(false)
  const [restoring, setRestoring] = createSignal<string | undefined>(undefined)
  const [message, setMessage] = createSignal("")

  const [snapshots, { refetch }] = createResource(async () => {
    const res = await serverSdk.client.v2.snapshot.list()
    return res.data?.data ?? []
  })

  const create = async () => {
    if (creating()) return
    setCreating(true)
    try {
      await serverSdk.client.v2.snapshot.create({ message: message().trim() || undefined })
      setMessage("")
      showToast({ variant: "success", icon: "circle-check", title: language.t("settings.snapshots.created") })
      await refetch()
    } catch (err: unknown) {
      const m = err instanceof Error ? err.message : String(err)
      showToast({ title: language.t("common.requestFailed"), description: m })
    } finally {
      setCreating(false)
    }
  }

  const restore = async (id: string) => {
    if (restoring()) return
    setRestoring(id)
    try {
      await serverSdk.client.v2.snapshot.restore({ id })
      showToast({ variant: "success", icon: "circle-check", title: language.t("settings.snapshots.restored") })
    } catch (err: unknown) {
      const m = err instanceof Error ? err.message : String(err)
      showToast({ title: language.t("settings.snapshots.restoreFailed"), description: m })
    } finally {
      setRestoring(undefined)
    }
  }

  const formatTime = (t: number) => {
    try {
      return new Date(t).toLocaleString()
    } catch {
      return ""
    }
  }

  return (
    <>
      <div class="settings-v2-tab-header">
        <h2 class="settings-v2-tab-title">{language.t("settings.snapshots.title")}</h2>
      </div>

      <div class="settings-v2-tab-body">
        <div class="settings-v2-section">
          <h3 class="settings-v2-section-title">{language.t("settings.snapshots.new")}</h3>
          <div class="flex flex-wrap items-end gap-2">
            <div class="flex-1 min-w-[180px]">
              <TextField
                label={language.t("settings.snapshots.field.message")}
                placeholder={language.t("settings.snapshots.field.messagePlaceholder")}
                value={message()}
                onChange={setMessage}
              />
            </div>
            <ButtonV2 size="normal" variant="neutral" icon="plus" disabled={creating()} onClick={() => void create()}>
              {language.t("settings.snapshots.create")}
            </ButtonV2>
          </div>
        </div>

        <div class="settings-v2-section">
          <h3 class="settings-v2-section-title">{language.t("settings.snapshots.section.list")}</h3>
          <SettingsListV2>
            <Show
              when={!snapshots.loading}
              fallback={<div class="settings-v2-provider-empty">{language.t("settings.config.loading")}</div>}
            >
              <Show
                when={(snapshots()?.length ?? 0) > 0}
                fallback={<div class="settings-v2-provider-empty">{language.t("settings.snapshots.empty")}</div>}
              >
                <For each={snapshots()}>
                  {(snap) => (
                    <div class="settings-v2-provider-row">
                      <div class="settings-v2-provider-lead">
                        <div class="settings-v2-provider-copy">
                          <div class="settings-v2-provider-main">
                            <span class="settings-v2-provider-name">{snap.message || snap.sha.slice(0, 8)}</span>
                          </div>
                          <p class="settings-v2-provider-description">{formatTime(snap.time)}</p>
                        </div>
                      </div>
                      <ButtonV2
                        size="normal"
                        variant="ghost-muted"
                        disabled={restoring() === snap.id}
                        onClick={() => void restore(snap.id)}
                      >
                        {language.t("settings.snapshots.restore")}
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
