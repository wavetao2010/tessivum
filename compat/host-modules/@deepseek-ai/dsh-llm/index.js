function deepFreeze(value) {
  if (value !== null && typeof value === 'object' && !Object.isFrozen(value)) {
    Object.freeze(value)
    for (const child of Object.values(value)) deepFreeze(child)
  }
  return value
}

export function createUserMessage(input) {
  return deepFreeze(structuredClone({ ...input, role: 'user', id: crypto.randomUUID() }))
}
