import { describe, expect, test } from "bun:test"
import { assistantMessage, makeSession, rpcError, startWire, tokens } from "./wire-fixture"

/**
 * These cover the `x.ai/*` extension surface the grok-derived TUI depends on.
 * The wire client is untyped for custom methods, so the request helper is
 * called with an explicit response type.
 */

type ListRow = {
  readonly sessionId: string
  readonly cwd: string
  readonly summary: string
  readonly updatedAt: string
  readonly createdAt: string
  readonly source: string
}

type ListResponse = {
  readonly sessions: ListRow[]
  readonly nextCursor?: string
  readonly _meta: { readonly "x.ai/listScope": string }
}

type InfoResponse = {
  readonly result: {
    readonly sessionId: string
    readonly cwd: string
    readonly model: string | null
    readonly turns: number
    readonly context: {
      readonly used: number
      readonly total: number
      readonly messageCount: number
      readonly usagePct: number
    }
  }
}

// `Wire.request` is typed only for the ACP standard methods; custom extension
// methods go through the SDK's untyped overload.
const ext = <T>(acp: Awaited<ReturnType<typeof startWire>>, method: string, params: unknown) =>
  (acp.request as unknown as (method: string, params: unknown) => Promise<T>)(method, params)

const list = (acp: Awaited<ReturnType<typeof startWire>>, params: Record<string, unknown> = {}) =>
  ext<ListResponse>(acp, "x.ai/session/list", params)

const sessionInfo = (acp: Awaited<ReturnType<typeof startWire>>, params: Record<string, unknown>) =>
  ext<InfoResponse>(acp, "x.ai/session/info", params)

describe("x.ai/session/list", () => {
  test("is registered instead of answering method-not-found", async () => {
    await using acp = await startWire()
    await acp.initialize()
    const response = await list(acp)
    expect(response.sessions).toEqual([])
    expect(response._meta["x.ai/listScope"]).toBe("all")
  })

  test("answers under the leading-underscore spelling the Rust client sends", async () => {
    await using acp = await startWire()
    acp.server.sessions.set("ses_here", makeSession("ses_here", { cwd: "/workspace", time: { created: 1, updated: 2 } }))
    await acp.initialize()

    // `agent_client_protocol` encodes a custom method as `_x.ai/...` and strips
    // the prefix when decoding the reply, so the TUI never asks for the bare name.
    const prefixed = await ext<ListResponse>(acp, "_x.ai/session/list", { cwd: "/workspace" })
    expect(prefixed.sessions.map((row) => row.sessionId)).toEqual(["ses_here"])

    const info = await ext<InfoResponse>(acp, "_x.ai/session/info", { sessionId: "ses_here" })
    expect(info.result.sessionId).toBe("ses_here")
  })

  test("an exact cwd lists only that directory and reports the cwd scope", async () => {
    await using acp = await startWire()
    acp.server.sessions.set(
      "ses_here",
      makeSession("ses_here", { cwd: "/workspace", time: { created: 1, updated: 2 } }),
    )
    acp.server.sessions.set(
      "ses_elsewhere",
      makeSession("ses_elsewhere", { cwd: "/other", time: { created: 1, updated: 3 } }),
    )
    await acp.initialize()

    const scoped = await list(acp, { cwd: "/workspace" })

    expect(scoped.sessions.map((row) => row.sessionId)).toEqual(["ses_here"])
    expect(scoped._meta["x.ai/listScope"]).toBe("cwd")
    expect(scoped.sessions[0]).toMatchObject({
      cwd: "/workspace",
      // `opencode` is the pager's daemon-row source: it routes a pick straight
      // to ACP `session/load` instead of the local grok store or registry restore.
      source: "opencode",
      summary: "Session ses_here",
    })
    // The picker drops rows without a usable label, so the timestamps must parse.
    expect(Number.isNaN(Date.parse(scoped.sessions[0]!.updatedAt))).toBe(false)
    expect(Number.isNaN(Date.parse(scoped.sessions[0]!.createdAt))).toBe(false)
  })

  test("allowRelax widens the listing across directories", async () => {
    await using acp = await startWire()
    acp.server.sessions.set("ses_a", makeSession("ses_a", { cwd: "/workspace", time: { created: 1, updated: 2 } }))
    acp.server.sessions.set("ses_b", makeSession("ses_b", { cwd: "/other", time: { created: 1, updated: 3 } }))
    await acp.initialize()

    const relaxed = await list(acp, { cwd: "/workspace", allowRelax: true })

    expect(relaxed.sessions.map((row) => row.sessionId)).toEqual(["ses_b", "ses_a"])
    expect(relaxed._meta["x.ai/listScope"]).toBe("all")
  })

  test("query is forwarded as the kernel's title search and limit caps the page", async () => {
    await using acp = await startWire()
    acp.server.sessions.set("ses_one", makeSession("ses_one", { cwd: "/workspace", time: { created: 1, updated: 3 } }))
    acp.server.sessions.set("ses_two", makeSession("ses_two", { cwd: "/workspace", time: { created: 1, updated: 2 } }))
    await acp.initialize()

    await list(acp, { cwd: "/workspace", query: "ses_one" })
    const searched = acp.server.requests.filter((request) => request.path === "/api/session").at(-1)
    expect(searched?.query).toMatchObject({ search: "ses_one", directory: "/workspace" })

    const capped = await list(acp, { cwd: "/workspace", limit: 1 })
    expect(capped.sessions).toHaveLength(1)
  })

  test("a cursor pages through the remaining sessions", async () => {
    await using acp = await startWire()
    for (const index of [1, 2, 3]) {
      acp.server.sessions.set(
        `ses_${index}`,
        makeSession(`ses_${index}`, { cwd: "/workspace", time: { created: 1, updated: index } }),
      )
    }
    await acp.initialize()

    const first = await list(acp, { cwd: "/workspace", limit: 2 })
    expect(first.sessions.map((row) => row.sessionId)).toEqual(["ses_3", "ses_2"])
    expect(first.nextCursor).toBeDefined()

    const second = await list(acp, { cwd: "/workspace", limit: 2, cursor: first.nextCursor })
    expect(second.sessions.map((row) => row.sessionId)).toEqual(["ses_1"])
    expect(second.nextCursor).toBeUndefined()
  })

  test("a relative cwd is rejected instead of silently listing everything", async () => {
    await using acp = await startWire()
    await acp.initialize()
    expect(await rpcError(list(acp, { cwd: "workspace" }))).toMatchObject({
      code: -32602,
      data: { field: "cwd" },
    })
  })
})

describe("x.ai/session/info", () => {
  test("reports the session's model window and durable message accounting", async () => {
    await using acp = await startWire()
    acp.server.sessions.set(
      "ses_info",
      makeSession("ses_info", {
        cwd: "/workspace",
        model: { providerID: "test", id: "test-model" },
        time: { created: 1, updated: 2 },
      }),
    )
    acp.server.messages.set("ses_info", [
      { id: "msg_user", type: "user", text: "hello there", time: { created: 1 } },
      assistantMessage("msg_assistant", {
        content: [{ type: "text", text: "hi" }],
        tokens: { ...tokens(0), input: 400, cache: { read: 100, write: 0 } },
      }),
    ])
    await acp.initialize()

    const info = await sessionInfo(acp, { sessionId: "ses_info" })

    // The pager reads `{ result }` here, so the wire shape is asserted, not just the values.
    expect(info.result.sessionId).toBe("ses_info")
    expect(info.result.cwd).toBe("/workspace")
    expect(info.result.model).toBe("test/test-model")
    // What the status bar calls "used" is the last step's prompt size: 400 + 100.
    expect(info.result.context.used).toBe(500)
    expect(info.result.context.total).toBe(100_000)
    expect(info.result.context.messageCount).toBe(2)
    expect(info.result.context.usagePct).toBe(1)
  })

  test("an unknown session is an invalid-params error, not a defect", async () => {
    await using acp = await startWire()
    await acp.initialize()
    expect(
      await rpcError(ext(acp, "x.ai/session/info", { sessionId: "ses_missing" })),
    ).toMatchObject({ code: -32602 })
  })
})
