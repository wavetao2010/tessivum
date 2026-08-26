export const SUBAGENT_DESCRIPTOR_VERSION = 2

export function snapshotSubagentDescriptor(input) {
  const candidate = input.mode === 'one-shot'
    ? { version: SUBAGENT_DESCRIPTOR_VERSION, mode: input.mode, provider: input.provider, ...(input.label === undefined ? {} : { label: input.label }) }
    : {
        version: SUBAGENT_DESCRIPTOR_VERSION,
        mode: input.mode,
        provider: input.provider,
        label: input.label,
        ...(input.agentProvider === undefined ? {} : { agentProvider: input.agentProvider }),
        ...(input.agentModel === undefined ? {} : { agentModel: input.agentModel }),
        ...(input.persona === undefined ? {} : { persona: input.persona }),
        ...(input.toolFilter === undefined ? {} : { toolFilter: input.toolFilter }),
      }
  const encoded = JSON.stringify(candidate)
  if (encoded === undefined) throw new TypeError('subagent descriptor is not losslessly JSON-serializable')
  return JSON.parse(encoded)
}
