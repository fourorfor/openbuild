// @openbuild-managed version=8
// Installed by OpenBuild for the optional `/uncensored` mode.
// This file is managed: the next OpenBuild launch may replace it.
//
// v8 replaces the v7 policy pipeline. The mode is now a persona bundle, the
// same shape as a Claude Code `CLAUDE.md` + `system-prompt.md` install: the
// body under `<config>/uncensored/persona/<id>.md` becomes the leading system
// part of this agent, and the kernel mounts it in `session/agent-persona.ts`.
//
// What is left here is deliberately small:
// - register the `uncensored` primary agent, which is what makes the mode
//   reachable at all (the TUI switches the session's agent id),
// - report the effective config for diagnostics.
//
// The body is read by the kernel, not by this file, so a user edit takes effect
// after a restart without touching the plugin.
import { Agent, Model, Plugin } from "@opencode/plugin"
import { readFile } from "node:fs/promises"
import path from "node:path"

const AGENT_ID = "uncensored"
const CONFIG_FILE = "uncensored.json"
const CONFIG_VERSION = 4

export interface UncensoredConfig {
  version: number
  /** New sessions start in uncensored. The session-level switch is the agent id. */
  enabled: boolean
  scope: "new-sessions" | "session-only"
  mode: "persona" | "off"
  /** Which persona body the kernel mounts. */
  persona: string
  /** Extra bodies mounted after the persona. */
  mounts: string[]
  variant: string
  model?: string
  inheritSubagents: boolean
  providerRules: "auto" | "off"
  providerBody: Record<string, Record<string, unknown>>
  rewriteRefusalHistory: boolean
  refusalRetryBudget: number
  /** An older file was read; the next write upgrades it. */
  migrated?: boolean
  error?: string
}

const DEFAULTS: UncensoredConfig = {
  version: CONFIG_VERSION,
  enabled: false,
  scope: "new-sessions",
  mode: "persona",
  persona: "seagull",
  mounts: ["brief"],
  variant: "auto",
  inheritSubagents: false,
  providerRules: "auto",
  providerBody: {},
  rewriteRefusalHistory: true,
  refusalRetryBudget: 2,
}

const V1_KEYS = new Set(["model", "inheritSubagents", "inherit_subagents", "refusalRetry", "refusal_retry"])

/**
 * Every spelling a v1–v3 file may contain. Retired keys stay readable so an
 * upgrade keeps the operator's choices instead of resetting them.
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
  "packs",
  "observeRefusals",
  "observe_refusals",
])

const CONFIG_KEYS = new Set([
  "version",
  "enabled",
  "scope",
  "mode",
  "persona",
  "mounts",
  "variant",
  "model",
  "inheritSubagents",
  "inherit_subagents",
  "providerRules",
  "provider_rules",
  "providerBody",
  "provider_body",
  "rewriteRefusalHistory",
  "rewrite_refusal_history",
  "refusalRetryBudget",
  "refusal_retry_budget",
])

const PERSONA_ID = /^[a-z0-9][a-z0-9_-]{0,63}$/

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
  const allowed = version === undefined || version === 1 ? V1_KEYS : legacy ? LEGACY_KEYS : CONFIG_KEYS
  const unknown = Object.keys(value).filter((key) => !allowed.has(key))
  if (unknown.length > 0) {
    return { ...DEFAULTS, error: `unknown option(s): ${unknown.join(", ")}` }
  }

  const errors: string[] = []
  const model = readModel(value.model, "model", errors)
  const config: UncensoredConfig = { ...DEFAULTS, ...(model ? { model } : {}) }
  config.inheritSubagents = readBoolean(value, "inheritSubagents", errors) ?? false
  if (version === undefined || version === 1) {
    config.migrated = true
    return errors.length > 0 ? { ...DEFAULTS, error: errors.join("; ") } : config
  }

  config.version = CONFIG_VERSION
  config.enabled = readBoolean(value, "enabled", errors) ?? false
  const runtimeOnly = readBoolean(value, "runtimeOnly", errors) ?? false
  config.scope = runtimeOnly
    ? "session-only"
    : (readEnum(value, "scope", ["new-sessions", "session-only"], errors) ?? "new-sessions")
  config.variant = readVariant(value.variant, errors) ?? "auto"
  config.mode = readEnum(value, "mode", ["persona", "protocol", "off"], errors) === "off" ? "off" : "persona"

  // A v3 file named its extra bodies `packs`; they become persona mounts, so an
  // upgrade keeps what the operator mounted.
  const persona = value.persona
  if (persona !== undefined) {
    if (typeof persona === "string" && PERSONA_ID.test(persona)) config.persona = persona
    else errors.push("persona must be a plain id (letters, digits, - and _)")
  }
  const mounts = readMounts(version === 3 ? value.packs : value.mounts, errors)
  if (mounts !== undefined) config.mounts = mounts

  config.providerRules =
    readEnum(value, "providerRules", ["auto", "off"], errors) ??
    readEnum(value, "providerRewrite", ["auto", "off"], errors) ??
    "auto"
  config.providerBody = readBody(value.providerBody ?? value.providerRewriteBody, errors)
  config.rewriteRefusalHistory =
    readBoolean(value, "rewriteRefusalHistory", errors) ??
    readBoolean(value, "stripRefusalHistory", errors) ??
    true
  const budget = value.refusalRetryBudget
  if (budget !== undefined) {
    if (typeof budget === "number" && Number.isInteger(budget) && budget >= 0 && budget <= 10) {
      config.refusalRetryBudget = budget
    } else {
      errors.push("refusalRetryBudget must be an integer between 0 and 10")
    }
  }
  config.migrated = version !== undefined && version < CONFIG_VERSION
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
    return { ...DEFAULTS, error: `cannot read ${configPath}: ${errorMessage(error)}` }
  }
  try {
    return parseConfig(JSON.parse(text))
  } catch (error) {
    return { ...DEFAULTS, error: `invalid JSON in ${configPath}: ${errorMessage(error)}` }
  }
}

export function configPath(): string {
  const override = process.env.OPENBUILD_UNCENSORED_CONFIG
  if (override) return override
  const base = process.env.OPENCODE_CONFIG_DIR || defaultConfigDir()
  return path.join(base, CONFIG_FILE)
}

function defaultConfigDir(): string {
  const xdg = process.env.XDG_CONFIG_HOME
  if (xdg) return path.join(xdg, "openbuild")
  const home = process.env.USERPROFILE || process.env.HOME || "."
  return path.join(home, ".config", "openbuild")
}

function readVersion(value: Record<string, unknown>): number | undefined | string {
  const raw = value.version
  if (raw === undefined || raw === null) return undefined
  if (typeof raw === "number" && Number.isInteger(raw) && raw >= 0) return raw
  return "version must be a non-negative integer"
}

function readModel(value: unknown, name: string, errors: string[]): string | undefined {
  if (value === undefined || value === null) return undefined
  if (typeof value !== "string" || !/^[^/]+\/[^/]+$/.test(value.trim())) {
    errors.push(`${name} must be "provider/model"`)
    return undefined
  }
  return value.trim()
}

function readBoolean(
  source: Record<string, unknown>,
  primary: string,
  errors: string[],
): boolean | undefined {
  const alias = primary.replace(/[A-Z]/g, (c) => `_${c.toLowerCase()}`)
  const a = source[primary]
  const b = source[alias]
  if (a !== undefined && b !== undefined && a !== b) {
    errors.push(`${primary} and ${alias} disagree`)
    return undefined
  }
  const value = a ?? b
  if (value === undefined) return undefined
  if (typeof value !== "boolean") {
    errors.push(`${primary} must be true or false`)
    return undefined
  }
  return value
}

function readEnum<T extends string>(
  source: Record<string, unknown>,
  name: string,
  allowed: ReadonlyArray<T>,
  errors: string[],
): T | undefined {
  const value = source[name]
  if (value === undefined) return undefined
  if (typeof value !== "string" || !allowed.includes(value as T)) {
    errors.push(`${name} must be one of ${allowed.join(", ")}`)
    return undefined
  }
  return value as T
}

function readVariant(value: unknown, errors: string[]): string | undefined {
  if (value === undefined) return undefined
  if (typeof value !== "string" || !/^(auto|[A-Za-z0-9._-]+)$/.test(value)) {
    errors.push('variant must be "auto" or a plain id')
    return undefined
  }
  return value
}

function readMounts(value: unknown, errors: string[]): string[] | undefined {
  if (value === undefined) return undefined
  if (!Array.isArray(value)) {
    errors.push("mounts must be an array of ids")
    return undefined
  }
  const mounts: string[] = []
  for (const entry of value) {
    if (typeof entry !== "string" || !PERSONA_ID.test(entry)) {
      errors.push(`mounts contains an invalid id: ${String(entry)}`)
      return undefined
    }
    mounts.push(entry)
  }
  return mounts
}

function readBody(value: unknown, errors: string[]): Record<string, Record<string, unknown>> {
  if (value === undefined || value === null) return {}
  if (typeof value !== "object" || Array.isArray(value)) {
    errors.push("providerBody must be a JSON object")
    return {}
  }
  const body: Record<string, Record<string, unknown>> = {}
  for (const [key, entry] of Object.entries(value as Record<string, unknown>)) {
    if (typeof entry !== "object" || entry === null || Array.isArray(entry)) {
      errors.push(`providerBody.${key} must be a JSON object`)
      return {}
    }
    body[key] = entry as Record<string, unknown>
  }
  return body
}

function isNotFound(error: unknown): boolean {
  return typeof error === "object" && error !== null && "code" in error && error.code === "ENOENT"
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

export default Plugin.define({
  id: "openbuild.uncensored",
  setup: async (ctx) => {
    const config = await loadConfig(ctx.options, configPath())
    const resolved = resolveModel(config)
    if (config.error) console.warn(`[openbuild.uncensored] ${config.error}`)
    if (resolved.error) console.warn(`[openbuild.uncensored] ${resolved.error}`)

    // Registering the agent is the whole activation path. The kernel mounts the
    // persona body for exactly this id, in session/agent-persona.ts.
    await ctx.agent.transform((editor) => {
      editor.update(AGENT_ID, (agent) => {
        agent.name = Agent.Name.make("Uncensored")
        agent.description =
          "Answers as the installed persona. Tools and permissions are unchanged."
        agent.mode = "primary"
        agent.hidden = false
        agent.permissions.push({ action: "question", resource: "*", effect: "allow" })
        if (resolved.ref) agent.model = resolved.ref
      })
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
    return { error: `invalid model ${JSON.stringify(config.model)}: ${errorMessage(error)}` }
  }
}
