import { Component, For, Show, createResource } from "solid-js"
import { Tag } from "@opencode-ai/ui/v2/badge-v2"
import { useLanguage } from "@/context/language"
import { useServerSDK } from "@/context/server-sdk"
import { SettingsListV2 } from "./parts/list"
import "./settings-v2.css"

/**
 * Extensions overview: reflects the backend's real MCP + integration state (both are wired-empty /
 * stubbed on the local Rust backend today, so this typically shows empty). Plugin management has no API
 * yet, so it's an explicit note. Fetchers swallow errors so a stubbed endpoint just renders empty.
 */
export const SettingsExtensionsV2: Component = () => {
  const language = useLanguage()
  const serverSdk = useServerSDK()

  const [mcp] = createResource(async () => {
    try {
      const res = await serverSdk.client.mcp.status()
      return Object.keys(res.data ?? {})
    } catch {
      return [] as string[]
    }
  })

  const [integrations] = createResource(async () => {
    try {
      const res = await serverSdk.client.v2.integration.list()
      return res.data?.data ?? []
    } catch {
      return []
    }
  })

  return (
    <>
      <div class="settings-v2-tab-header">
        <h2 class="settings-v2-tab-title">{language.t("settings.extensions.title")}</h2>
      </div>

      <div class="settings-v2-tab-body">
        <div class="settings-v2-section">
          <h3 class="settings-v2-section-title">{language.t("settings.extensions.mcp")}</h3>
          <SettingsListV2>
            <Show
              when={!mcp.loading}
              fallback={<div class="settings-v2-provider-empty">{language.t("settings.config.loading")}</div>}
            >
              <Show
                when={(mcp()?.length ?? 0) > 0}
                fallback={<div class="settings-v2-provider-empty">{language.t("settings.extensions.mcp.empty")}</div>}
              >
                <For each={mcp()}>
                  {(name) => (
                    <div class="settings-v2-provider-row">
                      <div class="settings-v2-provider-lead">
                        <div class="settings-v2-provider-copy">
                          <div class="settings-v2-provider-main">
                            <span class="settings-v2-provider-name">{name}</span>
                          </div>
                        </div>
                      </div>
                    </div>
                  )}
                </For>
              </Show>
            </Show>
          </SettingsListV2>
        </div>

        <div class="settings-v2-section">
          <h3 class="settings-v2-section-title">{language.t("settings.extensions.integrations")}</h3>
          <SettingsListV2>
            <Show
              when={!integrations.loading}
              fallback={<div class="settings-v2-provider-empty">{language.t("settings.config.loading")}</div>}
            >
              <Show
                when={(integrations()?.length ?? 0) > 0}
                fallback={
                  <div class="settings-v2-provider-empty">{language.t("settings.extensions.integrations.empty")}</div>
                }
              >
                <For each={integrations()}>
                  {(it) => (
                    <div class="settings-v2-provider-row">
                      <div class="settings-v2-provider-lead">
                        <div class="settings-v2-provider-copy">
                          <div class="settings-v2-provider-main">
                            <span class="settings-v2-provider-name">{it.name}</span>
                            <Show when={it.connections.length > 0}>
                              <Tag>{language.t("settings.extensions.connected")}</Tag>
                            </Show>
                          </div>
                        </div>
                      </div>
                    </div>
                  )}
                </For>
              </Show>
            </Show>
          </SettingsListV2>
        </div>

        <div class="settings-v2-section">
          <h3 class="settings-v2-section-title">{language.t("settings.extensions.plugins")}</h3>
          <div class="settings-v2-provider-empty">{language.t("settings.extensions.plugins.note")}</div>
        </div>
      </div>
    </>
  )
}
