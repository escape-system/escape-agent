function cleanVariant(variant: unknown): string | undefined {
  if (typeof variant !== "string") return undefined
  const trimmed = variant.trim()

  return trimmed && trimmed.toLowerCase() !== "default" ? trimmed : undefined
}

export function resolveSessionComposerSelection(
  info: { agent?: string; model?: { id: string; providerID: string; variant?: string } } | undefined,
  metadata: Record<string, unknown> | undefined,
) {
  const model = metadata?.model

  const historical =
    model &&
    typeof model === "object" &&
    !Array.isArray(model) &&
    "providerID" in model &&
    "modelID" in model &&
    typeof model.providerID === "string" &&
    typeof model.modelID === "string"
      ? {
          providerID: model.providerID,
          modelID: model.modelID,
          variant: "variant" in model ? cleanVariant(model.variant) : undefined,
        }
      : undefined

  return {
    agent: info?.agent ?? (typeof metadata?.agent === "string" ? metadata.agent : undefined),
    model: info?.model
      ? { providerID: info.model.providerID, modelID: info.model.id, variant: cleanVariant(info.model.variant) }
      : historical,
  }
}
