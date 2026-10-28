/**
 * Proxy routing for outbound HTTP requests.
 *
 * Bun's `fetch` honours `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY`, but a proxy
 * that refuses connections makes every request fail: there is no fallback. Bun
 * also ignores the Windows system proxy, so a kernel started from Explorer or a
 * service never sees the proxy the user configured.
 *
 * OpenBuild resolves the proxy in the TUI, applies reachability and precedence
 * there, and passes the answer as `OPENBUILD_PROXY`. This module turns it into
 * behaviour: requests go through the proxy when one is configured, a transport
 * failure retries the same request directly, and the proxy is skipped for a
 * cool-down so a dead proxy does not double the latency of every request.
 *
 * Bun reads the proxy environment synchronously at `fetch()` call time, and a
 * per-request `proxy` option wins over it. Direct requests therefore run inside
 * a delete/restore window that lasts only until `fetch` returns its promise, so
 * concurrent requests and child processes still observe the original variables.
 */

/** Names Bun consults, in precedence order. */
const PROXY_VARS = ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]

/** Explicit decision handed over by the TUI. */
export const PROXY_ENV = "OPENBUILD_PROXY"

/** Values of `OPENBUILD_PROXY` that mean "never use a proxy". */
const DIRECT_SWITCHES = new Set(["off", "none", "direct", "false", "0"])

/** How long a failed proxy is skipped before it is tried again. */
const RETRY_AFTER_MS = 60_000

/** Transport failures that mean the proxy could not be reached. */
const TRANSPORT_MARKERS = [
  "ECONNREFUSED",
  "ECONNRESET",
  "ECONNABORTED",
  "ENOTFOUND",
  "EHOSTUNREACH",
  "ENETUNREACH",
  "ETIMEDOUT",
  "UND_ERR",
  "EPIPE",
  "EPROTO",
  "UNABLE TO CONNECT",
  "FAILED TO CONNECT",
  "CONNECTION CLOSED",
]

export type ProxyConfig =
  | { mode: "direct"; source: string }
  | { mode: "proxy"; url: string; source: string }

export interface ProxyState {
  config: ProxyConfig
  /** Transport failures observed through the proxy. */
  failures: number
  /** Epoch milliseconds of the most recent cool-down deadline, for diagnostics. */
  directUntil: number
  /**
   * Per-host cool-down deadlines.
   *
   * The cool-down must not be global: the kernel also probes local servers (an
   * Ollama on 11434, an LM Studio on 1234). Those probes legitimately fail, and
   * while the deadline was process-wide they pushed every later request —
   * including model traffic — onto the direct path for a minute. On a machine
   * whose direct route does not answer, that turned a single failed local probe
   * into a request that never returns.
   */
  coolDown: Map<string, number>
}

/** Give a bare `host:port` an `http://` scheme; keep any scheme as given. */
export function normalise(value: string): string {
  const trimmed = value.trim().replace(/\/+$/, "")
  return trimmed.includes("://") ? trimmed : `http://${trimmed}`
}

function isDirectSwitch(value: string): boolean {
  return DIRECT_SWITCHES.has(value.trim().toLowerCase())
}

/**
 * Decide how this process should reach the network.
 *
 * `OPENBUILD_PROXY` is decisive because the TUI already applied precedence and
 * reachability. The ambient variables are the fallback for a kernel started by
 * hand, which is the pre-existing behaviour.
 */
export function readConfig(env: Record<string, string | undefined>): ProxyConfig {
  const explicit = env[PROXY_ENV]?.trim()
  if (explicit) {
    if (isDirectSwitch(explicit)) return { mode: "direct", source: PROXY_ENV }
    return { mode: "proxy", url: normalise(explicit), source: PROXY_ENV }
  }
  const ambient = PROXY_VARS.map((name) => [name, env[name]?.trim()] as const).find(
    ([, value]) => value && !isDirectSwitch(value),
  )
  if (ambient) return { mode: "proxy", url: normalise(ambient[1]!), source: ambient[0] }
  return { mode: "direct", source: "none" }
}

/**
 * Is this a failure to reach the proxy rather than an answer from the provider?
 *
 * Only transport failures are retried. An HTTP 4xx/5xx is a real response and
 * must reach the caller unchanged.
 */
export function isTransportFailure(error: unknown): boolean {
  if (!(error instanceof Error)) return false
  if (error.name === "AbortError") return false
  const text = `${(error as { code?: string }).code ?? ""} ${error.message}`.toUpperCase()
  return TRANSPORT_MARKERS.some((marker) => text.includes(marker))
}

/** Run `start` with the proxy variables removed, restoring them immediately. */
function withoutProxyEnv<T>(env: Record<string, string | undefined>, start: () => T): T {
  const saved = PROXY_VARS.map((name) => [name, env[name]] as const)
  saved.forEach(([name]) => delete env[name])
  try {
    return start()
  } finally {
    saved.forEach(([name, value]) => {
      if (value !== undefined) env[name] = value
    })
  }
}

export interface RouteOptions {
  env?: Record<string, string | undefined>
  transport?: typeof globalThis.fetch
  now?: () => number
  /** Deadline for the proxy to produce response headers. */
  headerDeadlineMs?: number
}

export interface Router {
  fetch: typeof globalThis.fetch
  state: ProxyState
}

/**
 * Build the proxy-aware fetch function.
 *
 * Separate from `install` so the routing rules are exercised without replacing
 * the process-global `fetch`.
 */
export function route(options: RouteOptions = {}): Router {
  const env = options.env ?? process.env
  const transport = options.transport ?? globalThis.fetch.bind(globalThis)
  const now = options.now ?? Date.now
  const state: ProxyState = {
    config: readConfig(env),
    failures: 0,
    directUntil: 0,
    coolDown: new Map(),
  }
  const note = tracer(env)
  const headerDeadlineMs = options.headerDeadlineMs ?? HEADER_DEADLINE_MS

  const direct = (input: RequestInfo | URL, init?: RequestInit) =>
    withoutProxyEnv(env, () => transport(input, init))

  const fetch = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    const target = targetOf(input)
    if (state.config.mode === "direct") {
      note(`direct ${target}`)
      return direct(input, init)
    }
    // A loopback or private target cannot be reached through an outbound proxy.
    if (state.config.mode === "proxy" && isLocalTarget(target)) {
      note(`direct ${target} (local target, proxy skipped)`)
      return direct(input, init)
    }
    const cooldownUntil = state.coolDown.get(target) ?? 0
    if (now() < cooldownUntil) {
      note(`direct ${target} (cool-down for this host after a proxy failure)`)
      return direct(input, init)
    }
    try {
      note(`proxy ${state.config.url} -> ${target}`)
      const response = await withHeaderDeadline(
        transport(input, { ...init, proxy: state.config.url } as RequestInit),
        headerDeadlineMs,
      )
      note(`proxy answered ${response.status} ${target}`)
      return response
    } catch (error) {
      const reachable = isTransportFailure(error) || isHeaderTimeout(error)
      if (!reachable) throw error
      state.failures += 1
      state.directUntil = now() + RETRY_AFTER_MS
      state.coolDown.set(target, state.directUntil)
      note(`proxy failed ${target} (${errorText(error)}), retrying direct`)
      // Always reported, not only under the debug switch: a proxy that accepts
      // the connection and then stays silent used to end in a request that never
      // returns, with nothing in the log to explain it.
      process.stderr.write(
        `[proxy] ${state.config.url} did not answer for ${target} (${errorText(error)}); ` +
          `request retried directly, proxy skipped for ${target} for ${RETRY_AFTER_MS / 1000}s\n`,
      )
      return direct(input, init)
    }
  }

  // Bun's fetch carries `preconnect`; callers copy `globalThis.fetch` wholesale.
  const enhanced = Object.assign(fetch, { preconnect: transport.preconnect })
  return { fetch: enhanced as typeof globalThis.fetch, state }
}

/**
 * Route `globalThis.fetch` through the configured proxy with a direct fallback.
 *
 * Idempotent: a second call returns the state installed by the first, and a
 * direct configuration leaves `fetch` untouched.
 */
export function install(options: RouteOptions = {}): ProxyState {
  const installed = state()
  if (installed) return installed
  const router = route(options)
  if (router.state.config.mode === "proxy") globalThis.fetch = router.fetch
  ;(globalThis as { __openbuildProxy?: ProxyState }).__openbuildProxy = router.state
  return router.state
}

/** Current routing state, or `undefined` when nothing was installed. */
export function state(): ProxyState | undefined {
  return (globalThis as { __openbuildProxy?: ProxyState }).__openbuildProxy
}

/** One-line description for diagnostics. */
export function describe(current: ProxyState | undefined = state()): string {
  if (!current) return "direct (not routed)"
  if (current.config.mode === "direct") return `direct (${current.config.source})`
  const failures = current.failures > 0 ? `, ${current.failures} failure(s)` : ""
  return `${current.config.url} via ${current.config.source}${failures}`
}

/** Set to `1` to log every routing decision to stderr. */
export const DEBUG_ENV = "OPENBUILD_PROXY_DEBUG"

/**
 * How long the proxy may take to produce response headers.
 *
 * A proxy that accepts the CONNECT and then never answers is the exact failure
 * this module exists to survive: without a deadline the request hangs forever
 * and the fallback never runs. The body is not covered — a healthy proxy may
 * legitimately stream for minutes.
 */
export const HEADER_DEADLINE_MS = 30_000

/** Marker carried by the deadline error, so the fallback can recognise it. */
const HEADER_TIMEOUT = "OPENBUILD_PROXY_HEADER_TIMEOUT"

function withHeaderDeadline(pending: Promise<Response>, limitMs: number): Promise<Response> {
  let timer: ReturnType<typeof setTimeout> | undefined
  const deadline = new Promise<never>((_, reject) => {
    timer = setTimeout(
      () => reject(new TypeError(`${HEADER_TIMEOUT}: no response headers through the proxy`)),
      limitMs,
    )
    // Never hold the process open just for this timer.
    ;(timer as { unref?: () => void }).unref?.()
  })
  return Promise.race([pending, deadline]).finally(() => {
    if (timer !== undefined) clearTimeout(timer)
  })
}

function isHeaderTimeout(error: unknown): boolean {
  return error instanceof Error && error.message.includes(HEADER_TIMEOUT)
}

function targetOf(input: RequestInfo | URL): string {
  const raw = typeof input === "string" ? input : input instanceof URL ? input.href : (input as Request).url
  try {
    return new URL(raw).host
  } catch {
    return raw.slice(0, 60)
  }
}

/**
 * Loopback, private, and single-label targets belong to the local machine or a
 * LAN box, so an outbound proxy cannot serve them and must not record the
 * refusal as a proxy failure.
 */
export function isLocalTarget(host: string): boolean {
  const name = host.startsWith("[") ? host.slice(1, host.indexOf("]")) : (host.split(":")[0] ?? "")
  const lower = name.toLowerCase()
  if (lower === "localhost" || lower.endsWith(".localhost") || lower.endsWith(".local")) return true
  if (lower === "::1" || lower === "0.0.0.0" || lower === "::") return true
  if (/^127\./.test(lower) || /^10\./.test(lower) || /^192\.168\./.test(lower)) return true
  if (/^172\.(1[6-9]|2\d|3[01])\./.test(lower)) return true
  if (/^169\.254\./.test(lower)) return true
  // A bare hostname with no dot is a machine on the local network.
  return /^[a-z0-9-]+$/.test(lower)
}

function errorText(error: unknown): string {
  const code = (error as { code?: string }).code ?? ""
  const message = error instanceof Error ? error.message : String(error)
  return `${code} ${message}`.trim().slice(0, 120)
}

function tracer(env: Record<string, string | undefined>) {
  const flag = (env[DEBUG_ENV] ?? "").trim()
  const enabled = flag !== "" && flag !== "0"
  return (message: string) => {
    if (enabled) process.stderr.write(`[proxy] ${message}\n`)
  }
}

export * as Proxy from "./proxy"
