import {
  agent,
  RequestError,
  type AgentHandlerContext,
  type AgentNotificationHandlersByMethod,
  type AgentNotificationMethod,
  type AgentRequestHandlersByMethod,
  type AgentRequestMethod,
  type JsonRpcId,
  type Stream,
} from "@agentclientprotocol/sdk"
import type { OpenCodeClient } from "@opencode/client/effect"
import { Cause, Deferred, Effect, Ref, type Scope } from "effect"
import { ACPCapabilities } from "./capabilities"
import { ACPCatalog } from "./catalog"
import { ACPClient } from "./client"
import { ACPConnection } from "./connection"
import { ACPError } from "./error"
import { ACPService } from "./service"
import { ACPSessionExt } from "./session-ext"
import { ACPSessions } from "./sessions"
import { ACPTurn } from "./turn"

type HandlerContext<Params> = AgentHandlerContext<Params> & { readonly requestId?: JsonRpcId }

// Untraced so request spans parent to the caller's span instead of a setup span that has already ended.
export const connect = Effect.fnUntraced(function* (
  client: OpenCodeClient,
  stream: Stream,
  server?: { readonly url: string; readonly password: string },
) {
  const run = Effect.runPromiseWith(yield* Effect.context<Scope.Scope>())
  const catalog = yield* ACPCatalog.make(client)
  // Requests can dispatch before the service below is built.
  const ready = yield* Deferred.make<{
    readonly service: ACPService.Interface
    readonly connection: ACPConnection.Interface
  }>()
  const handle =
    <Params, A>(
      call: (service: ACPService.Interface, ctx: AgentHandlerContext<Params>) => Effect.Effect<A, ACPError.Failure>,
    ) =>
    (name: string) => {
      const handler = Effect.fn(name)(
        function* (ctx: HandlerContext<Params>) {
          const connected = yield* Deferred.await(ready)
          if (ctx.requestId === undefined) return yield* call(connected.service, ctx)
          return yield* call(connected.service, ctx).pipe(
            Effect.provideService(ACPConnection.Responded, connected.connection.responded(ctx.requestId)),
          )
        },
        Effect.catchTags({
          ACPCatalogLoadError: (error) => ACPClient.classify(error.cause),
          ACPCatalogNotReadyError: (error) =>
            Effect.fail(new ACPError.ServiceFailureError({ safeMessage: error.message, errorName: "CatalogNotReady" })),
        }),
        Effect.mapError((error) => (error instanceof RequestError ? error : ACPError.toRequestError(error))),
        Effect.tapCauseIf(Cause.hasDies, (cause) => Effect.logError("ACP request failed", cause)),
        Effect.catchDefect((defect) => Effect.fail(ACPError.toRequestError(ACPError.fromUnknown(defect)))),
      )
      return (ctx: HandlerContext<Params>) => run(handler(ctx))
    }
  const app = agent({ name: "opencode" })
  const request = <Method extends AgentRequestMethod>(
    method: Method,
    make: (name: string) => AgentRequestHandlersByMethod[Method],
  ) => app.onRequest(method, make(spanName(method)))
  const notification = <Method extends AgentNotificationMethod>(
    method: Method,
    make: (name: string) => AgentNotificationHandlersByMethod[Method],
  ) => app.onNotification(method, make(spanName(method)))

  request(
    "initialize",
    handle((service, ctx) => service.initialize(ctx.params)),
  )
  request(
    "authenticate",
    handle((service, ctx) => service.authenticate(ctx.params)),
  )
  request(
    "session/new",
    handle((service, ctx) => service.newSession(ctx.params)),
  )
  request(
    "session/load",
    handle((service, ctx) => service.loadSession(ctx.params)),
  )
  request(
    "session/list",
    handle((service, ctx) => service.listSessions(ctx.params)),
  )
  request(
    "session/delete",
    handle((service, ctx) => service.deleteSession(ctx.params)),
  )
  request(
    "session/resume",
    handle((service, ctx) => service.resumeSession(ctx.params)),
  )
  request(
    "session/close",
    handle((service, ctx) => service.closeSession(ctx.params)),
  )
  request(
    "session/fork",
    handle((service, ctx) => service.forkSession(ctx.params)),
  )
  request(
    "session/set_config_option",
    handle((service, ctx) => service.setSessionConfigOption(ctx.params)),
  )
  request(
    "session/set_mode",
    handle((service, ctx) => service.setSessionMode(ctx.params)),
  )
  request(
    "session/prompt",
    handle((service, ctx) => service.prompt(ctx.params, ctx.signal)),
  )
  notification(
    "session/cancel",
    handle((service, ctx) => service.cancel(ctx.params)),
  )
  // The grok-derived TUI asks for session history and a context breakdown
  // through `x.ai/*`. ACP types no custom methods, so each one declares its
  // own params parser; `handle` still gives them the same error mapping.
  //
  // The Rust ACP client encodes every custom method with a leading `_`
  // (`_x.ai/session/list`) and strips it again when decoding replies; this
  // SDK dispatches on the literal method name, so both spellings must be
  // registered or the TUI's `/resume` and `/context` see method-not-found.
  const ext = ACPSessionExt.make({ client })
  const listHandler = handle((_service, ctx: HandlerContext<ACPSessionExt.ListRequest>) => ext.list(ctx.params))
  const infoHandler = handle((_service, ctx: HandlerContext<ACPSessionExt.InfoRequest>) => ext.info(ctx.params))
  // `/compact` is its own RPC in the grok-derived TUI; without it the pager's
  // `/compact` reports "Compaction failed" from a method-not-found.
  const compactHandler = handle(
    (_service, ctx: HandlerContext<ACPSessionExt.CompactRequest>) => ext.compact(ctx.params),
  )
  // `/rename`, `/delete`, and `/fork` are likewise extension-only: the pager
  // never sends the standard `session/update`, `session/delete`, or
  // `session/fork` requests, so each one failed as method-not-found.
  const renameHandler = handle(
    (_service, ctx: HandlerContext<ACPSessionExt.RenameRequest>) => ext.rename(ctx.params),
  )
  const removeHandler = handle(
    (_service, ctx: HandlerContext<ACPSessionExt.DeleteRequest>) => ext.remove(ctx.params),
  )
  const forkHandler = handle((_service, ctx: HandlerContext<ACPSessionExt.ForkRequest>) => ext.fork(ctx.params))
  // `/usage` is the last extension-only command in daily use: its modal reads
  // `x.ai/session/usage` and shows "not supported" when the method is absent.
  const usageHandler = handle((_service, ctx: HandlerContext<ACPSessionExt.UsageRequest>) => ext.usage(ctx.params))
  // The usage modal also asks for billing; answering "no config" avoids a
  // method-not-found banner on a tab the kernel genuinely has nothing for.
  const billingHandler = handle(() => ext.billing())
  // `/rewind` needs both halves: the picker asks for the points, then executing
  // one truncates history at the chosen prompt.
  const rewindPointsHandler = handle((_service, ctx: HandlerContext<ACPSessionExt.RewindPointsRequest>) =>
    ext.rewindPoints(ctx.params),
  )
  const rewindExecuteHandler = handle((_service, ctx: HandlerContext<ACPSessionExt.RewindExecuteRequest>) =>
    ext.rewindExecute(ctx.params),
  )
  // `/plugins`, `/mcps`, and `/hooks` are three tabs' worth of read-only state.
  const pluginsHandler = handle((_service, ctx: HandlerContext<ACPSessionExt.SurfaceListRequest>) =>
    ext.plugins(ctx.params),
  )
  const mcpsHandler = handle((_service, ctx: HandlerContext<ACPSessionExt.SurfaceListRequest>) => ext.mcps(ctx.params))
  const hooksHandler = handle((_service, ctx: HandlerContext<ACPSessionExt.SurfaceListRequest>) =>
    ext.hooks(ctx.params),
  )
  // Their mutating verbs have no kernel equivalent; the pager renders the
  // returned outcome inline, so these stay explicit rather than method-not-found.
  const pluginsActionHandler = handle(() => Effect.succeed(ext.unsupportedAction("plugin management")))
  const hooksActionHandler = handle(() => Effect.succeed(ext.unsupportedAction("hook management")))
  const mcpUpsertHandler = handle(() => Effect.succeed(ext.unsupportedAction("MCP server editing")))
  for (const method of ["x.ai/session/list", "_x.ai/session/list"]) {
    app.onRequest(method, { parse: ACPSessionExt.parseListParams }, listHandler(spanName(method)))
  }
  for (const method of ["x.ai/session/info", "_x.ai/session/info"]) {
    app.onRequest(method, { parse: ACPSessionExt.parseInfoParams }, infoHandler(spanName(method)))
  }
  for (const method of ["x.ai/compact_conversation", "_x.ai/compact_conversation"]) {
    app.onRequest(method, { parse: ACPSessionExt.parseCompactParams }, compactHandler(spanName(method)))
  }
  for (const method of ["x.ai/session/rename", "_x.ai/session/rename"]) {
    app.onRequest(method, { parse: ACPSessionExt.parseRenameParams }, renameHandler(spanName(method)))
  }
  for (const method of ["x.ai/session/delete", "_x.ai/session/delete"]) {
    app.onRequest(method, { parse: ACPSessionExt.parseDeleteParams }, removeHandler(spanName(method)))
  }
  for (const method of ["x.ai/session/fork", "_x.ai/session/fork"]) {
    app.onRequest(method, { parse: ACPSessionExt.parseForkParams }, forkHandler(spanName(method)))
  }
  for (const method of ["x.ai/session/usage", "_x.ai/session/usage"]) {
    app.onRequest(method, { parse: ACPSessionExt.parseUsageParams }, usageHandler(spanName(method)))
  }
  for (const method of ["x.ai/billing", "_x.ai/billing"]) {
    app.onRequest(method, { parse: () => ({}) }, billingHandler(spanName(method)))
  }
  for (const method of ["x.ai/rewind/points", "_x.ai/rewind/points"]) {
    app.onRequest(method, { parse: ACPSessionExt.parseRewindPointsParams }, rewindPointsHandler(spanName(method)))
  }
  for (const method of ["x.ai/rewind/execute", "_x.ai/rewind/execute"]) {
    app.onRequest(method, { parse: ACPSessionExt.parseRewindExecuteParams }, rewindExecuteHandler(spanName(method)))
  }
  for (const method of ["x.ai/plugins/list", "_x.ai/plugins/list"]) {
    app.onRequest(method, { parse: ACPSessionExt.parseSurfaceParams }, pluginsHandler(spanName(method)))
  }
  for (const method of ["x.ai/mcp/list", "_x.ai/mcp/list"]) {
    app.onRequest(method, { parse: ACPSessionExt.parseSurfaceParams }, mcpsHandler(spanName(method)))
  }
  for (const method of ["x.ai/hooks/list", "_x.ai/hooks/list"]) {
    app.onRequest(method, { parse: ACPSessionExt.parseSurfaceParams }, hooksHandler(spanName(method)))
  }
  for (const method of ["x.ai/plugins/action", "_x.ai/plugins/action"]) {
    app.onRequest(method, { parse: ACPSessionExt.parseSurfaceParams }, pluginsActionHandler(spanName(method)))
  }
  for (const method of ["x.ai/hooks/action", "_x.ai/hooks/action"]) {
    app.onRequest(method, { parse: ACPSessionExt.parseSurfaceParams }, hooksActionHandler(spanName(method)))
  }
  for (const method of ["x.ai/mcp/upsert", "_x.ai/mcp/upsert"]) {
    app.onRequest(method, { parse: ACPSessionExt.parseSurfaceParams }, mcpUpsertHandler(spanName(method)))
  }
  const acp = ACPConnection.make(app, stream)
  const connection = acp.connection
  const sessions = yield* ACPSessions.make({ client, connection, catalog })
  const capabilities = yield* Ref.make(ACPCapabilities.parse(undefined))
  const turn = yield* ACPTurn.make({ client, connection, sessions, catalog, capabilities })
  yield* Deferred.succeed(ready, {
    service: ACPService.make({ client, connection, catalog, sessions, capabilities, turn, server }),
    connection,
  })
  return acp.agent
})

const spanName = (method: string) => `cli.acp.${method.replaceAll("/", ".")}`

export * as ACP from "./agent"
