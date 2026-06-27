// Compatibility shim for the deleted `@opencode-ai/core` session namespaces (`MessageV2`/`Message`/
// `Session`), which the public share-viewer components used. Re-maps that namespace-style usage onto
// the generated SDK types (`@opencode-ai/sdk/v2`) now that the TS backend is gone. Type-only.
import type {
  Message as SdkMessage,
  Session as SdkSession,
  Part as SdkPart,
  ToolPart as SdkToolPart,
  ToolStateCompleted as SdkToolStateCompleted,
  AssistantMessage as SdkAssistantMessage,
} from "@opencode-ai/sdk/v2"

export namespace MessageV2 {
  export type Info = SdkMessage
  export type Assistant = SdkAssistantMessage
  export type Part = SdkPart
  export type ToolPart = SdkToolPart
  export type ToolStateCompleted = SdkToolStateCompleted
}

export namespace Message {
  export type Info = SdkMessage
}

export namespace Session {
  export type Info = SdkSession
}
