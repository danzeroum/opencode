import { Component, For, Show, createResource, createSignal } from "solid-js"
import { ButtonV2 } from "@opencode-ai/ui/v2/button-v2"
import { Tag } from "@opencode-ai/ui/v2/badge-v2"
import { showToast } from "@/utils/toast"
import { useLanguage } from "@/context/language"
import { useServerSDK } from "@/context/server-sdk"
import { SettingsListV2 } from "./parts/list"
import "./settings-v2.css"

/**
 * Commands management: lists the project's commands (`v2.command.list`) and lets you remove one
 * (`v2.command.delete`, which deletes its `.opencode/command/<name>.md`). A one-shot resource keeps it
 * independent of the per-directory sync store; create/edit (`v2.command.set`) lands in a follow-up.
 */
export const SettingsCommandsV2: Component = () => {
  const language = useLanguage()
  const serverSdk = useServerSDK()
  const [deleting, setDeleting] = createSignal<string | undefined>(undefined)
  const [commands, { refetch }] = createResource(async () => {
    const res = await serverSdk.client.v2.command.list()
    return res.data?.data ?? []
  })

  const remove = async (commandID: string) => {
    if (deleting()) return
    setDeleting(commandID)
    try {
      await serverSdk.client.v2.command.delete({ commandID })
      showToast({
        variant: "success",
        icon: "circle-check",
        title: language.t("settings.commands.deleted", { name: commandID }),
      })
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
        <h2 class="settings-v2-tab-title">{language.t("settings.commands.title")}</h2>
      </div>

      <div class="settings-v2-tab-body">
        <div class="settings-v2-section">
          <SettingsListV2>
            <Show
              when={!commands.loading}
              fallback={<div class="settings-v2-provider-empty">{language.t("settings.config.loading")}</div>}
            >
              <Show
                when={(commands()?.length ?? 0) > 0}
                fallback={<div class="settings-v2-provider-empty">{language.t("settings.commands.empty")}</div>}
              >
                <For each={commands()}>
                  {(cmd) => (
                    <div class="settings-v2-provider-row">
                      <div class="settings-v2-provider-lead">
                        <div class="settings-v2-provider-copy">
                          <div class="settings-v2-provider-main">
                            <span class="settings-v2-provider-name">{cmd.name}</span>
                            <Show when={cmd.agent}>{(agent) => <Tag>{agent()}</Tag>}</Show>
                          </div>
                          <Show when={cmd.description}>
                            {(description) => <p class="settings-v2-provider-description">{description()}</p>}
                          </Show>
                        </div>
                      </div>
                      <ButtonV2
                        size="normal"
                        variant="ghost-muted"
                        disabled={deleting() === cmd.name}
                        onClick={() => void remove(cmd.name)}
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
