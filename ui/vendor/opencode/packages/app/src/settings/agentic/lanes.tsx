import { For, Show, type Component } from "solid-js"
import { Button } from "@opencode/ui/button"
import { useQuery } from "@tanstack/solid-query"
import { useLanguage } from "@/runtime/i18n/language"
import { useServerSDK } from "@/runtime/server/client"
import { SettingsList } from "@/settings/list"
import { SettingsRow } from "@/settings/row"
import { createAgenticConfig, isConfigObject, serverMessage, useAgenticRequest } from "./config"
import { InlineError, NumberField, RestartNotice } from "./fields"
import "./agentic.css"

type LaneStatus = {
  lane: string
  concurrency: number
  running: number
  queued: number
}

type LaneResult = {
  unavailable: boolean
  message?: string
  lanes: LaneStatus[]
}

const LANES = [
  { key: "cargo", fallback: 1 },
  { key: "node", fallback: 1 },
  { key: "network", fallback: 4 },
  { key: "gpu", fallback: 1 },
  { key: "default", fallback: 8 },
] as const

export const SettingsLanes: Component = () => {
  const language = useLanguage()
  const config = createAgenticConfig()
  const sdk = useServerSDK()
  const request = useAgenticRequest()

  const section = () => config.section("lanes")
  const concurrency = (key: (typeof LANES)[number]["key"]) =>
    typeof section()[key] === "number" ? (section()[key] as number) : undefined
  const commit = (changes: Record<string, unknown>) => void config.save("lanes", changes)

  const status = useQuery(() => ({
    queryKey: [sdk.scope, "settings-lane-status"],
    queryFn: async (): Promise<LaneResult> => {
      const response = await request("/api/experimental/lane")

      if (response.status === 503) {
        return { unavailable: true, message: await serverMessage(response), lanes: [] }
      }

      if (!response.ok) throw new Error(await serverMessage(response))
      const body: unknown = await response.json()
      const data = isConfigObject(body) && Array.isArray(body.data) ? body.data : []

      return { unavailable: false, lanes: data.filter(isLaneStatus) }
    },
    staleTime: 30_000,
    retry: false,
  }))

  return (
    <>
      <div class="settings-tab-header">
        <div class="settings-tab-header-row">
          <div class="flex flex-col gap-1">
            <h2 class="settings-tab-title">{language.t("settings.tab.lanes")}</h2>
            <span class="text-11-regular text-v2-text-text-muted">{language.t("settings.lanes.description")}</span>
          </div>
        </div>
      </div>

      <div class="settings-tab-body">
        <RestartNotice show={config.restart()} />
        <InlineError message={config.error()} />
        <div class="settings-section">
          <SettingsList>
            <For each={LANES}>
              {(item) => (
                <SettingsRow
                  title={language.t(`settings.lanes.row.${item.key}.title`)}
                  description={language.t(`settings.lanes.row.${item.key}.description`)}
                >
                  <NumberField
                    label={language.t(`settings.lanes.row.${item.key}.title`)}
                    testid={`settings-lanes-${item.key}`}
                    value={concurrency(item.key)}
                    fallback={item.fallback}
                    onCommit={(value) => commit({ [item.key]: value })}
                  />
                </SettingsRow>
              )}
            </For>
          </SettingsList>
        </div>

        <div class="settings-section">
          <div class="flex items-center justify-between gap-3">
            <div class="settings-section-title">{language.t("settings.lanes.status.title")}</div>
            <Button
              variant="ghost-muted"
              size="small"
              data-testid="settings-lanes-refresh"
              onClick={() => void status.refetch()}
            >
              {language.t("settings.lanes.status.refresh")}
            </Button>
          </div>

          <Show when={status.data?.unavailable}>
            <div
              role="status"
              data-testid="settings-lanes-unavailable"
              class="rounded-md border border-v2-border-border-base px-3 py-2"
            >
              <div class="text-12-medium">{language.t("settings.lanes.unavailable.title")}</div>
              <div class="text-11-regular text-v2-text-text-muted">{language.t("settings.lanes.unavailable.description")}</div>
              <Show when={status.data?.message}>
                {(message) => <div class="text-11-regular text-v2-text-text-faint">{message()}</div>}
              </Show>
            </div>
          </Show>

          <Show when={status.isPending}>
            <div class="text-12-regular text-v2-text-text-muted">{language.t("settings.lanes.status.loading")}</div>
          </Show>
          <Show when={status.isError}>
            <div role="alert" class="text-12-regular text-v2-state-fg-danger">
              {status.error instanceof Error ? status.error.message : language.t("settings.agentic.loadFailed")}
            </div>
          </Show>

          <Show when={!status.data?.unavailable && !status.isPending && !status.isError}>
            <Show
              when={status.data?.lanes.length}
              fallback={<div class="text-12-regular text-v2-text-text-muted">{language.t("settings.lanes.status.empty")}</div>}
            >
              <table class="w-full text-left text-12-regular" data-testid="settings-lanes-status">
                <thead class="text-11-regular text-v2-text-text-muted">
                  <tr>
                    <th class="py-1 font-normal">{language.t("settings.lanes.status.lane")}</th>
                    <th class="py-1 font-normal">{language.t("settings.lanes.status.concurrency")}</th>
                    <th class="py-1 font-normal">{language.t("settings.lanes.status.running")}</th>
                    <th class="py-1 font-normal">{language.t("settings.lanes.status.queued")}</th>
                  </tr>
                </thead>
                <tbody>
                  <For each={status.data?.lanes ?? []}>
                    {(lane) => (
                      <tr class="border-t border-v2-border-border-base">
                        <td class="py-1 font-mono">{lane.lane}</td>
                        <td class="py-1 tabular-nums">{lane.concurrency}</td>
                        <td class="py-1 tabular-nums">{lane.running}</td>
                        <td class="py-1 tabular-nums">{lane.queued}</td>
                      </tr>
                    )}
                  </For>
                </tbody>
              </table>
            </Show>
          </Show>
        </div>
      </div>
    </>
  )
}

function isLaneStatus(value: unknown): value is LaneStatus {
  return isConfigObject(value) && typeof value.lane === "string"
}
