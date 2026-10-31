import { describe, expect, test } from "bun:test"
import type { CreateElicitationResponse } from "@agentclientprotocol/sdk"
import { ACPElicitation } from "../../src/acp/elicitation"
import {
  delivered,
  ephemeralEvent,
  startSession,
  succeeded,
  textDelta,
  toolStarted,
  toolSucceeded,
} from "./wire-fixture"

const questions = (sessionID: string, id = "frm_question") =>
  ephemeralEvent("form.created", {
    form: {
      id,
      sessionID,
      title: "Questions",
      metadata: { kind: "question", tool: { messageID: "msg_tools", id: "call_question" } },
      fields: [
        {
          key: "q0",
          title: "Runtime",
          description: "Which runtime?",
          type: "string",
          options: [
            { value: "Bun", label: "Bun", description: "Fast" },
            { value: "Node", label: "Node", description: "Stable" },
          ],
          custom: true,
        },
        {
          key: "q1",
          title: "Goals",
          description: "What matters?",
          type: "multiselect",
          options: [{ value: "Fast", label: "Fast", description: "Speed" }],
          custom: true,
        },
      ],
    },
  })

const capable = { childSessionUpdates: false, formElicitation: true, compaction: false }

const form = (
  fields: ACPElicitation.AskedForm["fields"],
  metadata: ACPElicitation.AskedForm["metadata"] = { kind: "question" },
) => ({ id: "frm_test", sessionID: "ses_test", title: "Test", metadata, fields })

const accept = (content: Record<string, string | number | boolean | string[]>): CreateElicitationResponse => ({
  action: "accept",
  content,
})

describe("acp elicitation mapping", () => {
  test("cancels forms from unsupported clients, unknown flows, and credential-looking fields", () => {
    const fields: ACPElicitation.AskedForm["fields"] = [{ key: "name", type: "string" }]
    expect(ACPElicitation.requestedSchema(form(fields), { ...capable, formElicitation: false })).toBeUndefined()
    expect(ACPElicitation.requestedSchema(form(fields, { kind: "mcp-elicitation" }), capable)).toBeUndefined()
    expect(ACPElicitation.requestedSchema(form(fields, {}), capable)).toBeUndefined()
    expect(ACPElicitation.requestedSchema(form(fields, { kind: "websearch.provider" }), capable)).toBeDefined()
    const credentials: Array<ACPElicitation.AskedForm["fields"]> = [
      [{ key: "api_key", type: "string" }],
      [{ key: "q0", title: "GitHub token", type: "string" }],
      [{ key: "q0", title: "Password", type: "string", hidden: true, default: "" }],
      [{ key: "q0", title: "Setup", description: "Paste your API key", type: "string" }],
      [{ key: "q0", title: "Setup", type: "string", options: [{ value: "a", label: "Use my access token" }] }],
    ]
    expect(credentials.map((fields) => ACPElicitation.requestedSchema(form(fields), capable))).toEqual(
      credentials.map(() => undefined),
    )
  })
})

describe("acp elicitation over the wire", () => {
  test("answers a question form through elicitation and continues the turn", async () => {
    await using acp = await startSession({
      capabilities: { elicitation: true },
      onPrompt: ({ sessionID, id }) => [
        delivered(sessionID, id),
        toolStarted(sessionID, "call_question", "question"),
        questions(sessionID),
      ],
      elicitation: () => accept({ q0: "Bun", q1: ["Fast"], q1_custom: "Small" }),
      onFormReply: ({ sessionID, formID }) => [
        ephemeralEvent("form.replied", { sessionID, id: formID, answer: {} }),
        toolSucceeded(sessionID, "call_question", {}, "answered"),
        textDelta(sessionID, "msg_after", "thanks"),
        succeeded(sessionID),
      ],
    })

    expect((await acp.prompt(acp.sessionId, "hello")).stopReason).toBe("end_turn")
    expect(acp.elicitations).toMatchObject([
      { mode: "form", sessionId: acp.sessionId, toolCallId: "call_question", message: "Questions" },
    ])
    expect(acp.server.repliedForms).toEqual([
      { sessionID: acp.sessionId, formID: "frm_question", answer: { q0: "Bun", q1: ["Fast", "Small"] } },
    ])
    expect(acp.server.cancelledForms).toEqual([])
    expect(acp.updates.some((item) => item.update.sessionUpdate === "agent_message_chunk")).toBe(true)
  })

  test("cancels the form when the client declines, cancels, fails, or sends a wrong value type", async () => {
    const responses: Array<() => CreateElicitationResponse> = [
      () => ({ action: "decline" }),
      () => ({ action: "cancel" }),
      () => {
        throw new Error("elicitation UI failed")
      },
      () => ({ action: "accept", content: { q0: { nested: true } } }),
    ]
    const ids = ["frm_decline", "frm_cancel", "frm_fail", "frm_invalid"]
    await using acp = await startSession({
      capabilities: { elicitation: true },
      onPrompt: ({ sessionID, id }) => [delivered(sessionID, id), ...ids.map((form) => questions(sessionID, form))],
      elicitation: () => responses[acp.elicitations.length - 1](),
      onFormCancel: ({ sessionID }) => (acp.server.cancelledForms.length === ids.length ? [succeeded(sessionID)] : []),
    })

    expect((await acp.prompt(acp.sessionId, "hello")).stopReason).toBe("end_turn")
    expect(acp.elicitations).toHaveLength(ids.length)
    expect(acp.server.cancelledForms.map((item) => item.formID)).toEqual(ids)
    // Every non-accept outcome explains itself, so the question tool returns a
    // normal tool failure instead of a `CancelledError` that interrupts the step.
    expect(acp.server.cancelledForms.map((item) => item.message)).toEqual(
      ids.map(() => ACPElicitation.CancelledQuestionMessage),
    )
    expect(acp.server.repliedForms).toEqual([])
    expect(acp.server.interrupts).toEqual([])
  })
})

describe("acp question over the ask_user_question extension", () => {
  test("routes a question form to the client extension and replies with the answer", async () => {
    await using acp = await startSession({
      capabilities: { questionExt: true },
      onPrompt: ({ sessionID, id }) => [
        delivered(sessionID, id),
        toolStarted(sessionID, "call_question", "question"),
        questions(sessionID),
      ],
      askUserQuestion: () => ({
        outcome: "accepted",
        answers: { "Which runtime?": ["Bun"], "What matters?": ["Fast"] },
      }),
      onFormReply: ({ sessionID, formID }) => [
        ephemeralEvent("form.replied", { sessionID, id: formID, answer: {} }),
        toolSucceeded(sessionID, "call_question", {}, "answered"),
        succeeded(sessionID),
      ],
    })

    expect((await acp.prompt(acp.sessionId, "hello")).stopReason).toBe("end_turn")
    // No elicitation request: this client never declared the capability, so
    // sending one would have failed with method-not-found.
    expect(acp.elicitations).toEqual([])
    expect(acp.askQuestions).toHaveLength(1)
    expect(acp.askQuestions[0]).toMatchObject({
      sessionId: acp.sessionId,
      toolCallId: "call_question",
      mode: "default",
      questions: [
        { question: "Which runtime?", multi_select: false },
        { question: "What matters?", multi_select: true },
      ],
    })
    expect(acp.server.repliedForms).toEqual([
      { sessionID: acp.sessionId, formID: "frm_question", answer: { q0: "Bun", q1: ["Fast"] } },
    ])
    expect(acp.server.cancelledForms).toEqual([])
  })

  test("prefers free-form notes over the literal Other label", async () => {
    await using acp = await startSession({
      capabilities: { questionExt: true },
      onPrompt: ({ sessionID, id }) => [
        delivered(sessionID, id),
        toolStarted(sessionID, "call_question", "question"),
        questions(sessionID),
      ],
      askUserQuestion: () => ({
        outcome: "accepted",
        answers: { "Which runtime?": ["Other"] },
        annotations: { "Which runtime?": { notes: "Deno" } },
      }),
      onFormReply: ({ sessionID }) => [succeeded(sessionID)],
    })

    expect((await acp.prompt(acp.sessionId, "hello")).stopReason).toBe("end_turn")
    expect(acp.server.repliedForms[0]?.answer).toEqual({ q0: "Deno" })
  })

  test("a cancelled extension request continues the turn with an explanatory message", async () => {
    await using acp = await startSession({
      capabilities: { questionExt: true },
      onPrompt: ({ sessionID, id }) => [
        delivered(sessionID, id),
        toolStarted(sessionID, "call_question", "question"),
        questions(sessionID),
      ],
      askUserQuestion: () => ({ outcome: "cancelled" }),
      onFormCancel: ({ sessionID }) => [succeeded(sessionID)],
    })

    expect((await acp.prompt(acp.sessionId, "hello")).stopReason).toBe("end_turn")
    expect(acp.server.repliedForms).toEqual([])
    expect(acp.server.cancelledForms.map((item) => item.formID)).toEqual(["frm_question"])
    expect(acp.server.cancelledForms.map((item) => item.message)).toEqual([
      ACPElicitation.CancelledQuestionMessage,
    ])
  })

  test("an older client without the capability keeps the cancelUnshown path", async () => {
    await using acp = await startSession({
      capabilities: {},
      onPrompt: ({ sessionID, id }) => [
        delivered(sessionID, id),
        toolStarted(sessionID, "call_question", "question"),
        questions(sessionID),
      ],
      onFormCancel: ({ sessionID }) => [succeeded(sessionID)],
    })

    expect((await acp.prompt(acp.sessionId, "hello")).stopReason).toBe("end_turn")
    expect(acp.askQuestions).toEqual([])
    expect(acp.elicitations).toEqual([])
    expect(acp.server.cancelledForms.map((item) => item.message)).toEqual([
      ACPElicitation.UnshownQuestionMessage,
    ])
  })

  test("a failing extension request degrades to cancelUnshown instead of a dismissal", async () => {
    await using acp = await startSession({
      capabilities: { questionExt: true },
      onPrompt: ({ sessionID, id }) => [
        delivered(sessionID, id),
        toolStarted(sessionID, "call_question", "question"),
        questions(sessionID),
      ],
      askUserQuestion: () => {
        throw new Error("question UI failed")
      },
      onFormCancel: ({ sessionID }) => [succeeded(sessionID)],
    })

    expect((await acp.prompt(acp.sessionId, "hello")).stopReason).toBe("end_turn")
    expect(acp.server.cancelledForms.map((item) => item.message)).toEqual([
      ACPElicitation.UnshownQuestionMessage,
    ])
  })
})
