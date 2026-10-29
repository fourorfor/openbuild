import type { OpenCodeClient } from "@opencode/client/effect"
import { Session } from "@opencode/schema/session"
import type { SessionMessage } from "@opencode/schema/session-message"
import { displayLabel, withTimestampedFallback } from "@opencode/util/session-title-fallback"
import { DateTime, Effect, Option, Schema, Stream } from "effect"
import { ACPClient } from "./client"
import { ACPDirectories } from "./directories"

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

const decodeList = Schema.decodeUnknownOption(ListParams)
const decodeInfo = Schema.decodeUnknownOption(InfoParams)

const DEFAULT_LIMIT = 30
const MAX_LIMIT = 100
const PAGE_SIZE = 200

/** Unknown shapes fall back to the documented defaults (no cwd, no query), which list everything. */
export type ListRequest = typeof ListParams.Type
export type InfoRequest = typeof InfoParams.Type

export const parseListParams = (params: unknown): ListRequest =>
  Option.getOrElse(decodeList(params ?? {}), () => ({}))

export const parseInfoParams = (params: unknown): InfoRequest =>
  Option.getOrElse(decodeInfo(params ?? {}), () => ({ sessionId: "" }))

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

  const contextWindow = Effect.fnUntraced(function* (session: Session.Info, ref: string | undefined) {
    if (ref === undefined) return 0
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

  return { list, info }
}

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
