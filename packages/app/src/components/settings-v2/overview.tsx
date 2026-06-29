import { Component, For, Show, createResource } from "solid-js"
import { useLanguage } from "@/context/language"
import { useServerSDK } from "@/context/server-sdk"
import "./settings-v2.css"

/**
 * Admin overview: stat cards summarizing the project's agents / providers / commands / models, fetched
 * in parallel from the v2 list endpoints. Read-only; the per-item management lives in the other tabs.
 */
export const SettingsOverviewV2: Component = () => {
  const language = useLanguage()
  const serverSdk = useServerSDK()

  const [stats] = createResource(async () => {
    const [agents, commands, models, providers] = await Promise.all([
      serverSdk.client.v2.agent.list().then((r) => r.data?.data ?? []),
      serverSdk.client.v2.command.list().then((r) => r.data?.data ?? []),
      serverSdk.client.v2.model.list().then((r) => r.data?.data ?? []),
      serverSdk.client.v2.provider.list().then((r) => r.data?.data ?? []),
    ])
    return {
      agents: agents.length,
      agentsPrimary: agents.filter((a) => a.mode === "primary").length,
      commands: commands.length,
      models: models.length,
      providers: providers.length,
    }
  })

  const cards = () => {
    const s = stats()
    if (!s) return []
    return [
      {
        label: language.t("settings.overview.agents"),
        value: s.agents,
        sub: language.t("settings.overview.agents.sub", { count: String(s.agentsPrimary) }),
      },
      {
        label: language.t("settings.overview.providers"),
        value: s.providers,
        sub: language.t("settings.overview.providers.sub"),
      },
      { label: language.t("settings.overview.commands"), value: s.commands, sub: "" },
      { label: language.t("settings.overview.models"), value: s.models, sub: "" },
    ]
  }

  return (
    <>
      <div class="settings-v2-tab-header">
        <h2 class="settings-v2-tab-title">{language.t("settings.overview.title")}</h2>
      </div>

      <div class="settings-v2-tab-body">
        <Show
          when={!stats.loading}
          fallback={<div class="settings-v2-provider-empty">{language.t("settings.config.loading")}</div>}
        >
          <div class="grid grid-cols-2 md:grid-cols-4 gap-3">
            <For each={cards()}>
              {(card) => (
                <div class="flex flex-col gap-1 rounded-lg border p-3">
                  <span class="text-xs font-medium uppercase tracking-wide text-text-weak">{card.label}</span>
                  <span class="text-2xl font-semibold text-text-strong">{card.value}</span>
                  <Show when={card.sub}>
                    <span class="text-xs text-text-weak">{card.sub}</span>
                  </Show>
                </div>
              )}
            </For>
          </div>
        </Show>
      </div>
    </>
  )
}
