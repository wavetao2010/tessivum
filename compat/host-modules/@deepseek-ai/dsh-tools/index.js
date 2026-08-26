function compileProperties(spec) {
  const properties = {}
  const required = []
  for (const [name, value] of Object.entries(spec)) {
    properties[name] = compileSchema(value, true)
    if (value.required === true) required.push(name)
  }
  return { properties, ...(required.length === 0 ? {} : { required }) }
}

function compileSchema(spec, property = false) {
  if (spec === null || typeof spec !== 'object' || Array.isArray(spec)) throw new TypeError('tool schema must be an object')
  const node = {}
  for (const key of ['description', 'title', 'default', 'examples']) if (Object.hasOwn(spec, key)) node[key] = structuredClone(spec[key])
  if (Object.hasOwn(spec, 'oneOf')) {
    node.oneOf = spec.oneOf.map(value => compileSchema(value))
    return node
  }
  if (spec.type !== 'json') node.type = spec.type
  if (Object.hasOwn(spec, 'enum')) node.enum = structuredClone(spec.enum)
  if (Object.hasOwn(spec, 'const')) node.const = structuredClone(spec.const)
  if (spec.type === 'object') {
    node.additionalProperties = spec.additionalProperties
    if (Object.hasOwn(spec, 'properties')) Object.assign(node, compileProperties(spec.properties))
  } else if (spec.type === 'array' && Object.hasOwn(spec, 'items')) {
    node.items = compileSchema(spec.items)
  }
  if (!property && spec.required === true) throw new TypeError('required is only valid on object properties')
  return node
}

export function defineTool(options) {
  if (options.timeoutMs !== undefined && (!Number.isFinite(options.timeoutMs) || options.timeoutMs <= 0)) {
    throw new TypeError(`defineTool(${options.name}): timeoutMs must be a positive finite number`)
  }
  return {
    ...options,
    parameters: { type: 'object', ...compileProperties(options.parameters) },
    output: { ...options.output, schema: compileSchema(options.output.schema) },
  }
}
