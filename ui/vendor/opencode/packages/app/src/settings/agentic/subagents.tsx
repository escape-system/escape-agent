import { Show, createEffect, createSignal, type Component } from "solid-js"
import { Button } from "@opencode/ui/button"
import { Textarea } from "@opencode/ui/textarea"
import { useLanguage } from "@/runtime/i18n/language"
import { SettingsList } from "@/settings/list"
import { SettingsRow } from "@/settings/row"
import { createAgenticConfig, isConfigObject } from "./config"
import { InlineError, NumberField, RestartNotice, TextField, ToggleField } from "./fields"
import "./agentic.css"

export const SettingsSubagents: Component = () => {
  const language = useLanguage()
  const config = createAgenticConfig()

  const section = () => config.section("subagents")
  const siblings = () => {
    const value = section().siblings

    return isConfigObject(value) ? value : {}
  }
  const numberOf = (key: "max_depth" | "max_children_per_session") =>
    typeof section()[key] === "number" ? (section()[key] as number) : undefined
  const textOf = (key: "default_agent" | "default_model") =>
    typeof section()[key] === "string" ? (section()[key] as string) : undefined
  const commit = (changes: Record<string, unknown>) => void config.save("subagents", changes)

  const [agents, setAgents] = createSignal("")
  const [agentsError, setAgentsError] = createSignal<string>()
  let agentsFocused = false

  createEffect(() => {
    if (agentsFocused) return
    const value = section().agents
    setAgents(JSON.stringify(isConfigObject(value) ? value : {}, null, 2))
  })

  const saveAgents = () => {
    let parsed: unknown

    try {
      parsed = JSON.parse(agents())
    } catch (cause) {
      setAgentsError(
        cause instanceof Error
          ? language.t("settings.subagents.agents.invalid", { message: cause.message })
          : language.t("settings.subagents.agents.invalid", { message: String(cause) }),
      )

      return
    }

    if (!isConfigObject(parsed)) {
      setAgentsError(language.t("settings.subagents.agents.object"))

      return
    }

    setAgentsError(undefined)
    void commit({ agents: parsed })
  }

  return (
    <>
      <div class="settings-tab-header">
        <div class="settings-tab-header-row">
          <div class="flex flex-col gap-1">
            <h2 class="settings-tab-title">{language.t("settings.tab.subagents")}</h2>
            <span class="text-11-regular text-v2-text-text-muted">{language.t("settings.subagents.description")}</span>
          </div>
        </div>
      </div>

      <div class="settings-tab-body">
        <RestartNotice show={config.restart()} />
        <InlineError message={config.error()} />
        <div class="settings-section">
          <SettingsList>
            <SettingsRow
              title={language.t("settings.subagents.row.enabled.title")}
              description={language.t("settings.subagents.row.enabled.description")}
            >
              <ToggleField
                label={language.t("settings.subagents.row.enabled.title")}
                testid="settings-subagents-enabled"
                checked={section().enabled !== false}
                onChange={(checked) => commit({ enabled: checked })}
              />
            </SettingsRow>
            <SettingsRow
              title={language.t("settings.subagents.row.defaultAgent.title")}
              description={language.t("settings.subagents.row.defaultAgent.description")}
            >
              <TextField
                label={language.t("settings.subagents.row.defaultAgent.title")}
                testid="settings-subagents-default-agent"
                placeholder="general"
                value={textOf("default_agent")}
                commit={(value) => commit({ default_agent: value ?? "general" })}
              />
            </SettingsRow>
            <SettingsRow
              title={language.t("settings.subagents.row.defaultModel.title")}
              description={language.t("settings.subagents.row.defaultModel.description")}
            >
              <TextField
                label={language.t("settings.subagents.row.defaultModel.title")}
                testid="settings-subagents-default-model"
                placeholder={language.t("settings.agentic.model.placeholder")}
                value={textOf("default_model")}
                commit={(value) => commit({ default_model: value })}
              />
            </SettingsRow>
            <SettingsRow
              title={language.t("settings.subagents.row.maxDepth.title")}
              description={language.t("settings.subagents.row.maxDepth.description")}
            >
              <NumberField
                label={language.t("settings.subagents.row.maxDepth.title")}
                testid="settings-subagents-max-depth"
                value={numberOf("max_depth")}
                fallback={3}
                onCommit={(value) => commit({ max_depth: value })}
              />
            </SettingsRow>
            <SettingsRow
              title={language.t("settings.subagents.row.maxChildren.title")}
              description={language.t("settings.subagents.row.maxChildren.description")}
            >
              <NumberField
                label={language.t("settings.subagents.row.maxChildren.title")}
                testid="settings-subagents-max-children"
                value={numberOf("max_children_per_session")}
                fallback={8}
                onCommit={(value) => commit({ max_children_per_session: value })}
              />
            </SettingsRow>
            <SettingsRow
              title={language.t("settings.subagents.row.allowSpawn.title")}
              description={language.t("settings.subagents.row.allowSpawn.description")}
            >
              <ToggleField
                label={language.t("settings.subagents.row.allowSpawn.title")}
                testid="settings-subagents-allow-spawn"
                checked={section().allow_spawn !== false}
                onChange={(checked) => commit({ allow_spawn: checked })}
              />
            </SettingsRow>
            <SettingsRow
              title={language.t("settings.subagents.row.siblingsEnabled.title")}
              description={language.t("settings.subagents.row.siblingsEnabled.description")}
            >
              <ToggleField
                label={language.t("settings.subagents.row.siblingsEnabled.title")}
                testid="settings-subagents-siblings-enabled"
                checked={siblings().enabled !== false}
                onChange={(checked) => commit({ siblings: { enabled: checked } })}
              />
            </SettingsRow>
            <SettingsRow
              title={language.t("settings.subagents.row.siblingsWake.title")}
              description={language.t("settings.subagents.row.siblingsWake.description")}
            >
              <ToggleField
                label={language.t("settings.subagents.row.siblingsWake.title")}
                testid="settings-subagents-siblings-wake"
                checked={siblings().wake !== false}
                onChange={(checked) => commit({ siblings: { wake: checked } })}
              />
            </SettingsRow>
            <SettingsRow
              title={language.t("settings.subagents.row.allowParent.title")}
              description={language.t("settings.subagents.row.allowParent.description")}
            >
              <ToggleField
                label={language.t("settings.subagents.row.allowParent.title")}
                testid="settings-subagents-allow-parent"
                checked={siblings().allow_parent !== false}
                onChange={(checked) => commit({ siblings: { allow_parent: checked } })}
              />
            </SettingsRow>
            <SettingsRow
              title={language.t("settings.subagents.row.maxMessages.title")}
              description={language.t("settings.subagents.row.maxMessages.description")}
            >
              <NumberField
                label={language.t("settings.subagents.row.maxMessages.title")}
                testid="settings-subagents-max-messages"
                value={typeof siblings().max_messages_per_min === "number" ? (siblings().max_messages_per_min as number) : undefined}
                fallback={20}
                onCommit={(value) => commit({ siblings: { max_messages_per_min: value } })}
              />
            </SettingsRow>
          </SettingsList>
        </div>

        <div class="settings-section">
          <div class="settings-section-title">{language.t("settings.subagents.agents.title")}</div>
          <p class="mb-3 text-11-regular text-v2-text-text-muted">{language.t("settings.subagents.agents.description")}</p>
          <Textarea
            data-testid="settings-subagents-agents"
            class="!w-full [&_[data-slot=textarea-v2-textarea]]:font-mono"
            rows={8}
            value={agents()}
            aria-label={language.t("settings.subagents.agents.title")}
            spellcheck={false}
            onInput={(event) => setAgents(event.currentTarget.value)}
            onFocus={() => (agentsFocused = true)}
            onBlur={() => (agentsFocused = false)}
          />
          <Show when={agentsError()}>
            {(message) => (
              <div role="alert" data-testid="settings-subagents-agents-error" class="mt-2 text-11-regular text-v2-state-fg-danger">
                {message()}
              </div>
            )}
          </Show>
          <div class="mt-2 flex justify-end">
            <Button variant="neutral" size="small" data-testid="settings-subagents-agents-save" onClick={saveAgents}>
              {language.t("settings.agentic.save")}
            </Button>
          </div>
        </div>
      </div>
    </>
  )
}
