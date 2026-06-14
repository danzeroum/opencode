import { describe, expect, test } from "bun:test"
import { ModelTier } from "../src/model-tier"

describe("ModelTier", () => {
  test("classifies small models", () => {
    for (const id of ["claude-haiku-4", "gpt-4o-mini", "gemini-2.0-flash", "ministral-8b", "gpt-3.5-turbo"])
      expect(ModelTier.fromId(id)).toBe("small")
  })

  test("classifies large models", () => {
    for (const id of ["claude-opus-4", "claude-3-5-sonnet", "gpt-4o", "gpt-5", "gemini-1.5-pro"])
      expect(ModelTier.fromId(id)).toBe("large")
  })

  test("the gpt-4o / gpt-4o-mini boundary is correct", () => {
    expect(ModelTier.fromId("gpt-4o-mini")).toBe("small")
    expect(ModelTier.fromId("gpt-4o")).toBe("large")
  })

  test("unknown ids are medium", () => {
    expect(ModelTier.fromId("model")).toBe("medium")
    expect(ModelTier.isSmall("model")).toBe(false)
  })
})
