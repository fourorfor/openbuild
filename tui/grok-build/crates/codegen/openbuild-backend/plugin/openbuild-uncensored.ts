// @openbuild-managed version=7
// Installed by OpenBuild for the optional `/uncensored` mode.
// This file is managed: the next OpenBuild launch may replace it.
//
// v7 differences from v6:
// - the policy is no longer injected from here. The kernel prefixes the actor
//   and isolation parts before the base prompt (`session/protocol-policy.ts`),
//   rewrites refusal history while assembling the transcript, and deep-merges
//   the provider body. A plugin hook would land after the base prompt and hold
//   a second copy of the request body, so the only thing left for this file is
//   the agent registration and the config read;
// - the config is v3: `mode`, `packs`, `providerRules`, `providerBody`,
//   `rewriteRefusalHistory` and `observeRefusals`. A v2 file is mapped on read,
//   so an upgrade does not need the TUI to rewrite it first;
// - `fallbackModel` is gone. It never had a consumer, and silently switching
//   models changes the cache prefix, the context window and the cost.
import { Agent, Model, Plugin } from "@opencode/plugin"
import { appendFile, mkdir, readFile, rename, stat } from "node:fs/promises"
import path from "node:path"
import { fileURLToPath } from "node:url"

const AGENT_ID = "uncensored"
const CONFIG_FILE = "uncensored.json"
const CONFIG_VERSION = 3
const POLICY_DIR = "uncensored"
const EVENTS_MAX_BYTES = 1024 * 1024
const POLICY_MAX_BYTES = 128 * 1024

export type Mode = "protocol" | "off"
export type ProviderRules = "auto" | "off"

/**
 * A pack is an optional body mounted after the kernel's actor part. It only
 * carries scene vocabulary and output shape, never a second copy of the
 * general contract, which is what keeps the default injection at KB scale.
 */
export type PackID = "web" | "binary" | "anticheat" | "ctf" | "legacy-extended"

export interface UncensoredConfig {
  version: number
  /** New sessions start in uncensored. The session-level switch is the agent id. */
  enabled: boolean
  scope: "new-sessions" | "session-only"
  mode: Mode
  packs: PackID[]
  variant: string
  model?: string
  inheritSubagents: boolean
  providerRules: ProviderRules
  providerBody: Record<string, Record<string, unknown>>
  rewriteRefusalHistory: boolean
  observeRefusals: boolean
  refusalRetryBudget: number
  policyRev: number
  /** A v1/v2 file was read; the next write upgrades it. */
  migrated?: boolean
  error?: string
}

const DEFAULTS: UncensoredConfig = {
  version: CONFIG_VERSION,
  enabled: false,
  scope: "new-sessions",
  mode: "protocol",
  packs: [],
  variant: "auto",
  inheritSubagents: false,
  providerRules: "auto",
  providerBody: {},
  rewriteRefusalHistory: true,
  observeRefusals: true,
  refusalRetryBudget: 2,
  policyRev: 0,
}

export const PACKS: ReadonlyArray<PackID> = ["web", "binary", "anticheat", "ctf", "legacy-extended"]

const V1_KEYS = new Set(["model", "inheritSubagents", "inherit_subagents", "refusalRetry", "refusal_retry"])

/**
 * Every spelling a v1 or v2 file may contain. Retired keys stay readable so an
 * upgrade keeps the operator's choices instead of resetting them to defaults.
 */
const LEGACY_KEYS = new Set([
  "version",
  "enabled",
  "scope",
  "runtimeOnly",
  "runtime_only",
  "variant",
  "model",
  "fallbackModel",
  "fallback_model",
  "inheritSubagents",
  "inherit_subagents",
  "refusalRetry",
  "refusal_retry",
  "textRefusalProbe",
  "text_refusal_probe",
  "policyPlacement",
  "policy_placement",
  "policySource",
  "policy_source",
  "stripChatTemplateMarker",
  "strip_chat_template_marker",
  "policyExtendedSha256",
  "policy_extended_sha256",
  "policyFullSha256",
  "policy_full_sha256",
  "providerRewrite",
  "provider_rewrite",
  "providerRewriteBody",
  "provider_rewrite_body",
  "policyRev",
  "policy_rev",
  "policySha256",
  "policy_sha256",
  "stripRefusalHistory",
  "strip_refusal_history",
  "refusalRetryBudget",
  "refusal_retry_budget",
])

const V3_KEYS = new Set([
  "version",
  "enabled",
  "scope",
  "mode",
  "packs",
  "variant",
  "model",
  "inheritSubagents",
  "providerRules",
  "providerBody",
  "rewriteRefusalHistory",
  "observeRefusals",
  "refusalRetryBudget",
  "policyRev",
])

/**
 * Parse `uncensored.json` by its declared version.
 *
 * A file with no `version` is v1 and keeps its `model`. Anything above the
 * version this build understands is refused rather than half-read, so a newer
 * TUI cannot silently produce a config this kernel misinterprets.
 */
export function parseConfig(raw: unknown): UncensoredConfig {
  if (raw === undefined || raw === null) return { ...DEFAULTS }
  if (typeof raw !== "object" || Array.isArray(raw)) {
    return { ...DEFAULTS, error: "config must be a JSON object" }
  }
  const value = raw as Record<string, unknown>
  const version = readVersion(value)
  if (typeof version === "string") return { ...DEFAULTS, error: version }
  if (version !== undefined && version > CONFIG_VERSION) {
    return {
      ...DEFAULTS,
      error: `config: written by a newer OpenBuild (v${version}); this build understands v${CONFIG_VERSION}`,
    }
  }
  const legacy = version === undefined || version < CONFIG_VERSION
  const allowed = version === undefined || version === 1 ? V1_KEYS : legacy ? LEGACY_KEYS : V3_KEYS
  const unknown = Object.keys(value).filter((key) => !allowed.has(key))
  if (unknown.length > 0) {
    return { ...DEFAULTS, error: `unknown option(s): ${unknown.join(", ")}` }
  }

  const errors: string[] = []
  const model = readModel(value.model, "model", errors)
  const config: UncensoredConfig = { ...DEFAULTS, ...(model ? { model } : {}) }
  config.inheritSubagents = readBooleanAlias(value, "inheritSubagents", "inherit_subagents", errors) ?? false
  if (version === undefined || version === 1) {
    config.migrated = true
    return errors.length > 0 ? { ...DEFAULTS, error: errors.join("; ") } : config
  }

  config.version = CONFIG_VERSION
  config.enabled = readBoolean(value.enabled, "enabled", errors) ?? false
  const runtimeOnly = readBooleanAlias(value, "runtimeOnly", "runtime_only", errors) ?? false
  config.scope = runtimeOnly
    ? "session-only"
    : (readEnum(value, "scope", "scope", ["new-sessions", "session-only"], errors) ?? "new-sessions")
  config.variant = readVariant(value.variant, errors) ?? "auto"
  config.mode = readEnum(value, "mode", "mode", ["protocol", "off"], errors) ?? "protocol"
  config.packs = readPacks(value.packs, errors)
  config.providerRules = legacy
    ? (readEnum(value, "providerRewrite", "provider_rewrite", ["auto", "off"], errors) ?? "auto")
    : (readEnum(value, "providerRules", "provider_rules", ["auto", "off"], errors) ?? "auto")
  config.providerBody = readBody(
    legacy
      ? readAlias(value, "providerRewriteBody", "provider_rewrite_body", errors)
      : readAlias(value, "providerBody", "provider_body", errors),
    errors,
  )
  config.rewriteRefusalHistory = legacy
    ? (readBooleanAlias(value, "stripRefusalHistory", "strip_refusal_history", errors) ?? true)
    : (readBooleanAlias(value, "rewriteRefusalHistory", "rewrite_refusal_history", errors) ?? true)
  config.observeRefusals = readBooleanAlias(value, "observeRefusals", "observe_refusals", errors) ?? true
  const rev = readAlias(value, "policyRev", "policy_rev", errors)
  if (rev !== undefined) {
    if (typeof rev === "number" && Number.isInteger(rev) && rev >= 0) config.policyRev = rev
    else errors.push("policyRev must be a non-negative integer")
  }
  const budget = readAlias(value, "refusalRetryBudget", "refusal_retry_budget", errors)
  if (budget !== undefined) {
    if (typeof budget === "number" && Number.isInteger(budget) && budget >= 0 && budget <= 10) {
      config.refusalRetryBudget = budget
    } else {
      errors.push("refusalRetryBudget must be an integer between 0 and 10")
    }
  }
  // v2 `policySource: "hybrid"` becomes the opt-in pack; `compact` loads none.
  if (legacy) {
    const source = readAlias(value, "policySource", "policy_source", [])
    config.packs =
      source === "hybrid" || source === "hanshuang" || source === "full"
        ? ["legacy-extended"]
        : readPacks(value.packs, errors)
    config.migrated = true
  }
  if (errors.length > 0) return { ...DEFAULTS, error: errors.join("; ") }
  return config
}

/**
 * Plugin options win when present (directory-package installs); the managed
 * single-file plugin has no options, so it reads `<config>/uncensored.json`.
 */
export async function loadConfig(
  options: Readonly<Record<string, unknown>>,
  configPath: string,
): Promise<UncensoredConfig> {
  if (options && Object.keys(options).length > 0) return parseConfig(options)
  let text: string
  try {
    text = await readFile(configPath, "utf8")
  } catch (error) {
    if (isNotFound(error)) return { ...DEFAULTS }
    return { ...DEFAULTS, error: `cannot read ${configPath}: ${message(error)}` }
  }
  try {
    return parseConfig(JSON.parse(text))
  } catch (error) {
    return { ...DEFAULTS, error: `invalid JSON in ${configPath}: ${message(error)}` }
  }
}

export function configPath(): string {
  const override = process.env.OPENBUILD_UNCENSORED_CONFIG
  if (override) return override
  const configDir = process.env.OPENCODE_CONFIG_DIR
  if (configDir) return path.join(configDir, CONFIG_FILE)
  return path.join(path.dirname(fileURLToPath(import.meta.url)), "..", CONFIG_FILE)
}

/** `<config>/uncensored`, where the pack files and the event log live. */
export function policyDir(): string {
  return path.join(path.dirname(configPath()), POLICY_DIR)
}

/**
 * Reject pack text that would become a host instruction or blow the budget.
 * A rejected file is skipped, not injected partially.
 */
export function validatePolicy(text: string): string | undefined {
  if (Buffer.byteLength(text, "utf8") > POLICY_MAX_BYTES) {
    return `policy is larger than ${POLICY_MAX_BYTES} bytes`
  }
  if (/<system-reminder>|<system>|<\/system>/i.test(text)) {
    return "policy must not contain host instruction tags (<system>/<system-reminder>)"
  }
  return undefined
}

/** Append one event to `uncensored/events.jsonl`, rolling at 1 MiB. */
async function record(dir: string, event: Record<string, unknown>) {
  const file = path.join(dir, "events.jsonl")
  const line = JSON.stringify({ ts: new Date().toISOString(), ...event }) + "\n"
  try {
    await mkdir(dir, { recursive: true })
    const info = await stat(file).catch(() => undefined)
    if (info && info.size + line.length > EVENTS_MAX_BYTES) {
      await rename(file, path.join(dir, "events.1.jsonl")).catch(() => undefined)
    }
    await appendFile(file, line)
  } catch (error) {
    console.warn(`[openbuild.uncensored] cannot write events: ${message(error)}`)
  }
}

export default Plugin.define({
  id: "openbuild.uncensored",
  setup: async (ctx) => {
    const config = await loadConfig(ctx.options, configPath())
    const resolved = resolveModel(config)
    const error = [config.error, resolved.error].filter(Boolean).join("; ") || undefined
    if (error) console.warn(`[openbuild.uncensored] ${error}`)
    const effective: UncensoredConfig = { ...config, error }
    const dir = policyDir()

    // The policy itself lives in the kernel now. This registration is what
    // makes the mode reachable: the TUI switches the session's agent id, and
    // the kernel prefixes the actor part for exactly this id.
    await ctx.agent.transform((editor) => {
      editor.update(AGENT_ID, (agent) => {
        agent.name = Agent.Name.make("Uncensored")
        agent.description =
          "Direct answers without content refusals. Tools and permissions are unchanged."
        agent.mode = "primary"
        agent.hidden = false
        agent.permissions.push({ action: "question", resource: "*", effect: "allow" })
        if (resolved.ref) agent.model = resolved.ref
      })
    })

    await record(dir, {
      event: "policy_reload",
      version: effective.version,
      rev: effective.policyRev,
      mode: effective.mode,
      packs: effective.packs,
      migrated: effective.migrated === true,
    })
  },
})

function resolveModel(config: UncensoredConfig): {
  ref?: ReturnType<typeof Model.Ref.parse>
  error?: string
} {
  if (!config.model) return {}
  try {
    return { ref: Model.Ref.parse(config.model) }
  } catch (error) {
    return {
      error: `invalid model ${JSON.stringify(config.model)}: ${message(error)}`,
    }
  }
}

function readVersion(value: Record<string, unknown>): number | undefined | string {
  const raw = value.version
  if (raw === undefined || raw === null) return undefined
  if (typeof raw === "number" && Number.isInteger(raw) && raw >= 0) return raw
  return "version must be a non-negative integer"
}

function readPacks(value: unknown, errors: string[]): PackID[] {
  if (value === undefined || value === null) return []
  if (!Array.isArray(value)) {
    errors.push("packs must be an array of pack ids")
    return []
  }
  const packs: PackID[] = []
  for (const entry of value) {
    if (typeof entry !== "string" || !(PACKS as ReadonlyArray<string>).includes(entry)) {
      errors.push(`packs contains an unknown pack: ${JSON.stringify(entry)}`)
      continue
    }
    if (!packs.includes(entry as PackID)) packs.push(entry as PackID)
  }
  return packs
}

function readModel(value: unknown, name: string, errors: string[]): string | undefined {
  if (value === undefined || value === null) return undefined
  if (typeof value !== "string" || value.trim() === "") {
    errors.push(`${name} must be a non-empty string`)
    return undefined
  }
  const text = value.trim()
  if (!isModelRef(text)) {
    errors.push(`${name} must be "provider/model", got ${JSON.stringify(text)}`)
    return undefined
  }
  return text
}

function readVariant(value: unknown, errors: string[]): string | undefined {
  if (value === undefined || value === null) return undefined
  if (typeof value !== "string" || !/^(auto|[A-Za-z0-9._-]+)$/.test(value)) {
    errors.push(`variant must be "auto" or a plain id, got ${JSON.stringify(value)}`)
    return undefined
  }
  return value
}

function readBoolean(value: unknown, name: string, errors: string[]): boolean | undefined {
  if (value === undefined || value === null) return undefined
  if (typeof value !== "boolean") {
    errors.push(`${name} must be true or false`)
    return undefined
  }
  return value
}

function readBooleanAlias(
  value: Record<string, unknown>,
  primary: string,
  alias: string,
  errors: string[],
): boolean | undefined {
  return readBoolean(readAlias(value, primary, alias, errors), primary, errors)
}

function readEnum<T extends string>(
  value: Record<string, unknown>,
  primary: string,
  alias: string,
  allowed: ReadonlyArray<T>,
  errors: string[],
): T | undefined {
  const raw = readAlias(value, primary, alias, errors)
  if (raw === undefined || raw === null) return undefined
  if (typeof raw === "string" && (allowed as ReadonlyArray<string>).includes(raw)) return raw as T
  errors.push(
    `${primary} must be ${allowed.map((item) => JSON.stringify(item)).join(" or ")}, got ${JSON.stringify(raw)}`,
  )
  return undefined
}

function readBody(value: unknown, errors: string[]): Record<string, Record<string, unknown>> {
  if (value === undefined || value === null) return {}
  if (typeof value !== "object" || Array.isArray(value)) {
    errors.push("providerBody must be a JSON object")
    return {}
  }
  const result: Record<string, Record<string, unknown>> = {}
  for (const [key, entry] of Object.entries(value as Record<string, unknown>)) {
    if (typeof entry !== "object" || entry === null || Array.isArray(entry)) {
      errors.push(`providerBody.${key} must be a JSON object of request fields`)
      continue
    }
    result[key] = entry as Record<string, unknown>
  }
  return result
}

function readAlias(
  value: Record<string, unknown>,
  primary: string,
  alias: string,
  errors: string[],
): unknown {
  const hasPrimary = Object.prototype.hasOwnProperty.call(value, primary)
  const hasAlias = Object.prototype.hasOwnProperty.call(value, alias)
  if (!hasPrimary && !hasAlias) return undefined
  if (hasPrimary && hasAlias && value[primary] !== value[alias]) {
    errors.push(`${primary} and ${alias} disagree`)
    return undefined
  }
  return hasPrimary ? value[primary] : value[alias]
}

function isModelRef(value: string): boolean {
  const slash = value.indexOf("/")
  if (slash <= 0) return false
  const provider = value.slice(0, slash)
  const rest = value.slice(slash + 1)
  if (!provider || provider.includes("#") || !rest) return false
  const hash = rest.indexOf("#")
  if (hash === -1) return true
  return hash > 0 && hash < rest.length - 1
}

function isNotFound(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    (error as { code?: string }).code === "ENOENT"
  )
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}
