import type { ClientCapabilities } from "@agentclientprotocol/sdk"

export const ChildSessionUpdates = "opencode/child-session-updates"
/**
 * The client can decode the `_x.ai/ask_user_question` extension request, so
 * question forms can be handed to it directly instead of going through ACP form
 * elicitation. Only a client that opts in gets this channel: the grok-derived
 * TUI's Rust ACP client (agent-client-protocol 0.10.4) cannot decode
 * `elicitation/create` at all, and sending it produced a method-not-found that
 * the kernel reported as "the user dismissed this question".
 */
export const QuestionExt = "openbuild/question-ext"

export type Capabilities = ReturnType<typeof parse>

export function parse(client: ClientCapabilities | undefined) {
  const elicitation = client?.elicitation
  const compaction = client?.session?.compaction
  return {
    childSessionUpdates: client?._meta?.[ChildSessionUpdates] === true,
    formElicitation: elicitation?.form !== undefined && elicitation.form !== null,
    // An older TUI never sends this key, so it keeps the cancelUnshown path and
    // never sees an extension request it would answer with -32601.
    questionExt: client?._meta?.[QuestionExt] === true,
    compaction: compaction !== undefined && compaction !== null,
  }
}

export * as ACPCapabilities from "./capabilities"
