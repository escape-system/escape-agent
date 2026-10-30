import { For, Show, createMemo, createSignal, onCleanup, type Component } from "solid-js"
import { Badge } from "@opencode/ui/badge"
import { Icon } from "@opencode/ui/icon"
import { Button } from "@opencode/ui/button"
import { IconButton } from "@opencode/ui/icon-button"
import { Select } from "@opencode/ui/select"
import { TextInput } from "@opencode/ui/text-input"
import { useQuery } from "@tanstack/solid-query"
import { useLanguage } from "@/runtime/i18n/language"
import { useServer } from "@/runtime/server/current"
import { useServerSDK } from "@/runtime/server/client"
import { SettingsList } from "@/settings/list"
import { SettingsRow } from "@/settings/row"
import { createAgenticConfig, isConfigObject, serverMessage, useAgenticRequest } from "./config"
import { InlineError, NumberField, RestartNotice, TextField, ToggleField } from "./fields"
import "./agentic.css"

type MemoryItem = {
  id: string
  kind?: string
  text: string
  tags?: string[]
  importance?: number
}

type MemoryResult = {
  unavailable: boolean
  message?: string
  items: MemoryItem[]
}

const KINDS = ["fact", "decision", "preference", "pattern", "gotcha"] as const

export const SettingsMemory: Component = () => {
  const language = useLanguage()
  const config = createAgenticConfig()
  const request = useAgenticRequest()
  const sdk = useServerSDK()
  const server = useServer()

  const project = createMemo(() => server.ctx.projects.last() ?? server.ctx.projects.unique()[0])
  const [scope, setScope] = createSignal<"project" | "global">()
  const effectiveScope = () => (scope() === "global" || !project() ? "global" : "project")
  const scopeID = () => (effectiveScope() === "project" ? project()?.id : undefined)

  const [search, setSearch] = createSignal("")
  const [query, setQuery] = createSignal("")
  let timer: ReturnType<typeof setTimeout> | undefined
  onCleanup(() => clearTimeout(timer))

  const memory = useQuery(() => ({
    queryKey: [sdk.scope, "settings-memory", effectiveScope(), scopeID(), query()],
    queryFn: async (): Promise<MemoryResult> => {
      const params = new URLSearchParams({ scope: effectiveScope(), limit: "100" })

      if (scopeID()) params.set("scopeID", scopeID()!)
      if (query().trim()) params.set("query", query().trim())
      const response = await request(`/api/experimental/memory?${params.toString()}`)

      if (response.status === 503) {
        return { unavailable: true, message: await serverMessage(response), items: [] }
      }

      if (!response.ok) throw new Error(await serverMessage(response))
      const body: unknown = await response.json()
      const data = isConfigObject(body) && Array.isArray(body.data) ? body.data : []

      return { unavailable: false, items: data.filter(isMemoryItem) }
    },
    staleTime: 30_000,
    retry: false,
  }))

  const [text, setText] = createSignal("")
  const [kind, setKind] = createSignal<(typeof KINDS)[number]>("fact")
  const [importance, setImportance] = createSignal(3)
  const [tags, setTags] = createSignal("")
  const [submitting, setSubmitting] = createSignal(false)
  const [submitError, setSubmitError] = createSignal<string>()

  const section = () => config.section("memory")
  const embeddingModel = createMemo(() => {
    const embedding = section().embedding

    return isConfigObject(embedding) && typeof embedding.model === "string" ? embedding.model : undefined
  })

  const commit = (changes: Record<string, unknown>) => void config.save("memory", changes)

  const add = async () => {
    const value = text().trim()

    if (!value || submitting()) return
    setSubmitting(true)
    setSubmitError(undefined)

    try {
      const body: Record<string, unknown> = {
        text: value,
        kind: kind(),
        importance: importance(),
        tags: tags()
          .split(",")
          .map((tag) => tag.trim())
          .filter(Boolean),
        scope: effectiveScope(),
      }

      if (scopeID()) body.scopeID = scopeID()
      const response = await request("/api/experimental/memory", { method: "POST", body: JSON.stringify(body) })

      if (!response.ok) {
        setSubmitError(await serverMessage(response))

        return
      }

      await response.arrayBuffer()
      setText("")
      setTags("")
      void memory.refetch()
    } catch (cause) {
      setSubmitError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setSubmitting(false)
    }
  }

  const forget = async (id: string) => {
    setSubmitError(undefined)

    try {
      const response = await request(`/api/experimental/memory/${encodeURIComponent(id)}`, { method: "DELETE" })

      if (!response.ok) {
        setSubmitError(await serverMessage(response))

        return
      }

      await response.arrayBuffer()
      void memory.refetch()
    } catch (cause) {
      setSubmitError(cause instanceof Error ? cause.message : String(cause))
    }
  }

  const scopeOptions = createMemo(() => [
    { value: "project" as const, label: language.t("settings.memory.scope.project"), disabled: !project() },
    { value: "global" as const, label: language.t("settings.memory.scope.global") },
  ])

  const kindOptions = KINDS.map((value) => ({ value, label: value }))
  const importanceOptions = [1, 2, 3, 4, 5].map((value) => ({ value, label: String(value) }))

  return (
    <>
      <div class="settings-tab-header">
        <div class="settings-tab-header-row">
          <div class="flex flex-col gap-1">
            <h2 class="settings-tab-title">{language.t("settings.tab.memory")}</h2>
            <span class="text-11-regular text-v2-text-text-muted">{language.t("settings.memory.description")}</span>
          </div>
        </div>
      </div>

      <div class="settings-tab-body">
        <RestartNotice show={config.restart()} />
        <InlineError message={config.error()} />
        <Show when={config.failed()}>
          <div role="alert" class="mb-4 text-12-regular text-v2-state-fg-danger">
            {language.t("settings.agentic.loadFailed")}
          </div>
        </Show>

        <div class="settings-section">
          <SettingsList>
            <SettingsRow
              title={language.t("settings.memory.row.autoScribe.title")}
              description={language.t("settings.memory.row.autoScribe.description")}
            >
              <ToggleField
                label={language.t("settings.memory.row.autoScribe.title")}
                testid="settings-memory-auto-scribe"
                checked={section().auto_scribe !== false}
                onChange={(checked) => commit({ auto_scribe: checked })}
              />
            </SettingsRow>
            <SettingsRow
              title={language.t("settings.memory.row.maxTokens.title")}
              description={language.t("settings.memory.row.maxTokens.description")}
            >
              <NumberField
                label={language.t("settings.memory.row.maxTokens.title")}
                testid="settings-memory-max-tokens"
                value={typeof section().max_tokens === "number" ? (section().max_tokens as number) : undefined}
                fallback={800}
                onCommit={(value) => commit({ max_tokens: value })}
              />
            </SettingsRow>
            <SettingsRow
              title={language.t("settings.memory.row.embeddingModel.title")}
              description={language.t("settings.memory.row.embeddingModel.description")}
            >
              <TextField
                label={language.t("settings.memory.row.embeddingModel.title")}
                testid="settings-memory-embedding-model"
                placeholder={language.t("settings.agentic.model.placeholder")}
                value={embeddingModel()}
                commit={(value) => commit({ embedding: { model: value } })}
              />
            </SettingsRow>
          </SettingsList>
        </div>

        <div class="settings-section">
          <div class="settings-section-title">{language.t("settings.memory.list.title")}</div>
          <div class="mb-3 flex flex-wrap items-center gap-2">
            <Select
              data-testid="settings-memory-scope"
              aria-label={language.t("settings.memory.scope.label")}
              options={scopeOptions()}
              current={scopeOptions().find((option) => option.value === effectiveScope())}
              value={(option) => option.value}
              label={(option) => option.label}
              placement="bottom-start"
              gutter={6}
              onSelect={(option) => option && setScope(option.value)}
            />
            <TextInput
              type="search"
              class="min-w-56 flex-1"
              data-testid="settings-memory-search"
              aria-label={language.t("settings.memory.search.placeholder")}
              placeholder={language.t("settings.memory.search.placeholder")}
              value={search()}
              onInput={(event) => {
                const value = event.currentTarget.value
                setSearch(value)
                clearTimeout(timer)
                timer = setTimeout(() => setQuery(value), 250)
              }}
            />
          </div>

          <Show when={memory.data?.unavailable}>
            <div
              role="status"
              data-testid="settings-memory-unavailable"
              class="mb-3 rounded-md border border-v2-border-border-base px-3 py-2"
            >
              <div class="text-12-medium">{language.t("settings.memory.unavailable.title")}</div>
              <div class="text-11-regular text-v2-text-text-muted">
                {language.t("settings.memory.unavailable.description")}
              </div>
              <Show when={memory.data?.message}>
                {(message) => <div class="text-11-regular text-v2-text-text-faint">{message()}</div>}
              </Show>
            </div>
          </Show>

          <Show when={!memory.data?.unavailable}>
            <div class="mb-4 flex flex-col gap-2" data-testid="settings-memory-add">
              <div class="flex flex-wrap items-center gap-2">
                <TextInput
                  type="text"
                  class="min-w-56 flex-1"
                  aria-label={language.t("settings.memory.add.placeholder")}
                  placeholder={language.t("settings.memory.add.placeholder")}
                  value={text()}
                  maxLength={500}
                  onInput={(event) => setText(event.currentTarget.value)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") void add()
                  }}
                />
                <Select
                  aria-label={language.t("settings.memory.add.kind")}
                  options={kindOptions}
                  current={kindOptions.find((option) => option.value === kind())}
                  value={(option) => option.value}
                  label={(option) => option.label}
                  placement="bottom-end"
                  gutter={6}
                  onSelect={(option) => option && setKind(option.value)}
                />
                <Select
                  aria-label={language.t("settings.memory.add.importance")}
                  options={importanceOptions}
                  current={importanceOptions.find((option) => option.value === importance())}
                  value={(option) => String(option.value)}
                  label={(option) => option.label}
                  placement="bottom-end"
                  gutter={6}
                  onSelect={(option) => option && setImportance(option.value)}
                />
                <Button
                  data-testid="settings-memory-add-button"
                  variant="neutral"
                  size="small"
                  disabled={submitting() || !text().trim()}
                  onClick={() => void add()}
                >
                  {submitting() ? language.t("settings.memory.add.saving") : language.t("settings.memory.add.submit")}
                </Button>
              </div>
              <TextInput
                type="text"
                class="w-full"
                aria-label={language.t("settings.memory.add.tags")}
                placeholder={language.t("settings.memory.add.tags")}
                value={tags()}
                onInput={(event) => setTags(event.currentTarget.value)}
              />
            </div>
          </Show>

          <InlineError message={submitError()} testid="settings-memory-submit-error" />

          <Show when={memory.isPending}>
            <div class="text-12-regular text-v2-text-text-muted">{language.t("settings.memory.loading")}</div>
          </Show>
          <Show when={memory.isError}>
            <div role="alert" class="text-12-regular text-v2-state-fg-danger">
              {memory.error instanceof Error ? memory.error.message : language.t("settings.agentic.loadFailed")}
            </div>
          </Show>
          <Show when={!memory.isPending && !memory.isError && !memory.data?.unavailable && memory.data?.items.length === 0}>
            <div class="text-12-regular text-v2-text-text-muted">{language.t("settings.memory.empty")}</div>
          </Show>

          <div class="flex flex-col gap-2">
            <For each={memory.data?.items ?? []}>
              {(item) => (
                <div
                  data-testid="settings-memory-item"
                  class="flex items-start gap-3 rounded-md border border-v2-border-border-base bg-v2-background-bg-layer-01 px-3 py-2"
                >
                  <div class="flex min-w-0 flex-1 flex-col gap-1">
                    <span class="text-12-regular break-words">{item.text}</span>
                    <div class="flex flex-wrap items-center gap-1">
                      <Badge variant="accent">{item.kind ?? "fact"}</Badge>
                      <For each={item.tags ?? []}>{(tag) => <Badge variant="subtle">#{tag}</Badge>}</For>
                      <span class="text-11-regular text-v2-text-text-faint">
                        {language.t("settings.memory.importance", { value: String(item.importance ?? 3) })}
                      </span>
                    </div>
                  </div>
                  <IconButton
                    type="button"
                    variant="ghost-muted"
                    size="small"
                    icon={<Icon name="xmark-small" />}
                    aria-label={language.t("settings.memory.delete")}
                    data-testid="settings-memory-delete"
                    onClick={() => void forget(item.id)}
                  />
                </div>
              )}
            </For>
          </div>
        </div>
      </div>
    </>
  )
}

function isMemoryItem(value: unknown): value is MemoryItem {
  return isConfigObject(value) && typeof value.id === "string" && typeof value.text === "string"
}
