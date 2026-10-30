import { describe, expect, test } from "bun:test"
import { Schema } from "effect"
import { assistantMessage, durableEvent, enqueued, rpcError, startSession, userMessage } from "./wire-fixture"

// The `/compact` extension only settles on the kernel's terminal compaction
// event; `/rename`, `/delete`, and `/fork` each map to one session endpoint.
// The pager owns their banners, so these must fail with a typed reason instead
// of a bare method-not-found.

const decodeCompact = Schema.decodeUnknownSync(Schema.Struct({ id: Schema.String }))

describe("x.ai extension methods the pager renders itself", () => {
  test("compact_conversation waits for the compaction to settle before replying", async () => {
    const held = Promise.withResolvers<Response>()
    await using acp = await startSession({
      fetch: (request) => (request.path.endsWith("/compact") ? held.promise : undefined),
    })

    const compacting = acp.extend<Record<string, never>, { sessionId: string }>("x.ai/compact_conversation", {
      sessionId: acp.sessionId,
    })
    const request = await acp.until(
      () => acp.server.requests.find((item) => item.path.endsWith("/compact")),
      "compact request",
    )
    const id = decodeCompact(request.body).id
    let settled = false
    void compacting.then(
      () => (settled = true),
      () => (settled = true),
    )
    await Promise.resolve()
    expect(settled).toBe(false)

    acp.server.send(
      durableEvent("session.compaction.started", {
        sessionID: acp.sessionId,
        reason: "manual",
        inputID: id,
      }),
      durableEvent("session.compaction.ended", {
        sessionID: acp.sessionId,
        reason: "manual",
        text: "Summary of the earlier conversation",
      }),
    )
    held.resolve(enqueued(acp.sessionId, id, "compaction", {}))

    expect(await compacting).toEqual({})
  })

  test("compact_conversation reports the reason the kernel gave", async () => {
    const held = Promise.withResolvers<Response>()
    await using acp = await startSession({
      fetch: (request) => (request.path.endsWith("/compact") ? held.promise : undefined),
    })

    const compacting = acp.extend<Record<string, never>, { sessionId: string }>("x.ai/compact_conversation", {
      sessionId: acp.sessionId,
    })
    const request = await acp.until(
      () => acp.server.requests.find((item) => item.path.endsWith("/compact")),
      "compact request",
    )
    const id = decodeCompact(request.body).id
    acp.server.send(
      durableEvent("session.compaction.failed", {
        sessionID: acp.sessionId,
        reason: "manual",
        inputID: id,
        error: { type: "compaction.unavailable", message: "Nothing to compact yet" },
      }),
    )
    held.resolve(enqueued(acp.sessionId, id, "compaction", {}))

    const error = await rpcError(compacting)
    expect(error.data).toEqual({ kind: "compact_failed", message: "Nothing to compact yet" })
  })

  test("session/rename sets the title", async () => {
    await using acp = await startSession()

    await acp.extend<Record<string, never>, { sessionId: string; title: string }>("x.ai/session/rename", {
      sessionId: acp.sessionId,
      title: "Renamed by the pager",
    })

    const patched = acp.server.requests.find((item) => item.method === "PATCH")
    expect(patched?.body).toMatchObject({ title: "Renamed by the pager" })
    expect(acp.server.sessions.get(acp.sessionId)?.title).toBe("Renamed by the pager")
  })

  test("session/rename refuses a blank title", async () => {
    await using acp = await startSession()

    const error = await rpcError(
      acp.extend<Record<string, never>, { sessionId: string; title: string }>("x.ai/session/rename", {
        sessionId: acp.sessionId,
        title: "   ",
      }),
    )

    expect(error.code).toBe(-32602)
    expect(error.message).toContain("title must not be blank")
  })

  test("session/delete removes the session", async () => {
    await using acp = await startSession()

    await acp.extend<Record<string, never>, { sessionId: string }>("x.ai/session/delete", {
      sessionId: acp.sessionId,
    })

    expect(acp.server.sessions.has(acp.sessionId)).toBe(false)
  })

  test("session/fork returns the new session id the pager loads", async () => {
    await using acp = await startSession()
    const source = acp.server.sessions.get(acp.sessionId)

    const forked = await acp.extend<{ newSessionId: string }, { sourceSessionId: string; newCwd: string }>(
      "x.ai/session/fork",
      { sourceSessionId: acp.sessionId, newCwd: "/workspace" },
    )

    expect(forked.newSessionId).not.toBe(acp.sessionId)
    expect(acp.server.sessions.get(forked.newSessionId)?.location).toEqual(source?.location)
  })

  test("session/usage folds each step's tokens and cost", async () => {
    await using acp = await startSession()
    acp.server.messages.set(acp.sessionId, [
      assistantMessage("msg_1", {
        tokens: { input: 100, output: 20, reasoning: 5, cache: { read: 40, write: 10 } },
        cost: 0.25,
        time: { created: 1, streamed: 1_000, completed: 1_500 },
      }),
      assistantMessage("msg_2", { tokens: { input: 50, output: 10, reasoning: 0, cache: { read: 0, write: 0 } }, cost: 0.5 }),
    ])

    const usage = await acp.extend<{ usage: Record<string, number | object> }, { sessionId: string }>(
      "x.ai/session/usage",
      { sessionId: acp.sessionId },
    )

    // `inputTokens` is the full prompt: 100 + 40 + 10, then 50.
    expect(usage.usage).toMatchObject({
      inputTokens: 200,
      outputTokens: 30,
      totalTokens: 230,
      cachedReadTokens: 40,
      cacheCreationTokens: 10,
      reasoningTokens: 5,
      modelCalls: 2,
      numTurns: 2,
      // Server cost is reported in 1e10 ticks per USD.
      costUsdTicks: 7_500_000_000,
    })
    expect(usage.usage.apiDurationMs).toBe(500)
  })

  test("session/usage marks a partially billed row instead of reporting zero cost", async () => {
    await using acp = await startSession()
    acp.server.messages.set(acp.sessionId, [
      assistantMessage("msg_1", { cost: 0.5 }),
      assistantMessage("msg_2", { cost: undefined }),
    ])

    const usage = await acp.extend<{ usage: Record<string, unknown> }, { sessionId: string }>("x.ai/session/usage", {
      sessionId: acp.sessionId,
    })

    expect(usage.usage.costUsdTicks).toBe(5_000_000_000)
    expect(usage.usage.costIsPartial).toBe(true)
  })

  test("session/usage on an empty session reports no model calls", async () => {
    await using acp = await startSession()

    const usage = await acp.extend<{ usage: Record<string, unknown> }, { sessionId: string }>("x.ai/session/usage", {
      sessionId: acp.sessionId,
    })

    expect(usage.usage).toMatchObject({ inputTokens: 0, outputTokens: 0, modelCalls: 0 })
    expect(usage.usage.costUsdTicks).toBeUndefined()
  })

  test("billing answers with no config so the usage modal has nothing to bill", async () => {
    await using acp = await startSession()

    const billing = await acp.extend<{ config: null }, Record<string, never>>("x.ai/billing", {})

    expect(billing).toEqual({ config: null })
  })

  test("rewind/points numbers only user prompts", async () => {
    await using acp = await startSession()
    acp.server.messages.set(acp.sessionId, [
      userMessage("msg_u1", "first prompt"),
      assistantMessage("msg_a1"),
      userMessage("msg_u2", "second prompt"),
      assistantMessage("msg_a2"),
      userMessage("msg_u3", "third prompt"),
    ])

    const points = await acp.extend<{ rewindPoints: Array<Record<string, unknown>> }, { sessionId: string }>(
      "x.ai/rewind/points",
      { sessionId: acp.sessionId },
    )

    expect(points.rewindPoints.map((point) => point.promptIndex)).toEqual([0, 1, 2])
    expect(points.rewindPoints[1]?.promptPreview).toBe("second prompt")
    // Rewind is conversation-only here, so nothing advertises a file snapshot.
    expect(points.rewindPoints.every((point) => point.hasFileChanges === false)).toBe(true)
  })

  test("rewind/execute truncates from the chosen prompt and returns its text", async () => {
    await using acp = await startSession()
    acp.server.messages.set(acp.sessionId, [
      userMessage("msg_u1", "first prompt"),
      assistantMessage("msg_a1"),
      userMessage("msg_u2", "second prompt"),
      assistantMessage("msg_a2"),
    ])

    const response = await acp.extend<Record<string, unknown>, Record<string, unknown>>("x.ai/rewind/execute", {
      sessionId: acp.sessionId,
      targetPromptIndex: 1,
      force: true,
      mode: "conversation_only",
    })

    expect(response).toMatchObject({ success: true, targetPromptIndex: 1, promptText: "second prompt" })
    // The boundary message and everything after it are gone; prompt 0 survives.
    expect(acp.server.messages.get(acp.sessionId)?.map((message) => message.id)).toEqual(["msg_u1", "msg_a1"])
    expect(acp.server.reverts).toEqual([{ sessionID: acp.sessionId, messageID: "msg_u2", files: false }])
  })

  test("rewind/execute reports an out-of-range prompt instead of truncating", async () => {
    await using acp = await startSession()
    acp.server.messages.set(acp.sessionId, [userMessage("msg_u1", "only prompt"), assistantMessage("msg_a1")])

    const response = await acp.extend<Record<string, unknown>, Record<string, unknown>>("x.ai/rewind/execute", {
      sessionId: acp.sessionId,
      targetPromptIndex: 4,
    })

    expect(response.success).toBe(false)
    expect(response.error).toContain("no prompt at index 4")
    expect(acp.server.reverts).toEqual([])
    expect(acp.server.messages.get(acp.sessionId)).toHaveLength(2)
  })

  test("rewind/execute refuses a missing target index", async () => {
    await using acp = await startSession()
    acp.server.messages.set(acp.sessionId, [userMessage("msg_u1", "only prompt")])

    const error = await rpcError(
      acp.extend<Record<string, never>, Record<string, unknown>>("x.ai/rewind/execute", {
        sessionId: acp.sessionId,
      }),
    )

    expect(error.code).toBe(-32602)
    expect(error.message).toContain("targetPromptIndex is required")
  })

  test("rewind/execute refuses files-only instead of truncating history", async () => {
    await using acp = await startSession()
    acp.server.messages.set(acp.sessionId, [userMessage("msg_u1", "one"), userMessage("msg_u2", "two")])

    const response = await acp.extend<Record<string, unknown>, Record<string, unknown>>("x.ai/rewind/execute", {
      sessionId: acp.sessionId,
      targetPromptIndex: 1,
      mode: "files_only",
    })

    expect(response.success).toBe(false)
    expect(response.error).toContain("files-only rewind is not supported")
    expect(acp.server.messages.get(acp.sessionId)).toHaveLength(2)
  })

  test("plugins/list reports the loaded plugins with the fields the modal needs", async () => {
    await using acp = await startSession()
    acp.server.catalog.plugins.push({
      id: "user/local-tools",
      source: { type: "local", path: "/workspace/.plugins/local-tools" },
      features: { server: true },
      state: { status: "active" },
    })

    const listed = await acp.extend<{ plugins: Array<Record<string, unknown>> }, { sessionId: string }>(
      "x.ai/plugins/list",
      { sessionId: acp.sessionId },
    )

    // The builtin module the kernel always loads is not a user-installable
    // plugin, so only the local one is listed.
    expect(listed.plugins).toHaveLength(1)
    expect(listed.plugins[0]).toMatchObject({
      name: "local-tools",
      id: "user/local-tools",
      root: "/workspace/.plugins/local-tools",
      scope: "project",
      enabled: true,
      trusted: true,
      hookStatus: "none",
      mcpStatus: "none",
    })
  })

  test("mcp/list maps live server status and keeps a failure reason", async () => {
    await using acp = await startSession()

    const listed = await acp.extend<{ servers: Array<Record<string, unknown>> }, { sessionId: string }>(
      "x.ai/mcp/list",
      { sessionId: acp.sessionId },
    )

    const byName = new Map(listed.servers.map((server) => [server.name as string, server]))
    expect(byName.get("test-server")).toMatchObject({
      displayName: "Test Server",
      enabled: true,
      session: { enabled: true, status: "ready" },
    })
    // A failed server shows the reason instead of a bare "unavailable".
    expect(byName.get("broken-server")).toMatchObject({ session: { status: "unavailable" }, sourceLabel: "spawn ENOENT" })
  })

  test("hooks/list reports no hooks without inventing rows", async () => {
    await using acp = await startSession()

    const listed = await acp.extend<Record<string, unknown>, { sessionId: string }>("x.ai/hooks/list", {
      sessionId: acp.sessionId,
    })

    expect(listed).toEqual({ hooks: [], projectTrusted: true, loadErrors: [] })
  })

  test("plugin and hook actions answer unsupported instead of failing the RPC", async () => {
    await using acp = await startSession()

    const plugins = await acp.extend<Record<string, unknown>, Record<string, unknown>>("x.ai/plugins/action", {
      sessionId: acp.sessionId,
      action: { type: "reload" },
    })
    const hooks = await acp.extend<Record<string, unknown>, Record<string, unknown>>("x.ai/hooks/action", {
      sessionId: acp.sessionId,
      action: { type: "reload" },
    })

    expect(plugins).toMatchObject({ status: "unsupported", requiresReload: false })
    expect(hooks).toMatchObject({ status: "unsupported", requiresRestart: false })
  })
})
