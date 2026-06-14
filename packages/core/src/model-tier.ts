export * as ModelTier from "./model-tier"

export type Tier = "small" | "medium" | "large"

// String-based capability tiers for the V2 runtime. Mirrors packages/opencode/src/provider/model-tier.ts
// (V1); kept duplicated because core cannot depend on the opencode package. Keep the two in sync.
const SMALL = [/haiku/, /mini/, /flash/, /nano/, /lite/, /small/, /\b\d+b\b/, /3\.5-turbo/]

const LARGE = [
  /opus/,
  /sonnet/,
  /gpt-4\.1/,
  /gpt-4o(?!-mini)/,
  /gpt-5(?!-(?:mini|nano))/,
  /gemini-[\d.]+-pro/,
  /-(?:70|72|123|235|405)b\b/,
]

export function fromId(modelID: string): Tier {
  const id = modelID.toLowerCase()
  if (LARGE.some((re) => re.test(id))) return "large"
  if (SMALL.some((re) => re.test(id))) return "small"
  return "medium"
}

export function isSmall(modelID: string): boolean {
  return fromId(modelID) === "small"
}
