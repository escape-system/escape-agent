import type { IconProps } from "@opencode/ui/icon"
import type { useLanguage } from "@/runtime/i18n/language"
import type { SettingsRootTab } from "./surface"

export const pageIcons = {
  general: "sliders",
  appearance: "appearance",
  notifications: "notifications",
  shortcuts: "keyboard",
  projects: "folder",
  workspaces: "outline-worktree",
  providers: "providers",
  models: "models",
  extensions: "extensions",
  memory: "outline-brain",
  goals: "check",
  observer: "outline-eye",
  subagents: "branch-out",
  lanes: "window-analytics",
  servers: "server",
  experimental: "flask",
  about: "info",
} as const satisfies Record<SettingsRootTab, IconProps["name"]>

export const pageLabels = {
  general: "settings.tab.preferences",
  appearance: "settings.general.section.appearance",
  notifications: "settings.tab.notifications",
  shortcuts: "settings.shortcuts.title",
  projects: "settings.tab.projects",
  workspaces: "settings.tab.workspaces",
  providers: "settings.providers.title",
  models: "settings.models.title",
  extensions: "settings.tab.extensions",
  memory: "settings.tab.memory",
  goals: "settings.tab.goals",
  observer: "settings.tab.observer",
  subagents: "settings.tab.subagents",
  lanes: "settings.tab.lanes",
  servers: "settings.section.server",
  experimental: "settings.tab.experimental",
  about: "settings.tab.about",
} as const satisfies Record<SettingsRootTab, Parameters<ReturnType<typeof useLanguage>["t"]>[0]>
