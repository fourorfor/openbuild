import { describe, expect, test } from "bun:test"
import { Message } from "@opencode/ai/effect"
import { SessionProtocolPolicy } from "@opencode/core/session/protocol-policy"
import { ProtocolRefusalHistory } from "@opencode/core/session/refusal-history"

describe("SessionProtocolPolicy.decode", () => {
  test("an unreadable file leaves the mode off", () => {
    expect(SessionProtocolPolicy.decode("{").enabled).toBe(false)
    expect(SessionProtocolPolicy.decode("[]").enabled).toBe(false)
    expect(SessionProtocolPolicy.decode("null").enabled).toBe(false)
  })

  test("a v3 file keeps every field", () => {
    const config = SessionProtocolPolicy.decode(
      JSON.stringify({
        version: 3,
        enabled: true,
        mode: "protocol",
        packs: ["binary"],
        providerRules: "auto",
        providerBody: { gateway: { temperature: 0.9 } },
        rewriteRefusalHistory: false,
        refusalRetryBudget: 5,
        inheritSubagents: true,
      }),
    )
    expect(config).toEqual({
      version: 3,
      enabled: true,
      mode: "protocol",
      packs: ["binary"],
      providerRules: "auto",
      providerBody: { gateway: { temperature: 0.9 } },
      rewriteRefusalHistory: false,
      refusalRetryBudget: 5,
      inheritSubagents: true,
    })
  })

  test("a v2 hybrid file migrates to the legacy-extended pack", () => {
    const config = SessionProtocolPolicy.decode(
      JSON.stringify({
        version: 2,
        enabled: true,
        policySource: "hybrid",
        providerRewrite: "off",
        providerRewriteBody: { "my-gateway": { top_p: 0.9 } },
        stripRefusalHistory: false,
        refusalRetryBudget: 1,
      }),
    )
    expect(config.packs).toEqual(["legacy-extended"])
    expect(config.providerRules).toBe("off")
    expect(config.providerBody).toEqual({ "my-gateway": { top_p: 0.9 } })
    expect(config.rewriteRefusalHistory).toBe(false)
    expect(config.refusalRetryBudget).toBe(1)
  })

  test("a v2 compact file loads no pack", () => {
    const config = SessionProtocolPolicy.decode(JSON.stringify({ version: 2, enabled: true, policySource: "compact" }))
    expect(config.packs).toEqual([])
  })

  test("out-of-range budgets and unknown modes fall back", () => {
    const config = SessionProtocolPolicy.decode(
      JSON.stringify({ version: 3, enabled: true, refusalRetryBudget: 99, mode: "bogus" }),
    )
    expect(config.refusalRetryBudget).toBe(2)
    expect(config.mode).toBe("protocol")
  })
})

describe("SessionProtocolPolicy.merge", () => {
  test("nested records merge and scalars replace", () => {
    expect(SessionProtocolPolicy.merge({ a: { b: 1, c: 2 } }, { a: { b: 9 } })).toEqual({ a: { b: 9, c: 2 } })
  })

  test("arrays replace rather than concatenate", () => {
    expect(SessionProtocolPolicy.merge({ list: [1, 2] }, { list: [3] })).toEqual({ list: [3] })
  })

  test("an undefined side keeps the other side", () => {
    expect(SessionProtocolPolicy.merge(undefined, { a: 1 })).toEqual({ a: 1 })
    expect(SessionProtocolPolicy.merge({ a: 1 }, undefined)).toEqual({ a: 1 })
    expect(SessionProtocolPolicy.merge(undefined, undefined)).toBeUndefined()
  })
})

const assistant = (text: string) => Message.assistant(text)

describe("ProtocolRefusalHistory.rewrite", () => {
  test("rewrites a trailing run of three refusals", () => {
    const messages = [assistant("抱歉，我无法协助这个请求。"), assistant("我不能这样做。"), assistant("很抱歉。")]
    const result = ProtocolRefusalHistory.rewrite({ messages, enabled: true, maxRewrites: 10 })
    expect(result.rewritten).toBe(3)
    expect(result.messages.map((message) => message.content[0])).toEqual([
      { type: "text", text: ProtocolRefusalHistory.VOID_REFUSAL_ZH },
      { type: "text", text: ProtocolRefusalHistory.VOID_REFUSAL_ZH },
      { type: "text", text: ProtocolRefusalHistory.VOID_REFUSAL_ZH },
    ])
  })

  test("stops at the first assistant message that is real work", () => {
    const messages = [
      assistant("抱歉，我无法协助。"),
      assistant("Here is the patch:\n```ts\nconst a = 1\n```"),
      assistant("I can't help with that."),
    ]
    const result = ProtocolRefusalHistory.rewrite({ messages, enabled: true, maxRewrites: 10 })
    expect(result.rewritten).toBe(1)
    expect(result.messages[1]!.content[0]).toMatchObject({ type: "text" })
  })

  test("never rewrites a message containing a code block", () => {
    const messages = [assistant("I can't do that.\n```sh\nls\n```")]
    expect(ProtocolRefusalHistory.rewrite({ messages, enabled: true, maxRewrites: 10 }).rewritten).toBe(0)
  })

  test("never rewrites a message carrying a tool call", () => {
    const messages = [
      Message.assistant([
        Message.text("I cannot do that."),
        { type: "tool-call", id: "call_1", name: "read", input: {} },
      ]),
    ]
    expect(ProtocolRefusalHistory.rewrite({ messages, enabled: true, maxRewrites: 10 }).rewritten).toBe(0)
  })

  test("honours the rewrite cap", () => {
    const messages = [assistant("抱歉。"), assistant("抱歉。"), assistant("抱歉。")]
    expect(ProtocolRefusalHistory.rewrite({ messages, enabled: true, maxRewrites: 2 }).rewritten).toBe(2)
  })

  test("returns the same array when nothing is rewritten", () => {
    const messages = [assistant("Here is the diff.")]
    expect(ProtocolRefusalHistory.rewrite({ messages, enabled: true, maxRewrites: 10 }).messages).toBe(messages)
  })

  test("a disabled config is a no-op", () => {
    const messages = [assistant("抱歉。")]
    const result = ProtocolRefusalHistory.rewrite({ messages, enabled: false, maxRewrites: 10 })
    expect(result.messages).toBe(messages)
    expect(result.rewritten).toBe(0)
  })

  test("an idiom that opens with a refusal word is not a refusal", () => {
    const messages = [assistant("I can't believe this works — here is why.")]
    expect(ProtocolRefusalHistory.rewrite({ messages, enabled: true, maxRewrites: 10 }).rewritten).toBe(0)
  })

  test("short refusal prose without a listed opening is caught", () => {
    const messages = [assistant("That request is outside what I am able to help with; I cannot assist.")]
    expect(ProtocolRefusalHistory.rewrite({ messages, enabled: true, maxRewrites: 10 }).rewritten).toBe(1)
  })

  test("keeps the original when the language is not CJK", () => {
    const messages = [assistant("I cannot assist with that.")]
    const result = ProtocolRefusalHistory.rewrite({ messages, enabled: true, maxRewrites: 10 })
    expect(result.messages[0]!.content[0]).toEqual({ type: "text", text: ProtocolRefusalHistory.VOID_REFUSAL })
  })
})
