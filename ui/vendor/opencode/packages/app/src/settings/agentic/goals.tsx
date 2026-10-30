import { type Component } from "solid-js"
import { useLanguage } from "@/runtime/i18n/language"
import { SettingsList } from "@/settings/list"
import { SettingsRow } from "@/settings/row"
import { createAgenticConfig } from "./config"
import { InlineError, NumberField, RestartNotice } from "./fields"
import "./agentic.css"

export const SettingsGoals: Component = () => {
  const language = useLanguage()
  const config = createAgenticConfig()

  const section = () => config.section("goal")
  const number = (key: "max_rounds" | "max_stalls" | "wait_limit_minutes") =>
    typeof section()[key] === "number" ? (section()[key] as number) : undefined
  const commit = (changes: Record<string, unknown>) => void config.save("goal", changes)

  return (
    <>
      <div class="settings-tab-header">
        <div class="settings-tab-header-row">
          <div class="flex flex-col gap-1">
            <h2 class="settings-tab-title">{language.t("settings.tab.goals")}</h2>
            <span class="text-11-regular text-v2-text-text-muted">{language.t("settings.goals.description")}</span>
          </div>
        </div>
      </div>

      <div class="settings-tab-body">
        <RestartNotice show={config.restart()} />
        <InlineError message={config.error()} />
        <div class="settings-section">
          <div
            data-testid="settings-goals-scope-note"
            class="mb-4 rounded-md border border-v2-border-border-base bg-v2-background-bg-layer-01 px-3 py-2"
          >
            <div class="text-12-medium">{language.t("settings.goals.scope.title")}</div>
            <div class="text-11-regular text-v2-text-text-muted">{language.t("settings.goals.scope.description")}</div>
          </div>
          <SettingsList>
            <SettingsRow
              title={language.t("settings.goals.row.maxRounds.title")}
              description={language.t("settings.goals.row.maxRounds.description")}
            >
              <NumberField
                label={language.t("settings.goals.row.maxRounds.title")}
                testid="settings-goals-max-rounds"
                value={number("max_rounds")}
                fallback={50}
                onCommit={(value) => commit({ max_rounds: value })}
              />
            </SettingsRow>
            <SettingsRow
              title={language.t("settings.goals.row.maxStalls.title")}
              description={language.t("settings.goals.row.maxStalls.description")}
            >
              <NumberField
                label={language.t("settings.goals.row.maxStalls.title")}
                testid="settings-goals-max-stalls"
                value={number("max_stalls")}
                fallback={3}
                onCommit={(value) => commit({ max_stalls: value })}
              />
            </SettingsRow>
            <SettingsRow
              title={language.t("settings.goals.row.waitLimit.title")}
              description={language.t("settings.goals.row.waitLimit.description")}
            >
              <NumberField
                label={language.t("settings.goals.row.waitLimit.title")}
                testid="settings-goals-wait-limit"
                value={number("wait_limit_minutes")}
                fallback={30}
                onCommit={(value) => commit({ wait_limit_minutes: value })}
              />
            </SettingsRow>
          </SettingsList>
        </div>
      </div>
    </>
  )
}
