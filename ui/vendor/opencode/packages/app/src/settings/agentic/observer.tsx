import { type Component } from "solid-js"
import { useLanguage } from "@/runtime/i18n/language"
import { SettingsList } from "@/settings/list"
import { SettingsRow } from "@/settings/row"
import { createAgenticConfig } from "./config"
import { InlineError, NumberField, RestartNotice, TextField, ToggleField } from "./fields"
import "./agentic.css"

const NUMBERS = [
  { key: "tick_ms", fallback: 1000 },
  { key: "min_interval_ms", fallback: 2000 },
  { key: "max_calls_per_min", fallback: 12 },
  { key: "stall_seconds", fallback: 120 },
  { key: "max_repairs_per_10min", fallback: 5 },
  { key: "nudge_max_chars", fallback: 400 },
] as const

export const SettingsObserver: Component = () => {
  const language = useLanguage()
  const config = createAgenticConfig()

  const section = () => config.section("observer")
  const numberOf = (key: (typeof NUMBERS)[number]["key"]) =>
    typeof section()[key] === "number" ? (section()[key] as number) : undefined
  const textOf = (key: "model" | "digest_model" | "escalation_model") =>
    typeof section()[key] === "string" ? (section()[key] as string) : undefined
  const commit = (changes: Record<string, unknown>) => void config.save("observer", changes)

  return (
    <>
      <div class="settings-tab-header">
        <div class="settings-tab-header-row">
          <div class="flex flex-col gap-1">
            <h2 class="settings-tab-title">{language.t("settings.tab.observer")}</h2>
            <span class="text-11-regular text-v2-text-text-muted">{language.t("settings.observer.description")}</span>
          </div>
        </div>
      </div>

      <div class="settings-tab-body">
        <RestartNotice show={config.restart()} />
        <InlineError message={config.error()} />
        <div class="settings-section">
          <div
            data-testid="settings-observer-audit-note"
            class="mb-4 rounded-md border border-v2-border-border-base bg-v2-background-bg-layer-01 px-3 py-2"
          >
            <div class="text-12-medium">{language.t("settings.observer.note.title")}</div>
            <div class="text-11-regular text-v2-text-text-muted">{language.t("settings.observer.note.description")}</div>
          </div>
          <SettingsList>
            <SettingsRow
              title={language.t("settings.observer.row.enabled.title")}
              description={language.t("settings.observer.row.enabled.description")}
            >
              <ToggleField
                label={language.t("settings.observer.row.enabled.title")}
                testid="settings-observer-enabled"
                checked={section().enabled !== false}
                onChange={(checked) => commit({ enabled: checked })}
              />
            </SettingsRow>
            <SettingsRow
              title={language.t("settings.observer.row.model.title")}
              description={language.t("settings.observer.row.model.description")}
            >
              <TextField
                label={language.t("settings.observer.row.model.title")}
                testid="settings-observer-model"
                placeholder={language.t("settings.agentic.model.placeholder")}
                value={textOf("model")}
                commit={(value) => commit({ model: value })}
              />
            </SettingsRow>
            <SettingsRow
              title={language.t("settings.observer.row.digestModel.title")}
              description={language.t("settings.observer.row.digestModel.description")}
            >
              <TextField
                label={language.t("settings.observer.row.digestModel.title")}
                testid="settings-observer-digest-model"
                placeholder={language.t("settings.agentic.model.placeholder")}
                value={textOf("digest_model")}
                commit={(value) => commit({ digest_model: value })}
              />
            </SettingsRow>
            <SettingsRow
              title={language.t("settings.observer.row.escalationModel.title")}
              description={language.t("settings.observer.row.escalationModel.description")}
            >
              <TextField
                label={language.t("settings.observer.row.escalationModel.title")}
                testid="settings-observer-escalation-model"
                placeholder={language.t("settings.agentic.model.placeholder")}
                value={textOf("escalation_model")}
                commit={(value) => commit({ escalation_model: value })}
              />
            </SettingsRow>
            {NUMBERS.map((item) => (
              <SettingsRow
                title={language.t(`settings.observer.row.${item.key}.title`)}
                description={language.t(`settings.observer.row.${item.key}.description`)}
              >
                <NumberField
                  label={language.t(`settings.observer.row.${item.key}.title`)}
                  testid={`settings-observer-${item.key}`}
                  value={numberOf(item.key)}
                  fallback={item.fallback}
                  onCommit={(value) => commit({ [item.key]: value })}
                />
              </SettingsRow>
            ))}
          </SettingsList>
        </div>
      </div>
    </>
  )
}
