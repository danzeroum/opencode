import { Component } from "solid-js"
import { Dialog } from "@opencode-ai/ui/v2/dialog-v2"
import { TabsV2 } from "@opencode-ai/ui/v2/tabs-v2"
import { Icon } from "@opencode-ai/ui/icon"
import { useLanguage } from "@/context/language"
import { usePlatform } from "@/context/platform"
import { SettingsGeneralV2 } from "./general"
import { SettingsKeybinds } from "../settings-keybinds"
import { SettingsProvidersV2 } from "./providers"
import { SettingsModelsV2 } from "./models"
import "./settings-v2.css"
import { SettingsServersV2 } from "./servers"
import { SettingsConfigSourcesV2 } from "./config-sources"
import { SettingsCommandsV2 } from "./commands"
import { SettingsAgentsV2 } from "./agents"
import { SettingsOverviewV2 } from "./overview"
import { SettingsPoliciesV2 } from "./policies"
import { SettingsSnapshotsV2 } from "./snapshots"
import { SettingsExtensionsV2 } from "./extensions"
import { SettingsAppearanceV2 } from "./appearance"

export const DialogSettings: Component = () => {
  const language = useLanguage()
  const platform = usePlatform()

  return (
    <Dialog size="x-large" variant="settings" class="settings-v2-dialog">
      <TabsV2 orientation="vertical" variant="settings" defaultValue="general" class="settings-v2">
        <TabsV2.List>
          <div class="flex flex-col justify-between h-full w-full">
            <div class="flex flex-col gap-3 w-full">
              <div class="flex flex-col gap-3">
                <div class="flex flex-col gap-1.5">
                  <TabsV2.SectionTitle>{language.t("settings.section.desktop")}</TabsV2.SectionTitle>
                  <div class="flex flex-col gap-1.5 w-full">
                    <TabsV2.Trigger value="general">
                      <Icon name="sliders" />
                      {language.t("settings.tab.general")}
                    </TabsV2.Trigger>
                    <TabsV2.Trigger value="shortcuts">
                      <Icon name="keyboard" />
                      {language.t("settings.tab.shortcuts")}
                    </TabsV2.Trigger>
                    <TabsV2.Trigger value="appearance">
                      <Icon name="layout-left" />
                      {language.t("settings.appearance.title")}
                    </TabsV2.Trigger>
                  </div>
                </div>

                <div class="flex flex-col gap-1.5">
                  <TabsV2.SectionTitle>{language.t("settings.section.server")}</TabsV2.SectionTitle>
                  <div class="flex flex-col gap-1.5 w-full">
                    <TabsV2.Trigger value="overview">
                      <Icon name="dot-grid" />
                      {language.t("settings.overview.title")}
                    </TabsV2.Trigger>
                    <TabsV2.Trigger value="servers">
                      <Icon name="server" />
                      {language.t("status.popover.tab.servers")}
                    </TabsV2.Trigger>
                    <TabsV2.Trigger value="providers">
                      <Icon name="providers" />
                      {language.t("settings.providers.title")}
                    </TabsV2.Trigger>
                    <TabsV2.Trigger value="models">
                      <Icon name="models" />
                      {language.t("settings.models.title")}
                    </TabsV2.Trigger>
                    <TabsV2.Trigger value="config">
                      <Icon name="branch" />
                      {language.t("settings.config.title")}
                    </TabsV2.Trigger>
                    <TabsV2.Trigger value="commands">
                      <Icon name="bullet-list" />
                      {language.t("settings.commands.title")}
                    </TabsV2.Trigger>
                    <TabsV2.Trigger value="agents">
                      <Icon name="brain" />
                      {language.t("settings.agents.title")}
                    </TabsV2.Trigger>
                    <TabsV2.Trigger value="policies">
                      <Icon name="circle-ban-sign" />
                      {language.t("settings.policies.title")}
                    </TabsV2.Trigger>
                    <TabsV2.Trigger value="snapshots">
                      <Icon name="archive" />
                      {language.t("settings.snapshots.title")}
                    </TabsV2.Trigger>
                  </div>
                </div>

                <div class="flex flex-col gap-1.5">
                  <TabsV2.SectionTitle>{language.t("settings.section.extensions")}</TabsV2.SectionTitle>
                  <div class="flex flex-col gap-1.5 w-full">
                    <TabsV2.Trigger value="extensions">
                      <Icon name="code-lines" />
                      {language.t("settings.extensions.title")}
                    </TabsV2.Trigger>
                  </div>
                </div>
              </div>
            </div>
            <div class="settings-v2-nav-footer">
              <span>{language.t("app.name.desktop")}</span>
              <span>v{platform.version}</span>
            </div>
          </div>
        </TabsV2.List>
        <TabsV2.Content value="overview" class="settings-v2-panel">
          <SettingsOverviewV2 />
        </TabsV2.Content>
        <TabsV2.Content value="general" class="settings-v2-panel">
          <SettingsGeneralV2 />
        </TabsV2.Content>
        <TabsV2.Content value="shortcuts" class="settings-v2-panel">
          <SettingsKeybinds v2 />
        </TabsV2.Content>
        <TabsV2.Content value="appearance" class="settings-v2-panel">
          <SettingsAppearanceV2 />
        </TabsV2.Content>
        <TabsV2.Content value="servers" class="settings-v2-panel">
          <SettingsServersV2 />
        </TabsV2.Content>
        <TabsV2.Content value="providers" class="settings-v2-panel">
          <SettingsProvidersV2 />
        </TabsV2.Content>
        <TabsV2.Content value="models" class="settings-v2-panel">
          <SettingsModelsV2 />
        </TabsV2.Content>
        <TabsV2.Content value="config" class="settings-v2-panel">
          <SettingsConfigSourcesV2 />
        </TabsV2.Content>
        <TabsV2.Content value="commands" class="settings-v2-panel">
          <SettingsCommandsV2 />
        </TabsV2.Content>
        <TabsV2.Content value="agents" class="settings-v2-panel">
          <SettingsAgentsV2 />
        </TabsV2.Content>
        <TabsV2.Content value="policies" class="settings-v2-panel">
          <SettingsPoliciesV2 />
        </TabsV2.Content>
        <TabsV2.Content value="snapshots" class="settings-v2-panel">
          <SettingsSnapshotsV2 />
        </TabsV2.Content>
        <TabsV2.Content value="extensions" class="settings-v2-panel">
          <SettingsExtensionsV2 />
        </TabsV2.Content>
      </TabsV2>
    </Dialog>
  )
}
