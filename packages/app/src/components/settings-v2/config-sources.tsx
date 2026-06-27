import { Component, For, Show, createResource } from "solid-js"
import { Tag } from "@opencode-ai/ui/v2/badge-v2"
import { useLanguage } from "@/context/language"
import { useServerSDK } from "@/context/server-sdk"
import { SettingsListV2 } from "./parts/list"
import "./settings-v2.css"

// The designer's 7-level precedence palette (base->top). Used as the level "badge" dot so the cascade
// reads the same here as on the design's Dashboard.
const LEVEL_COLORS: Record<string, string> = {
  REMOTE: "#5a9cf8",
  GLOBAL: "#e6c07b",
  CUSTOM: "#e09a4b",
  PROJECT: "#7fb069",
  OPENCODE: "#9d7cd8",
  INLINE: "#ac8e68",
  MANAGED: "#e06c75",
}

const configKeys = (config: unknown): string[] =>
  config && typeof config === "object" && !Array.isArray(config) ? Object.keys(config as Record<string, unknown>) : []

/**
 * Read-only view of the config precedence cascade (`config.sources`): the seven levels base->top, each
 * with its source, a read-only badge, and the keys it contributes. The merged effective value is what
 * the rest of settings reads (`config.get`); this shows *where* each value comes from.
 */
export const SettingsConfigSourcesV2: Component = () => {
  const language = useLanguage()
  const serverSdk = useServerSDK()
  const [sources] = createResource(async () => {
    const res = await serverSdk.client.config.sources()
    return res.data?.levels ?? []
  })

  return (
    <>
      <div class="settings-v2-tab-header">
        <h2 class="settings-v2-tab-title">{language.t("settings.config.title")}</h2>
      </div>

      <div class="settings-v2-tab-body">
        <div class="settings-v2-section">
          <h3 class="settings-v2-section-title">{language.t("settings.config.cascade")}</h3>
          <SettingsListV2>
            <Show
              when={!sources.loading}
              fallback={<div class="settings-v2-provider-empty">{language.t("settings.config.loading")}</div>}
            >
              <For each={sources()}>
                {(level) => {
                  const keys = configKeys(level.config)
                  return (
                    <div class="settings-v2-provider-row">
                      <div class="settings-v2-provider-lead">
                        <span
                          class="inline-block w-2.5 h-2.5 rounded-sm shrink-0"
                          style={{ "background-color": LEVEL_COLORS[level.code] ?? "var(--mut)" }}
                        />
                        <div class="settings-v2-provider-copy">
                          <div class="settings-v2-provider-main">
                            <span class="settings-v2-provider-name">{level.label}</span>
                            <Show when={level.readOnly}>
                              <Tag>{language.t("settings.config.readOnly")}</Tag>
                            </Show>
                          </div>
                          <p class="settings-v2-provider-description">{level.source}</p>
                        </div>
                      </div>
                      <span class="settings-v2-provider-env-hint">
                        {keys.length > 0 ? keys.join(", ") : language.t("settings.config.notSet")}
                      </span>
                    </div>
                  )
                }}
              </For>
            </Show>
          </SettingsListV2>
        </div>
      </div>
    </>
  )
}
