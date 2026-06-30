import { Component, For, createMemo, onMount } from "solid-js"
import { useTheme, type ColorScheme } from "@opencode-ai/ui/theme/context"
import { useLanguage } from "@/context/language"
import "./settings-v2.css"

/**
 * Appearance: a dedicated screen for the color scheme (system/light/dark) and a grid theme picker over
 * the existing theme catalog (`useTheme`). Hovering a theme previews it live; clicking applies it.
 * Selection is marked with a check so it reads correctly on any theme without depending on token colors.
 */
export const SettingsAppearanceV2: Component = () => {
  const language = useLanguage()
  const theme = useTheme()

  onMount(() => {
    void theme.loadThemes()
  })

  const schemes: { value: ColorScheme; label: string }[] = [
    { value: "system", label: language.t("theme.scheme.system") },
    { value: "light", label: language.t("theme.scheme.light") },
    { value: "dark", label: language.t("theme.scheme.dark") },
  ]

  const themeOptions = createMemo(() => theme.ids().map((id) => ({ id, name: theme.name(id) })))

  return (
    <>
      <div class="settings-v2-tab-header">
        <h2 class="settings-v2-tab-title">{language.t("settings.appearance.title")}</h2>
      </div>

      <div class="settings-v2-tab-body">
        <div class="settings-v2-section">
          <h3 class="settings-v2-section-title">{language.t("settings.general.row.colorScheme.title")}</h3>
          <div class="flex flex-wrap gap-2">
            <For each={schemes}>
              {(s) => (
                <button
                  type="button"
                  class="rounded-md border px-3 py-1.5 text-14-regular"
                  classList={{ "font-semibold": theme.colorScheme() === s.value }}
                  onClick={() => theme.setColorScheme(s.value)}
                >
                  {theme.colorScheme() === s.value ? "✓ " : ""}
                  {s.label}
                </button>
              )}
            </For>
          </div>
        </div>

        <div class="settings-v2-section">
          <h3 class="settings-v2-section-title">{language.t("settings.general.row.theme.title")}</h3>
          <div class="grid grid-cols-2 md:grid-cols-3 gap-2">
            <For each={themeOptions()}>
              {(opt) => (
                <button
                  type="button"
                  class="truncate rounded-md border px-3 py-2 text-left text-14-regular"
                  classList={{ "font-semibold": theme.themeId() === opt.id }}
                  onClick={() => theme.setTheme(opt.id)}
                  onMouseEnter={() => theme.previewTheme(opt.id)}
                  onMouseLeave={() => theme.cancelPreview()}
                >
                  {theme.themeId() === opt.id ? "✓ " : ""}
                  {opt.name}
                </button>
              )}
            </For>
          </div>
        </div>
      </div>
    </>
  )
}
