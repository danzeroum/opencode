import { Component, For, Show, createResource, createSignal } from "solid-js"
import { ButtonV2 } from "@opencode-ai/ui/v2/button-v2"
import { Tag } from "@opencode-ai/ui/v2/badge-v2"
import { useDialog } from "@opencode-ai/ui/context/dialog"
import { showToast } from "@/utils/toast"
import { useLanguage } from "@/context/language"
import { useServerSDK } from "@/context/server-sdk"
import { SettingsListV2 } from "./parts/list"
import { DialogAgentEdit } from "./dialog-agent-edit"
import "./settings-v2.css"

/**
 * Agents management: lists the project's agents (`v2.agent.list`), opens the create/edit dialog
 * (`v2.agent.set`), and deletes one (`v2.agent.delete`, removing its `.opencode/agent/<name>.md`). A
 * one-shot resource keeps it independent of the per-directory sync store.
 */
export const SettingsAgentsV2: Component = () => {
  const language = useLanguage()
  const serverSdk = useServerSDK()
  const dialog = useDialog()
  const [deleting, setDeleting] = createSignal<string | undefined>(undefined)
  const [agents, { refetch }] = createResource(async () => {
    const res = await serverSdk.client.v2.agent.list()
    return res.data?.data ?? []
  })

  const remove = async (agentID: string) => {
    if (deleting()) return
    setDeleting(agentID)
    try {
      await serverSdk.client.v2.agent.delete({ agentID })
      showToast({
        variant: "success",
        icon: "circle-check",
        title: language.t("settings.agents.deleted", { name: agentID }),
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
        <h2 class="settings-v2-tab-title">{language.t("settings.agents.title")}</h2>
      </div>

      <div class="settings-v2-tab-body">
        <div class="settings-v2-section">
          <SettingsListV2>
            <Show
              when={!agents.loading}
              fallback={<div class="settings-v2-provider-empty">{language.t("settings.config.loading")}</div>}
            >
              <Show
                when={(agents()?.length ?? 0) > 0}
                fallback={<div class="settings-v2-provider-empty">{language.t("settings.agents.empty")}</div>}
              >
                <For each={agents()}>
                  {(agent) => (
                    <div class="settings-v2-provider-row">
                      <div class="settings-v2-provider-lead">
                        <div class="settings-v2-provider-copy">
                          <div class="settings-v2-provider-main">
                            <span class="settings-v2-provider-name">{agent.id}</span>
                            <Tag>{language.t(`settings.agents.mode.${agent.mode}`)}</Tag>
                          </div>
                          <Show when={agent.description}>
                            {(description) => <p class="settings-v2-provider-description">{description()}</p>}
                          </Show>
                        </div>
                      </div>
                      <div class="flex items-center gap-2">
                        <ButtonV2
                          size="normal"
                          variant="ghost-muted"
                          onClick={() =>
                            dialog.show(() => <DialogAgentEdit agent={agent} onSaved={() => void refetch()} />)
                          }
                        >
                          {language.t("common.edit")}
                        </ButtonV2>
                        <ButtonV2
                          size="normal"
                          variant="ghost-muted"
                          disabled={deleting() === agent.id}
                          onClick={() => void remove(agent.id)}
                        >
                          {language.t("common.delete")}
                        </ButtonV2>
                      </div>
                    </div>
                  )}
                </For>
              </Show>
            </Show>
          </SettingsListV2>
          <ButtonV2
            size="normal"
            variant="neutral"
            icon="plus"
            class="mt-3"
            onClick={() => dialog.show(() => <DialogAgentEdit onSaved={() => void refetch()} />)}
          >
            {language.t("settings.agents.new")}
          </ButtonV2>
        </div>
      </div>
    </>
  )
}
