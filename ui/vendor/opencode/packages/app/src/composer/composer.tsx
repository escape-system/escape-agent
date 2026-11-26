import { For, Show, createEffect, createMemo, createSignal } from "solid-js"
import { createMediaQuery } from "@solid-primitives/media"
import { Badge } from "@opencode/ui/badge"
import { Button } from "@opencode/ui/button"
import { useDialog } from "@opencode/ui/context/dialog"
import { Icon } from "@opencode/ui/icon"
import { Keybind } from "@opencode/ui/keybind"
import { Menu } from "@opencode/ui/menu"
import { Slash } from "@opencode/ui/slash"
import { Tooltip } from "@opencode/ui/tooltip"
import { ComposerEditor } from "./editor/editor"
import { DialogSelectModel, ModelSelectorDrawer } from "@/providers/models/select-dialog"
import { DialogSelectModelUnpaid } from "@/providers/models/unpaid"
import { resolveSelectedKeyLabel, type ProviderKeyOption } from "@/providers/models/keys"
import { formatKeybind, useCommand } from "@/shell/commands/command"
import { useLanguage } from "@/runtime/i18n/language"
import type { ComposerModel } from "./model"
import type { ComposerModelLabel } from "./model-label"

export function Composer(props: {
  class?: string
  model: ComposerModel
  borderUnderlay?: boolean
  readOnly?: boolean
  suggestionBoundary?: () => HTMLElement | undefined
}) {
  const dialog = useDialog()
  const command = useCommand()
  const language = useLanguage()

  const modelName = () => {
    const preview = props.model.model.preview

    if (preview) return preview.model
    const selection = props.model.model.selection
    const model = selection.current()

    if (model) return model.name
    const key = selection.key()

    if (key) return language.t("dialog.model.unavailable", { model: key.modelID })

    return language.t("dialog.model.select.title")
  }

  const modelLabel = () => {
    const preview = props.model.model.preview
    const label = preview ? preview.label : props.model.model.label

    if (label) return label

    return { provider: preview ? preview.provider : props.model.model.providerName, model: modelName() }
  }

  const openAddKey = async () => {
    const { DialogConnectProvider, useProviderConnectController } = await import("@/providers/connect/dialog")
    const controller = useProviderConnectController()
    const target = props.model.model.integrationID

    if (target) controller.select(target)

    void dialog.show(
      () => (
        <DialogConnectProvider
          controller={controller}
          standalone={!!target}
          directory={props.model.model.directory}
          selection={props.model.model.selection}
        />
      ),
      props.model.restoreFocus,
    )
  }

  return (
    <div class="flex flex-col gap-3">
      <ComposerEditor
        controller={props.model}
        borderUnderlay={props.borderUnderlay}
        readOnly={props.readOnly}
        class={props.class}
        modelControlsVisible={!props.model.model.loading || !!props.model.model.preview}
        attachKeybind={command.keybindParts("file.attach")}
        attachShortcut={command.keybind("file.attach")}
        alternateKeybind={[formatKeybind("mod", language.t), "↵"]}
        exitShellKeybind={[formatKeybind("esc", language.t)]}
        suggestionBoundary={props.suggestionBoundary}
        modelControl={
          <div class="flex min-w-0 items-center">
            <ComposerModelControl
              loading={props.model.model.loading}
              preview={!!props.model.model.preview}
              paid={props.model.model.paid}
              title={language.t("command.model.choose")}
              keybind={command.keybindParts("model.choose")}
              model={props.model.model.selection}
              label={modelLabel()}
              onClose={props.model.restoreFocus}
              onUnpaidClick={() => dialog.show(() => <DialogSelectModelUnpaid model={props.model.model.selection} />)}
            />
            <Show
              when={
                !props.model.model.loading &&
                props.model.model.keys &&
                props.model.model.keys.length > 0 &&
                props.model.model.selection.credential
              }
            >
              <Slash />
              <span class="sr-only">{" / "}</span>
              <ComposerKeyControl
                keys={props.model.model.keys!}
                selected={props.model.model.selection.credential!.current()}
                onSelect={(credentialID) => props.model.model.selection.credential!.set(credentialID)}
                onAddKey={() => void openAddKey()}
                onClose={props.model.restoreFocus}
              />
            </Show>
          </div>
        }
      />
    </div>
  )
}

function ComposerModelControl(props: {
  loading: boolean
  preview: boolean
  paid: boolean
  title: string
  keybind: string[]
  model: ComposerModel["model"]["selection"]
  label: ComposerModelLabel
  onClose: () => void
  onUnpaidClick: () => void
}) {
  // A preview paints the chip in its resting state, and the loaded names replace it in place.
  const shouldAnimate = createMemo<boolean>((previous) => previous ?? (props.loading && !props.preview))
  const mobile = createMediaQuery("(max-width: 767px)")
  const dialog = useDialog()

  const open = (detail: number) => {
    if (!props.paid) return props.onUnpaidClick()
    // A pointer open keeps focus in the composer, so the dialog returns it to the caret. A keyboard open
    // (a click with no pointer presses) returns focus to this trigger instead of moving it to the editor.
    void dialog.show(() => <DialogSelectModel model={props.model} />, detail === 0 ? undefined : props.onClose)
  }

  // A previewed chip is visible before the catalog loads, so a click on it opens the dialog once loading finishes.
  const [deferred, setDeferred] = createSignal<number>()

  createEffect(() => {
    const detail = deferred()

    if (props.loading || detail === undefined) return
    setDeferred(undefined)
    open(detail)
  })

  const content = () => (
    <>
      <span class="flex min-w-0 whitespace-nowrap leading-4">
        <Show when={props.label.provider}>
          {(name) => (
            <>
              <bdi class="min-w-0 shrink-[2] truncate text-v2-text-text-faint">{name()}</bdi>
              <Slash />
              <span class="sr-only">{" / "}</span>
            </>
          )}
        </Show>
        <bdi class="min-w-0 truncate">{props.label.model}</bdi>
      </span>
      <span class="-ml-0.5 -mr-1 flex shrink-0">
        <Icon name="chevron-down" />
      </span>
    </>
  )

  return (
    <Show when={!props.loading || props.preview}>
      <Tooltip
        inactive={mobile()}
        placement="top"
        value={
          <>
            {props.title}
            <Keybind keys={props.keybind} variant="neutral" />
          </>
        }
      >
        {/* While loading, the drawer would open on a catalog that isn't there yet, so the inert chip stands in. */}
        <Show
          when={mobile() && !props.loading}
          fallback={
            <Button
              data-action="composer-model"
              data-control-type="dialog"
              variant="ghost-muted"
              size="normal"
              class="min-w-0 max-w-[60cqw] justify-start ![font-weight:440]"
              classList={{ "animate-in fade-in": shouldAnimate() }}
              style={{ height: "28px" }}
              onMouseDown={(event: MouseEvent) => event.preventDefault()}
              onClick={(event: MouseEvent) => {
                if (!props.loading) return open(event.detail)

                // On mobile this inert chip stands in for the drawer trigger, so a click there is dropped.
                if (!mobile()) setDeferred(event.detail)
              }}
            >
              {content()}
            </Button>
          }
        >
          <ModelSelectorDrawer
            model={props.model}
            trigger={(triggerProps) => (
              <Button
                {...triggerProps}
                variant="ghost-muted"
                size="normal"
                style={{ height: "28px" }}
                class="min-w-0 max-w-[60cqw] justify-start ![font-weight:440]"
                classList={{ "animate-in fade-in": shouldAnimate() }}
                data-action="composer-model"
                data-control-type="drawer"
              >
                {content()}
              </Button>
            )}
            onClose={props.onClose}
          />
        </Show>
      </Tooltip>
    </Show>
  )
}

function ComposerKeyControl(props: {
  keys: readonly ProviderKeyOption[]
  selected?: string
  onSelect: (credentialID: string | undefined) => void
  onAddKey: () => void
  onClose: () => void
}) {
  const language = useLanguage()
  const mobile = createMediaQuery("(max-width: 767px)")

  const activeKey = createMemo(() => props.keys.find((key) => key.active))
  const label = createMemo(
    () =>
      resolveSelectedKeyLabel({
        credentialID: props.selected,
        keys: props.keys,
        autoLabel: language.t("composer.key.auto"),
      }) ?? language.t("composer.key.auto"),
  )

  const cooldownSeconds = (untilMs?: number) => {
    if (!untilMs) return 0
    const remaining = Math.ceil((untilMs - Date.now()) / 1000)

    return remaining > 0 ? remaining : 0
  }

  return (
    <Tooltip inactive={mobile()} placement="top" value={language.t("composer.key.choose")}>
      <Menu gutter={6} modal={false} placement="top-start">
        <Menu.Trigger
          as={Button}
          data-action="composer-key"
          variant="ghost-muted"
          size="normal"
          style={{ height: "28px" }}
          class="min-w-0 max-w-[34cqw] justify-start ![font-weight:440]"
          aria-label={language.t("composer.key.choose")}
          onMouseDown={(event: MouseEvent) => event.preventDefault()}
        >
          <span class="flex min-w-0 whitespace-nowrap leading-4">
            <bdi class="min-w-0 truncate">{label()}</bdi>
          </span>
          <span class="-ml-0.5 -mr-1 flex shrink-0">
            <Icon name="chevron-down" />
          </span>
        </Menu.Trigger>
        <Menu.Portal>
          <Menu.Content onCloseAutoFocus={() => props.onClose()}>
            <Menu.RadioGroup value={props.selected ?? ""} onChange={(value) => props.onSelect(value || undefined)}>
              <Menu.RadioItem
                value=""
                closeOnSelect
                badge={
                  activeKey() ? (
                    <Badge variant="subtle">{activeKey()!.label}</Badge>
                  ) : undefined
                }
              >
                <span class="truncate">{language.t("composer.key.autoDescription")}</span>
              </Menu.RadioItem>
              <For each={props.keys}>
                {(item) => {
                  const cooldown = cooldownSeconds(item.cooldownUntilMs)

                  return (
                    <Menu.RadioItem
                      value={item.id}
                      closeOnSelect
                      badge={
                        cooldown > 0 ? (
                          <Badge variant="neutral">
                            {language.t("composer.key.cooldown", { seconds: String(cooldown) })}
                          </Badge>
                        ) : item.active ? (
                          <Badge variant="accent">{language.t("composer.key.active")}</Badge>
                        ) : undefined
                      }
                    >
                      <span class="truncate">{item.label}</span>
                    </Menu.RadioItem>
                  )
                }}
              </For>
            </Menu.RadioGroup>
            <Menu.Separator />
            <Menu.Item onSelect={props.onAddKey}>{language.t("composer.key.add")}</Menu.Item>
          </Menu.Content>
        </Menu.Portal>
      </Menu>
    </Tooltip>
  )
}
