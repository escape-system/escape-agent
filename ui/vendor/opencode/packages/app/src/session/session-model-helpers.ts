type ModelSelection = {
  model: {
    current(): { id: string; provider: { id: string } } | undefined
    variant: {
      current(): string | undefined
    }
    credential?: {
      current(): string | undefined
    }
  }
}

type PromptState = {
  model: {
    current(): { providerID: string; modelID: string; variant?: string | null; credentialID?: string } | undefined
    set(model: { providerID: string; modelID: string; variant?: string | null; credentialID?: string }): void
  }
}

export const syncPromptModel = (local: ModelSelection, prompt: PromptState) => {
  const model = local.model.current()

  if (!model) return
  const credentialID = local.model.credential?.current()

  const next = {
    providerID: model.provider.id,
    modelID: model.id,
    variant: local.model.variant.current(),
    ...(credentialID ? { credentialID } : {}),
  }

  const current = prompt.model.current()

  if (
    current?.providerID === next.providerID &&
    current.modelID === next.modelID &&
    current.variant === next.variant &&
    current.credentialID === next.credentialID
  )
    return
  prompt.model.set(next)
}
