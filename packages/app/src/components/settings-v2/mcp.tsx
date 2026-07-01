import { Component, For, Show, createResource } from "solid-js"
import { ButtonV2 } from "@opencode-ai/ui/v2/button-v2"
import { Tag } from "@opencode-ai/ui/v2/badge-v2"
import { useDialog } from "@opencode-ai/ui/context/dialog"
import type { McpStatus } from "@opencode-ai/sdk/v2"
import { showToast } from "@/utils/toast"
import { useLanguage } from "@/context/language"
import { useServerSDK } from "@/context/server-sdk"
import { SettingsListV2 } from "./parts/list"
import { DialogMcpEdit } from "./dialog-mcp-edit"
import "./settings-v2.css"

/**
 * MCP server management (Extensions ▸ MCP). Lists each server from `config.mcp` alongside its live
 * `mcp.status` (connected / failed+error / disabled / needs-auth), and lets you add, edit, or
 * enable/disable one — all via `config.update` (deep-merged into `opencode.json`). Both fetchers
 * swallow errors so a partial/stubbed backend just renders empty.
 */
export const SettingsMcpV2: Component = () => {
  const language = useLanguage()
  const serverSdk = useServerSDK()
  const dialog = useDialog()

  const [servers, { refetch }] = createResource(async () => {
    const [cfgRes, statusRes] = await Promise.all([
      serverSdk.client.config.get().catch(() => undefined),
      serverSdk.client.mcp.status().catch(() => undefined),
    ])
    const mcp = cfgRes?.data?.mcp ?? {}
    const status = (statusRes?.data ?? {}) as Record<string, McpStatus>
    return Object.entries(mcp).map(([name, config]) => ({ name, config, status: status[name] }))
  })

  const toggle = async (name: string, enabled: boolean) => {
    try {
      await serverSdk.client.config.update({ config: { mcp: { [name]: { enabled: !enabled } } } })
      await refetch()
    } catch (err: unknown) {
      const m = err instanceof Error ? err.message : String(err)
      showToast({ title: language.t("common.requestFailed"), description: m })
    }
  }

  const statusLabel = (st?: McpStatus) => {
    switch (st?.status) {
      case "connected":
        return language.t("settings.extensions.mcp.status.connected")
      case "disabled":
        return language.t("settings.extensions.mcp.status.disabled")
      case "failed":
        return language.t("settings.extensions.mcp.status.failed")
      case "needs_auth":
      case "needs_client_registration":
        return language.t("settings.extensions.mcp.status.needsAuth")
      default:
        return undefined
    }
  }

  const statusError = (st?: McpStatus) =>
    st && (st.status === "failed" || st.status === "needs_client_registration") ? st.error : undefined

  return (
    <div class="settings-v2-section">
      <h3 class="settings-v2-section-title">{language.t("settings.extensions.mcp")}</h3>
      <SettingsListV2>
        <Show
          when={!servers.loading}
          fallback={<div class="settings-v2-provider-empty">{language.t("settings.config.loading")}</div>}
        >
          <Show
            when={(servers()?.length ?? 0) > 0}
            fallback={<div class="settings-v2-provider-empty">{language.t("settings.extensions.mcp.empty")}</div>}
          >
            <For each={servers()}>
              {(s) => {
                const enabled = () => !("enabled" in s.config && s.config.enabled === false)
                const type = () => ("type" in s.config ? s.config.type : "local")
                return (
                  <div class="settings-v2-provider-row">
                    <div class="settings-v2-provider-lead">
                      <div class="settings-v2-provider-copy">
                        <div class="settings-v2-provider-main">
                          <span class="settings-v2-provider-name">{s.name}</span>
                          <Tag>{type()}</Tag>
                          <Show when={statusLabel(s.status)}>{(label) => <Tag>{label()}</Tag>}</Show>
                        </div>
                        <Show when={statusError(s.status)}>
                          {(error) => <p class="settings-v2-provider-description">{error()}</p>}
                        </Show>
                      </div>
                    </div>
                    <div class="flex items-center gap-2">
                      <ButtonV2
                        size="normal"
                        variant="ghost-muted"
                        onClick={() => void toggle(s.name, enabled())}
                      >
                        {enabled()
                          ? language.t("settings.extensions.mcp.disable")
                          : language.t("settings.extensions.mcp.enable")}
                      </ButtonV2>
                      <ButtonV2
                        size="normal"
                        variant="ghost-muted"
                        onClick={() =>
                          dialog.show(() => (
                            <DialogMcpEdit
                              server={{ name: s.name, config: s.config }}
                              onSaved={() => void refetch()}
                            />
                          ))
                        }
                      >
                        {language.t("common.edit")}
                      </ButtonV2>
                    </div>
                  </div>
                )
              }}
            </For>
          </Show>
        </Show>
      </SettingsListV2>
      <ButtonV2
        size="normal"
        variant="neutral"
        icon="plus"
        class="mt-3"
        onClick={() => dialog.show(() => <DialogMcpEdit onSaved={() => void refetch()} />)}
      >
        {language.t("settings.extensions.mcp.new")}
      </ButtonV2>
    </div>
  )
}
