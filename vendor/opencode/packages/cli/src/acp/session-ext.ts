import type { OpenCodeClient, OpenCodeEvent } from "@opencode/client/effect"
import { Session } from "@opencode/schema/session"
import { SessionMessage } from "@opencode/schema/session-message"
import type { Plugin } from "@opencode/schema/plugin"
import { displayLabel, withTimestampedFallback } from "@opencode/util/session-title-fallback"
import { DateTime, Effect, Fiber, Option, Queue, Schema, Stream } from "effect"
import { ACPClient } from "./client"
import { ACPCompaction } from "./compaction"
import { ACPDirectories } from "./directories"
import { ACPError } from "./error"
import { RequestError } from "@agentclientprotocol/sdk"

/**
 * Extensions the grok-derived TUI asks for under the `x.ai/*` namespace.
 *
 * ACP has no custom-method typing, so these register with an explicit params
 * parser. They are additive: the standard `session/list` keeps its exact-cwd
 * semantics for every other client.
 */

const ListParams = Schema.Struct({
  cwd: Schema.optional(Schema.NullOr(Schema.String)),
  query: Schema.optional(Schema.NullOr(Schema.String)),
  limit: Schema.optional(Schema.NullOr(Schema.Number)),
  cursor: Schema.optional(Schema.NullOr(Schema.String)),
  allowRelax: Schema.optional(Schema.NullOr(Schema.Boolean)),
})

const InfoParams = Schema.Struct({ sessionId: Schema.String })

/** `/compact` sends only the session id, under either spelling used by older shells. */
const CompactParams = Schema.Struct({
  sessionId: Schema.optional(Schema.NullOr(Schema.String)),
  session_id: Schema.optional(Schema.NullOr(Schema.String)),
})

/**
 * `/rename` and `/delete` send the session id plus the cwd the pager believes
 * it is in; `/fork` also names the fork and, for a worktree parent, its
 * workspace directory. Field names mirror the Rust structs' camelCase wire.
 */
const RenameParams = Schema.Struct({
  sessionId: Schema.optional(Schema.NullOr(Schema.String)),
  title: Schema.optional(Schema.NullOr(Schema.String)),
  resetToAuto: Schema.optional(Schema.NullOr(Schema.Boolean)),
})

const DeleteParams = Schema.Struct({
  sessionId: Schema.optional(Schema.NullOr(Schema.String)),
})

const ForkParams = Schema.Struct({
  sourceSessionId: Schema.optional(Schema.NullOr(Schema.String)),
  newSessionId: Schema.optional(Schema.NullOr(Schema.String)),
  newCwd: Schema.optional(Schema.NullOr(Schema.String)),
})

const UsageParams = Schema.Struct({
  sessionId: Schema.optional(Schema.NullOr(Schema.String)),
  session_id: Schema.optional(Schema.NullOr(Schema.String)),
})

/** `/rewind` names the session and, to execute, the 0-based prompt index to come back to. */
const RewindPointsParams = Schema.Struct({
  sessionId: Schema.optional(Schema.NullOr(Schema.String)),
  session_id: Schema.optional(Schema.NullOr(Schema.String)),
})

const RewindExecuteParams = Schema.Struct({
  sessionId: Schema.optional(Schema.NullOr(Schema.String)),
  session_id: Schema.optional(Schema.NullOr(Schema.String)),
  targetPromptIndex: Schema.optional(Schema.NullOr(Schema.Number)),
  target_prompt_index: Schema.optional(Schema.NullOr(Schema.Number)),
  mode: Schema.optional(Schema.NullOr(Schema.String)),
})

/** The surface modals send only the session id (MCP also sends a cache flag). */
const SurfaceParams = Schema.Struct({
  sessionId: Schema.optional(Schema.NullOr(Schema.String)),
  session_id: Schema.optional(Schema.NullOr(Schema.String)),
})

const decodeList = Schema.decodeUnknownOption(ListParams)
const decodeInfo = Schema.decodeUnknownOption(InfoParams)
const decodeCompact = Schema.decodeUnknownOption(CompactParams)
const decodeRename = Schema.decodeUnknownOption(RenameParams)
const decodeDelete = Schema.decodeUnknownOption(DeleteParams)
const decodeFork = Schema.decodeUnknownOption(ForkParams)
const decodeUsage = Schema.decodeUnknownOption(UsageParams)
const decodeRewindPoints = Schema.decodeUnknownOption(RewindPointsParams)
const decodeRewindExecute = Schema.decodeUnknownOption(RewindExecuteParams)
const decodeSurface = Schema.decodeUnknownOption(SurfaceParams)

const DEFAULT_LIMIT = 30
const MAX_LIMIT = 100
const PAGE_SIZE = 200

/** The pager scales server cost by this before printing dollars. */
const USD_TICKS_PER_USD = 1e10

type CompactionEvent = Extract<
  OpenCodeEvent,
  {
    readonly type:
      | "session.compaction.started"
      | "session.compaction.ended"
      | "session.compaction.failed"
  }
>

/** The pager routes a compact failure on `data.kind` and renders `data.message`. */
const compactFailure = (error: SessionErrorLike) => {
  const cancelled = error.type === "aborted" || error.type === "compaction.interrupted"
  return RequestError.internalError(
    {
      kind: cancelled ? "compact_cancelled" : "compact_failed",
      message: cancelled ? "compact cancelled" : ACPCompaction.displayError(error),
    },
    cancelled ? "Compaction cancelled" : "Compaction failed",
  )
}

type SessionErrorLike = { readonly type: string; readonly message: string }

/** Unknown shapes fall back to the documented defaults (no cwd, no query), which list everything. */
export type ListRequest = typeof ListParams.Type
export type InfoRequest = typeof InfoParams.Type
export type CompactRequest = { readonly sessionId: string }
export type RenameRequest = {
  readonly sessionId: string
  readonly title: string
  readonly resetToAuto: boolean
}
export type DeleteRequest = { readonly sessionId: string }
export type ForkRequest = {
  readonly sessionId: string
  readonly newSessionId?: string
  readonly cwd?: string
}
export type UsageRequest = { readonly sessionId: string }
export type RewindPointsRequest = { readonly sessionId: string }
/** The plugins/hooks/MCP modals open per session and send nothing else that matters. */
export type SurfaceListRequest = { readonly sessionId: string }
export type RewindExecuteRequest = {
  readonly sessionId: string
  /** `-1` means the field was absent; the handler rejects it rather than guessing a target. */
  readonly targetPromptIndex: number
  readonly mode: "conversation_only" | "all" | "files_only"
}
export const parseListParams = (params: unknown): ListRequest =>
  Option.getOrElse(decodeList(params ?? {}), () => ({}))

export const parseInfoParams = (params: unknown): InfoRequest =>
  Option.getOrElse(decodeInfo(params ?? {}), () => ({ sessionId: "" }))

export const parseCompactParams = (params: unknown): CompactRequest =>
  Option.match(decodeCompact(params ?? {}), {
    onNone: () => ({ sessionId: "" }),
    onSome: (value) => ({ sessionId: value.sessionId ?? value.session_id ?? "" }),
  })

export const parseRenameParams = (params: unknown): RenameRequest =>
  Option.match(decodeRename(params ?? {}), {
    onNone: () => ({ sessionId: "", title: "", resetToAuto: false }),
    onSome: (value) => ({
      sessionId: value.sessionId ?? "",
      title: value.title ?? "",
      resetToAuto: value.resetToAuto === true,
    }),
  })

export const parseDeleteParams = (params: unknown): DeleteRequest =>
  Option.match(decodeDelete(params ?? {}), {
    onNone: () => ({ sessionId: "" }),
    onSome: (value) => ({ sessionId: value.sessionId ?? "" }),
  })

export const parseForkParams = (params: unknown): ForkRequest =>
  Option.match(decodeFork(params ?? {}), {
    onNone: () => ({ sessionId: "" }),
    onSome: (value) => ({
      sessionId: value.sourceSessionId ?? "",
      ...(value.newSessionId ? { newSessionId: value.newSessionId } : {}),
      ...(value.newCwd ? { cwd: value.newCwd } : {}),
    }),
  })

export const parseUsageParams = (params: unknown): UsageRequest =>
  Option.match(decodeUsage(params ?? {}), {
    onNone: () => ({ sessionId: "" }),
    onSome: (value) => ({ sessionId: value.sessionId ?? value.session_id ?? "" }),
  })

export const parseRewindPointsParams = (params: unknown): RewindPointsRequest =>
  Option.match(decodeRewindPoints(params ?? {}), {
    onNone: () => ({ sessionId: "" }),
    onSome: (value) => ({ sessionId: value.sessionId ?? value.session_id ?? "" }),
  })

export const parseSurfaceParams = (params: unknown): SurfaceListRequest =>
  Option.match(decodeSurface(params ?? {}), {
    onNone: () => ({ sessionId: "" }),
    onSome: (value) => ({ sessionId: value.sessionId ?? value.session_id ?? "" }),
  })

export const parseRewindExecuteParams = (params: unknown): RewindExecuteRequest => {
  const decoded = decodeRewindExecute(params ?? {})
  if (Option.isNone(decoded)) return { sessionId: "", targetPromptIndex: -1, mode: "conversation_only" }
  return {
    sessionId: decoded.value.sessionId ?? decoded.value.session_id ?? "",
    targetPromptIndex: decoded.value.targetPromptIndex ?? decoded.value.target_prompt_index ?? -1,
    mode: rewindMode(decoded.value.mode),
  }
}

/**
 * The pager always names the mode; an older or careless client that omits it
 * gets the conversation-only behaviour, because restoring snapshots by default
 * could discard uncommitted work in the workspace.
 */
const rewindMode = (mode: string | null | undefined): RewindExecuteRequest["mode"] => {
  if (mode === "all") return "all"
  if (mode === "files_only" || mode === "code_only") return "files_only"
  return "conversation_only"
}

export function make(input: { readonly client: OpenCodeClient }) {
  const modelRef = (session: Session.Info) =>
    session.model ? `${session.model.providerID}/${session.model.id}` : undefined

  /**
   * A session only records a model once one is selected. The kernel still runs
   * such a session on the catalogue default, so `/context` must resolve that
   * default rather than reporting an unknown model with a zero window.
   */
  const resolvedModel = Effect.fnUntraced(function* (session: Session.Info) {
    const declared = modelRef(session)
    if (declared !== undefined) return declared
    const catalogued = yield* input.client.model
      .default({ location: { directory: session.location.directory } })
      .pipe(Effect.catch(ACPClient.classify), Effect.orElseSucceed(() => undefined))
    const model = catalogued?.data
    return model === undefined ? undefined : `${model.providerID}/${model.id}`
  })

  /**
   * The picker labels a row from `summary`/`firstPrompt` only, and falls back to
   * the local grok session store when both are absent — where these sessions do
   * not exist, so an empty summary silently drops the row.
   */
  const row = (session: Session.Info) => ({
    sessionId: session.id,
    cwd: session.location.directory,
    summary: displayLabel(session),
    title: withTimestampedFallback({ ...session, time: { created: DateTime.toEpochMillis(session.time.created) } }),
    updatedAt: DateTime.formatIso(session.time.updated),
    createdAt: DateTime.formatIso(session.time.created),
    // Not "local": the picker reads that as "this session lives in the local
    // grok store", and when that lookup misses it refuses to send
    // `session/load` at all. "remote" is worse: it takes the registry restore
    // path, which this build stubs out. `opencode` is recognised by the pager's
    // daemon-row hooks, which route a pick straight to ACP `session/load`.
    source: "opencode",
    numMessages: 0,
    ...(modelRef(session) === undefined ? {} : { modelId: modelRef(session) }),
    ...(session.parentID === undefined ? {} : { parentId: session.parentID }),
  })

  /**
   * `allowRelax` is the TUI's "every directory" switch. Re-filtering the same
   * cwd would answer it with one directory, which is why cross-directory
   * history stayed invisible while only the extension method was missing.
   */
  const list = Effect.fnUntraced(function* (params: ListRequest) {
    const relaxed = params.allowRelax === true || params.cwd === undefined || params.cwd === null
    const directory = relaxed || params.cwd === undefined || params.cwd === null
      ? undefined
      : yield* ACPDirectories.parseCwd(params.cwd)
    const query = params.query ?? undefined
    const page = yield* input.client.session
      .list({
        ...(directory === undefined ? {} : { directory }),
        ...(query === undefined || query.length === 0 ? {} : { search: query }),
        order: "desc",
        limit: Math.min(Math.max(params.limit ?? DEFAULT_LIMIT, 1), MAX_LIMIT),
        ...(params.cursor ? { cursor: Schema.decodeSync(Session.ListCursor)(params.cursor) } : {}),
      })
      .pipe(Effect.catch(ACPClient.classify))
    return {
      sessions: page.data.map(row),
      ...(page.cursor.next === undefined ? {} : { nextCursor: page.cursor.next }),
      _meta: { "x.ai/listScope": directory === undefined ? ("all" as const) : ("cwd" as const) },
    }
  })

  /**
   * `/context` renders a breakdown from the durable message log. The ACP
   * transcript is shaped for display and has lost the accounting.
   */
  const info = Effect.fnUntraced(function* (params: InfoRequest) {
    const sessionID = yield* ACPClient.decodeSessionID(params.sessionId)
    const session = yield* input.client.session.get({ sessionID }).pipe(Effect.catch(ACPClient.classify))
    const stats = yield* messages(input.client, sessionID).pipe(Stream.runFold(emptyStats, fold))
    const model = yield* resolvedModel(session)
    const total = yield* contextWindow(session, model)
    // The last step's prompt size is what the status bar calls "used"; the
    // byte estimate only matters before the first step has answered.
    const measured = stats.prompt > 0 ? stats.prompt : stats.estimated
    // An unknown window must not report zero occupancy: the pager caps `used`
    // to `total`, so a missing catalogue entry would blank the whole panel.
    const used = total > 0 ? Math.min(measured, total) : measured
    return {
      // The pager deserializes `{ result: T, error?: … }` for this method (see
      // `ExtMethodResult` in `xai-grok-shell`), unlike the list method whose
      // bare payload it also accepts.
      result: {
        sessionId: session.id,
        cwd: session.location.directory,
        // `SessionInfoData` in the pager has no struct-level serde default, so
        // every field it declares is emitted, with null standing in for unknown.
        agentName: session.agent ?? null,
        model: model ?? null,
        modelDisplayName: null,
        resolvedModelId: model ?? null,
        modelFingerprint: null,
        showModelFingerprint: false,
        conversationId: null,
        turns: stats.turns,
        turnIndex: stats.turns,
        context: {
          used,
          total,
          systemPromptTokens: stats.system,
          // Tool definitions are assembled per request and never persisted, so
          // they cannot be recovered from history.
          toolDefinitionsCount: 0,
          toolDefinitionsTokens: 0,
          compactionCount: stats.compactions,
          turnCount: stats.turns,
          toolCallCount: stats.toolCalls,
          messageCount: stats.messages,
          messageTokens: stats.body,
          freeTokens: Math.max(total - used, 0),
          usagePct: total > 0 ? Math.min(Math.round((used / total) * 100), 100) : 0,
          autoCompactThresholdPercent: 85,
        },
      },
    }
  })

  /**
   * `/compact` is a dedicated RPC, not a prompt: the pager sends
   * `x.ai/compact_conversation` and renders its own completed/failed banner from
   * the reply, so this must settle only after the kernel's compaction ended.
   *
   * Inbox admission only steers the work, so completion is read from the
   * durable compaction events rather than the admission response. Subscribe
   * first: a short conversation can finish before the admission returns.
   */
  const compact = Effect.fnUntraced(function* (params: CompactRequest) {
    const sessionID = yield* ACPClient.decodeSessionID(params.sessionId)
    yield* input.client.session.get({ sessionID }).pipe(Effect.catch(ACPClient.classify))
    // Mint the inbox id so the settle below can tell this compaction's events
    // apart from an automatic one that was already running.
    const id = SessionMessage.ID.create()
    /**
     * Only this admission's own events settle the request. `started` proves the
     * runner picked up this inbox item; `ended` carries no inbox id to compare,
     * so it counts only after that proof. An automatic compaction already in
     * flight therefore cannot settle a request that is still queued.
     */
    const settledBy = (event: CompactionEvent, started: boolean) => {
      if (event.type === "session.compaction.failed") return event.data.inputID === id
      if (event.type === "session.compaction.ended") return started && event.data.sessionID === sessionID
      return false
    }
    const isCompaction = (event: OpenCodeEvent): event is CompactionEvent =>
      event.type === "session.compaction.started" ||
      event.type === "session.compaction.ended" ||
      event.type === "session.compaction.failed"
    const settled = yield* Effect.scoped(
      Effect.gen(function* () {
        // The feed opens with `server.connected`; taking it proves the
        // subscription is live, so the admission below cannot finish unseen.
        const events = yield* input.client.event
          .subscribe()
          .pipe(Stream.toQueue({ capacity: "unbounded" }), Effect.catch(ACPClient.classify))
        const connected = yield* Queue.take(events)
        if (connected.type !== "server.connected")
          return yield* Effect.die(new Error(`expected server.connected, got ${connected.type}`))
        yield* input.client.session.compact({ sessionID, id }).pipe(Effect.catch(ACPClient.classify))
        let started = false
        while (true) {
          const event = yield* Queue.take(events).pipe(Effect.catch(ACPClient.classify))
          if (!isCompaction(event)) continue
          if (event.type === "session.compaction.started" && event.data.inputID === id) {
            started = true
            continue
          }
          if (settledBy(event, started)) return event
        }
      }),
    ).pipe(Effect.catch(ACPClient.classify), Effect.forkChild)
    const found = yield* Fiber.join(settled)
    if (found.type === "session.compaction.failed")
      // The pager routes on `data.kind` and renders `data.message`; a bare
      // internal error would show "Compaction failed" with no reason.
      return yield* Effect.fail(compactFailure(found.data.error))
    return {}
  })

  /**
   * `/rename` reaches the kernel only as this extension: the pager never sends
   * the standard `session/update`. An empty title with `resetToAuto` unpins the
   * title so the kernel regenerates it.
   */
  const rename = Effect.fnUntraced(function* (params: RenameRequest) {
    const sessionID = yield* ACPClient.decodeSessionID(params.sessionId)
    if (params.resetToAuto)
      yield* input.client.session
        .update({ sessionID, title: "" })
        .pipe(Effect.catch(ACPClient.classify))
    else if (params.title.trim())
      yield* input.client.session
        .update({ sessionID, title: params.title })
        .pipe(Effect.catch(ACPClient.classify))
    else
      return yield* Effect.fail(
        new ACPError.InvalidRequestError({ message: "title must not be blank", field: "title" }),
      )
    return {}
  })

  /**
   * `/delete` reaches the kernel only as this extension. `Session.remove` also
   * drops child sessions, which matches the pager asking to delete the tree.
   */
  const remove = Effect.fnUntraced(function* (params: DeleteRequest) {
    const sessionID = yield* ACPClient.decodeSessionID(params.sessionId)
    yield* input.client.session.remove({ sessionID }).pipe(Effect.catch(ACPClient.classify))
    return {}
  })

  /**
   * `/fork` reaches the kernel only as this extension. The kernel fork copies
   * settled history into a new top-level session, exactly what the pager's
   * placeholder then loads, so `newSessionId` is the only field it needs back.
   */
  const fork = Effect.fnUntraced(function* (params: ForkRequest) {
    const sessionID = yield* ACPClient.decodeSessionID(params.sessionId)
    const forked = yield* input.client.session.fork({ sessionID }).pipe(Effect.catch(ACPClient.classify))
    return { newSessionId: forked.id }
  })

  /**
   * `/usage` renders `{ usage: PromptUsage }` itself. The kernel keeps no
   * usage ledger, but every assistant step carries the tokens it billed and
   * its cost, so folding the durable log reproduces the same totals.
   */
  const usage = Effect.fnUntraced(function* (params: UsageRequest) {
    const sessionID = yield* ACPClient.decodeSessionID(params.sessionId)
    yield* input.client.session.get({ sessionID }).pipe(Effect.catch(ACPClient.classify))
    const folded = yield* messages(input.client, sessionID).pipe(Stream.runFold(emptyUsage, foldUsage))
    return {
      usage: {
        ...usageModel(folded.totals),
        // The pager keys per-model rows by "provider/model"; it only shows them
        // when more than one is present.
        modelUsage: Object.fromEntries([...folded.models].map(([ref, value]) => [ref, usageModel(value)])),
        numTurns: folded.totals.modelCalls,
      },
    }
  })

  /**
   * The usage modal also asks for billing so it can draw credits. The kernel
   * bills nothing, and every field of the response is optional, so answering
   * with an empty config keeps the tab honest instead of showing a method error.
   */
  const billing = Effect.fnUntraced(function* () {
    return { config: null }
  })

  /**
   * `/plugins` lists what the user actually installed for this session's
   * directory. Built-in and SDK modules ship with the kernel and cannot be
   * enabled, disabled, or removed, so listing them would fill the modal with
   * rows that have no action behind them.
   */
  const plugins = Effect.fnUntraced(function* (params: SurfaceListRequest) {
    const directory = yield* sessionDirectory(input.client, params.sessionId)
    const listed = yield* input.client.plugin
      .list({ location: { directory } })
      .pipe(Effect.catch(ACPClient.classify))
    return { plugins: listed.data.filter(isInstalledPlugin).map(pluginRow) }
  })

  /**
   * `/mcps` lists configured servers with the status the kernel reports. That
   * status is already live (`connected`/`pending`/`disabled`/`failed`/
   * `needs_auth`), so it maps straight onto the pager's row status.
   */
  const mcps = Effect.fnUntraced(function* (params: SurfaceListRequest) {
    const directory = yield* sessionDirectory(input.client, params.sessionId)
    const listed = yield* input.client.mcp.list({ location: { directory } }).pipe(Effect.catch(ACPClient.classify))
    return {
      servers: listed.data.map((server) => {
        const enabled = server.status.status !== "disabled"
        return {
          name: server.name,
          source: "local",
          enabled,
          session: { enabled, status: mcpStatus(server.status.status), tools: [] },
          // A failed server's reason is the one thing a user needs to fix it.
          ...(server.status.status === "failed" ? { sourceLabel: server.status.error } : {}),
          ...(server.title === undefined ? {} : { displayName: server.title }),
        }
      }),
      sessionMcpResolved: true,
    }
  })

  /**
   * Hooks are a grok concept: the kernel has no hook dispatcher to report, and
   * inventing rows would make the modal claim behaviour that cannot run. An
   * empty, trusted list is the honest answer.
   */
  const hooks = Effect.fnUntraced(function* (params: SurfaceListRequest) {
    yield* sessionDirectory(input.client, params.sessionId)
    return { hooks: [], projectTrusted: true, loadErrors: [] }
  })

  /**
   * The modals offer mutating actions the kernel does not implement. Answering
   * with an explicit `unsupported` outcome keeps the reason in the modal instead
   * of surfacing a transport-level method-not-found.
   */
  const unsupportedAction = (what: string) => ({
    status: "unsupported" as const,
    message: `${what} is not available in this kernel`,
    requiresReload: false,
    requiresRestart: false,
  })

  /**
   * `/rewind` lists every user prompt that can still be rewound to. The pager
   * counts prompts itself, so the index here must be the ordinal among this
   * session's own user messages — the same order the scrollback numbers them.
   */
  const rewindPoints = Effect.fnUntraced(function* (params: RewindPointsRequest) {
    const sessionID = yield* ACPClient.decodeSessionID(params.sessionId)
    yield* input.client.session.get({ sessionID }).pipe(Effect.catch(ACPClient.classify))
    const prompts = yield* messages(input.client, sessionID).pipe(
      Stream.filter((message) => message.type === "user"),
      Stream.map((message) => ({
        messageID: message.id,
        text: message.type === "user" ? message.text : "",
        createdAt: message.time.created,
      })),
      Stream.runCollect,
    )
    return {
      rewindPoints: Array.from(prompts).map((prompt, index) => ({
        promptIndex: index,
        createdAt: DateTime.formatIso(prompt.createdAt),
        // Rewind is conversation-only, so no prompt advertises a file snapshot
        // the pager could otherwise offer to restore.
        numFileSnapshots: 0,
        hasFileChanges: false,
        promptPreview: preview(prompt.text),
      })),
    }
  })

  /**
   * Rewinding to prompt N keeps prompts `0..N-1`. The kernel's revert boundary
   * deletes everything from the boundary message onward, so the boundary is the
   * Nth user message, and committing it both truncates history and clears the
   * staged revert that would otherwise fire on the next prompt.
   */
  const rewindExecute = Effect.fnUntraced(function* (params: RewindExecuteRequest) {
    const sessionID = yield* ACPClient.decodeSessionID(params.sessionId)
    if (params.targetPromptIndex < 0)
      return yield* Effect.fail(
        new ACPError.InvalidRequestError({ message: "targetPromptIndex is required", field: "targetPromptIndex" }),
      )
    // The pager only ever asks for conversation-only rewinds. Files-only has no
    // boundary-free equivalent here, so refuse it instead of quietly truncating
    // history as a side effect of restoring files.
    if (params.mode === "files_only")
      return failure(params.targetPromptIndex, "files-only rewind is not supported")
    const prompts = yield* messages(input.client, sessionID).pipe(
      Stream.filter((message) => message.type === "user"),
      Stream.map((message) => ({
        messageID: message.id,
        text: message.type === "user" ? message.text : "",
      })),
      Stream.runCollect,
    )
    const target = Array.from(prompts)[params.targetPromptIndex]
    if (target === undefined) return failure(params.targetPromptIndex, `no prompt at index ${params.targetPromptIndex}`)
    const revert = yield* input.client.session.revert
      .stage({
        sessionID,
        messageID: target.messageID,
        files: params.mode !== "conversation_only",
      })
      .pipe(Effect.catch(ACPClient.classify))
    // `commit` publishes the durable event that truncates; a failure here leaves
    // the revert staged, which the next prompt would apply, so drop it instead.
    yield* input.client.session.revert.commit({ sessionID }).pipe(
      Effect.tapError(() => input.client.session.revert.clear({ sessionID }).pipe(Effect.orElseSucceed(() => {}))),
      Effect.catch(ACPClient.classify),
    )
    const files = params.mode === "conversation_only" ? [] : (revert.files ?? []).map((file) => file.file)
    return {
      success: true,
      targetPromptIndex: params.targetPromptIndex,
      mode: params.mode,
      revertedFiles: files,
      cleanFiles: files,
      conflicts: [],
      promptText: target.text,
    }
  })
  const contextWindow = Effect.fnUntraced(function* (session: Session.Info, ref: string | undefined) {    if (ref === undefined) return 0
    const windowAt = (directory: string) =>
      input.client.model.list({ location: { directory } }).pipe(
        Effect.catch(ACPClient.classify),
        Effect.orElseSucceed(() => undefined),
        Effect.map((listed) => listed?.data.find((model) => `${model.providerID}/${model.id}` === ref)?.limit.context),
      )
    // A session's own directory is authoritative, but a profile may have been
    // pointed at a provider configured elsewhere; the kernel process cwd is the
    // second place a catalogue is resolved from.
    const local = yield* windowAt(session.location.directory)
    if (local !== undefined) return local
    return (yield* windowAt(process.cwd())) ?? 0
  })

  return {
    list,
    info,
    compact,
    rename,
    remove,
    fork,
    usage,
    billing,
    rewindPoints,
    rewindExecute,
    plugins,
    mcps,
    hooks,
    unsupportedAction,
  }
}

/** The session's own directory resolves plugins and MCP config the same way a prompt does. */
const sessionDirectory = (client: OpenCodeClient, sessionId: string) =>
  Effect.gen(function* () {
    const sessionID = yield* ACPClient.decodeSessionID(sessionId)
    const session = yield* client.session.get({ sessionID }).pipe(Effect.catch(ACPClient.classify))
    return session.location.directory
  })

/**
 * The pager expects grok's plugin row, which carries far more than the kernel
 * knows. Only the fields with a real source are filled; the rest are the
 * documented "nothing to show" values rather than plausible-looking defaults.
 */
const pluginRow = (plugin: Plugin.Info) => ({
  name: pluginName(plugin),
  id: plugin.id ?? pluginName(plugin),
  root: plugin.source.type === "local" ? plugin.source.path : "",
  scope: pluginScope(plugin),
  trusted: true,
  enabled: plugin.state.status === "active",
  skillCount: 0,
  agentCount: 0,
  hookStatus: "none",
  hookCount: 0,
  mcpServerCount: 0,
  mcpStatus: "none",
  ...(plugin.source.type === "package" && plugin.source.version ? { version: plugin.source.version } : {}),
  ...(plugin.state.status === "failed" ? { conflict: plugin.state.error } : {}),
})

/** Only plugins a user could manage belong in the modal. */
const isInstalledPlugin = (plugin: Plugin.Info) => plugin.source.type === "package" || plugin.source.type === "local"

const pluginName = (plugin: Plugin.Info) => {
  if (plugin.source.type === "local") return plugin.source.path.split(/[\\/]/).filter(Boolean).pop() ?? plugin.source.path
  if (plugin.source.type === "package") return plugin.source.target.split(/[\\/]/).filter(Boolean).pop() ?? plugin.source.target
  return plugin.id ?? plugin.source.type
}

/** `builtin`/`sdk` plugins ship with the kernel, so they read as config scope. */
const pluginScope = (plugin: Plugin.Info): "cli" | "project" | "user" | "config" => {
  if (plugin.source.type === "local") return "project"
  if (plugin.source.type === "package") return "user"
  return "config"
}

const mcpStatus = (status: string) => {
  if (status === "connected") return "ready"
  if (status === "pending") return "initializing"
  return "unavailable"
}

/** One line of a prompt, which is all the rewind picker can show per row. */
const preview = (text: string) => {
  const line = text.trim().split("\n")[0] ?? ""
  return line.length > 100 ? `${line.slice(0, 99)}…` : line
}

/**
 * A failed rewind is reported in the response rather than as an RPC error: the
 * pager renders `error` inside its rewind panel, and a transport error there
 * would replace the whole picker with a bare failure toast.
 */
const failure = (targetPromptIndex: number, error: string) => ({
  success: false,
  targetPromptIndex,
  mode: "conversation_only" as const,
  revertedFiles: [] as string[],
  cleanFiles: [] as string[],
  conflicts: [] as Array<{ path: string; conflictType: string }>,
  error,
})

/** Running usage totals for one model row, in the units the pager's wire uses. */
type Usage = {
  readonly input: number
  readonly output: number
  readonly cachedRead: number
  readonly cacheCreation: number
  readonly reasoning: number
  readonly modelCalls: number
  readonly apiDurationMs: number
  readonly ticks: number
  readonly missingCost: number
}

type UsageFold = { readonly totals: Usage; readonly models: Map<string, Usage> }

const zeroUsage = (): Usage => ({
  input: 0,
  output: 0,
  cachedRead: 0,
  cacheCreation: 0,
  reasoning: 0,
  modelCalls: 0,
  apiDurationMs: 0,
  ticks: 0,
  missingCost: 0,
})

const emptyUsage = (): UsageFold => ({ totals: zeroUsage(), models: new Map() })

/**
 * A step's `input` is the full prompt including cache reads, matching the ACP
 * identity the pager prints. A step with tokens but no recorded cost counts as
 * a missing cost, so the row is flagged partial instead of silently summed.
 */
const foldUsage = (folded: UsageFold, message: SessionMessage.Info): UsageFold => {
  if (message.type !== "assistant" || message.tokens === undefined) return folded
  const step: Usage = {
    input: message.tokens.input + message.tokens.cache.read + message.tokens.cache.write,
    output: message.tokens.output,
    cachedRead: message.tokens.cache.read,
    cacheCreation: message.tokens.cache.write,
    reasoning: message.tokens.reasoning,
    modelCalls: 1,
    apiDurationMs:
      message.time.streamed === undefined || message.time.completed === undefined
        ? 0
        : Math.max(0, DateTime.toEpochMillis(message.time.completed) - DateTime.toEpochMillis(message.time.streamed)),
    ticks: message.cost === undefined ? 0 : Math.round(message.cost * USD_TICKS_PER_USD),
    missingCost: message.cost === undefined ? 1 : 0,
  }
  const add = (left: Usage, right: Usage): Usage => ({
    input: left.input + right.input,
    output: left.output + right.output,
    cachedRead: left.cachedRead + right.cachedRead,
    cacheCreation: left.cacheCreation + right.cacheCreation,
    reasoning: left.reasoning + right.reasoning,
    modelCalls: left.modelCalls + right.modelCalls,
    apiDurationMs: left.apiDurationMs + right.apiDurationMs,
    ticks: left.ticks + right.ticks,
    missingCost: left.missingCost + right.missingCost,
  })
  const ref = `${message.model.providerID}/${message.model.id}`
  return {
    totals: add(folded.totals, step),
    models: folded.models.set(ref, add(folded.models.get(ref) ?? zeroUsage(), step)),
  }
}

/**
 * `costUsdTicks` is omitted rather than zeroed when nothing billed, because the
 * pager renders "Cost: $" from the field's presence.
 */
const usageModel = (usage: Usage) => ({
  inputTokens: usage.input,
  outputTokens: usage.output,
  totalTokens: usage.input + usage.output,
  cachedReadTokens: usage.cachedRead,
  cacheCreationTokens: usage.cacheCreation,
  reasoningTokens: usage.reasoning,
  modelCalls: usage.modelCalls,
  apiDurationMs: usage.apiDurationMs,
  ...(usage.ticks > 0 ? { costUsdTicks: usage.ticks } : {}),
  ...(usage.missingCost > 0 ? { costIsPartial: true } : {}),
})

/** Pages the durable log the same way `session/load` replays it, without holding the whole history twice. */
const messages = (client: OpenCodeClient, sessionID: Session.ID) =>
  Stream.paginate(undefined as string | undefined, (cursor) =>
    (cursor === undefined
      ? client.message.list({ sessionID, limit: PAGE_SIZE, order: "asc" })
      : client.message.list({ sessionID, limit: PAGE_SIZE, cursor })
    ).pipe(
      Effect.catch(ACPClient.classify),
      Effect.map((page) => [page.data, Option.fromNullishOr(page.cursor.next)] as const),
    ),
  )

type Stats = {
  readonly messages: number
  readonly turns: number
  readonly compactions: number
  readonly toolCalls: number
  readonly system: number
  readonly body: number
  readonly estimated: number
  readonly prompt: number
}

const emptyStats = (): Stats => ({
  messages: 0,
  turns: 0,
  compactions: 0,
  toolCalls: 0,
  system: 0,
  body: 0,
  estimated: 0,
  prompt: 0,
})

const estimate = (text: string) => Math.ceil(text.length / 4)

const content = (message: SessionMessage.Info) => {
  if (message.type === "assistant")
    return message.content.flatMap((part) => (part.type === "tool" ? [] : "text" in part ? [part.text] : []))
  return "text" in message ? [message.text] : []
}

const fold = (stats: Stats, message: SessionMessage.Info): Stats => {
  const counted = {
    ...stats,
    messages: stats.messages + 1,
    turns: message.type === "idle" ? stats.turns + 1 : stats.turns,
    compactions: message.type === "compaction" ? stats.compactions + 1 : stats.compactions,
    toolCalls: message.type === "assistant"
      ? stats.toolCalls + message.content.filter((part) => part.type === "tool").length
      : stats.toolCalls,
  }
  const size = content(message).reduce((total, text) => total + estimate(text), 0)
  if (message.type === "system") return { ...counted, system: counted.system + size, estimated: counted.estimated + size }
  const prompt = message.type === "assistant" && message.tokens !== undefined
    ? message.tokens.input + message.tokens.cache.read + message.tokens.cache.write
    : stats.prompt
  return { ...counted, body: counted.body + size, estimated: counted.estimated + size, prompt }
}

export * as ACPSessionExt from "./session-ext"
