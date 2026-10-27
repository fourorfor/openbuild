import { EOL } from "node:os"
import { Cause, Effect, Exit } from "effect"
import { OpenCode } from "@opencode/client"
import { Service } from "@opencode/client/effect/service"
import { Npm } from "@opencode/util/npm"
import { Config } from "../../../config"
import { ServerConnection } from "../../../services/server-connection"

export interface Item {
  readonly runtime: "Server" | "TUI"
  readonly target: string
  readonly name: string
  readonly version?: string
  readonly outdated: boolean
  readonly error?: string
}

export const inspect = Effect.fn("cli.plugin.inspect")(function* (selected?: string) {
  const { endpoint } = yield* ServerConnection.resolve()
  const client = OpenCode.make({ baseUrl: endpoint.url, headers: Service.headers(endpoint) })
  const location = { directory: process.cwd() }
  // A cold server activates plugins after it starts serving. `plugin check`
  // must not report "none" while the plugin supervisor is still loading, or a
  // failed local plugin would look identical to an empty config.
  const listed = yield* Effect.promise(async () => {
    const deadline = Date.now() + 2_000
    let response = await client.plugin.list({ location })
    while (response.data.length === 0 && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 100))
      response = await client.plugin.list({ location })
    }
    return response
  })
  const serverTargets = new Set(
    listed.data.flatMap((plugin) => (plugin.source.type === "package" ? [plugin.source.target] : [])),
  )
  const server =
    selected === undefined || serverTargets.has(selected)
      ? yield* Effect.promise(() => client.plugin.check({ location, ...(selected ? { target: selected } : {}) }))
      : listed
  const serverItems = server.data.flatMap((plugin): Item[] => {
    if (plugin.source.type === "builtin") return []
    const target =
      plugin.source.type === "package"
        ? plugin.source.target
        : plugin.source.type === "local"
          ? plugin.source.path
          : plugin.source.type
    if (selected !== undefined && target !== selected && plugin.id !== selected) return []
    return [
      {
        runtime: "Server",
        target,
        name: plugin.id ?? target,
        ...(plugin.source.type === "package" && plugin.source.version
          ? { version: displayVersion(plugin.source.version) }
          : {}),
        outdated: plugin.source.type === "package" && plugin.source.outdated === true,
        ...(plugin.state.status === "failed" ? { error: plugin.state.error } : {}),
      },
    ]
  })

  const config = yield* Config.Service
  const info = yield* config.get()
  const configured = [
    ...new Set(
      (info.plugins ?? []).flatMap((entry) => {
        const target = typeof entry === "string" ? entry : entry.package
        if (target.startsWith("-") || target === "*" || target.endsWith(".*") || target.startsWith("opencode.")) return []
        return [target]
      }),
    ),
  ]
  const tuiTargets = yield* Effect.promise(async () => {
    const installable = await Promise.all(configured.map((target) => Npm.isInstallablePackage(target)))
    return configured.filter((target, index) => installable[index] && (selected === undefined || target === selected))
  })
  const npm = yield* Npm.Service
  const tuiItems = yield* Effect.forEach(
    tuiTargets,
    (target) =>
      Effect.gen(function* () {
        const installed = yield* npm.resolve(target)
        const outdated = yield* npm.check(target).pipe(Effect.exit)
        return {
          runtime: "TUI" as const,
          target,
          name: target,
          ...(installed.version ? { version: displayVersion(installed.version) } : {}),
          outdated: Exit.isSuccess(outdated) && outdated.value,
          ...(Exit.isFailure(outdated) ? { error: Cause.pretty(outdated.cause) } : {}),
        }
      }),
    { concurrency: "unbounded" },
  )
  const items = [...serverItems, ...tuiItems]
  if (selected !== undefined && !items.length) return yield* Effect.fail(new Error(`Plugin is not configured: ${selected}`))
  return { client, location, items }
})

export function displayVersion(version: string) {
  return /^(?:[a-f0-9]{40}|[a-f0-9]{64})$/i.test(version) ? version.slice(0, 7) : version
}

export function format(items: readonly Item[]) {
  return ["Server", "TUI"]
    .flatMap((runtime) => {
      const rows = items.filter((item) => item.runtime === runtime)
      if (!rows.length) return []
      return [
        runtime,
        ...rows.map(
          (item) =>
            `  ${item.name}${item.version ? ` ${item.version}` : ""} (${item.error ? "check failed" : item.outdated ? "update available" : "current"})`,
        ),
      ]
    })
    .join(EOL)
}
