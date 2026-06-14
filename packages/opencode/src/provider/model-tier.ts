export * as ModelTier from "./model-tier"

import type { Provider } from "./provider"

export type Tier = "small" | "medium" | "large"

// Model ids that indicate a lower-capability ("small") model, matched case-insensitively
// against the catalog model id. Small models benefit from a leaner system prompt, a reduced
// tool surface, and earlier compaction. Keep this list conservative: a false "small" classification
// degrades a capable model more than a missed one.
const SMALL = [/haiku/, /mini/, /flash/, /nano/, /lite/, /small/, /\b\d+b\b/, /3\.5-turbo/]

// Model ids that indicate a high-capability ("large") model. Checked first so that, e.g.,
// "gpt-4o-mini" is not misread as large by the "gpt-4o" rule.
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

export function from(model: Provider.Model): Tier {
  return fromId(model.api?.id ?? model.id ?? "")
}

export function isSmall(model: Provider.Model): boolean {
  return from(model) === "small"
}
