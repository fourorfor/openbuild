// v8: the `/uncensored` plugin is a thin agent registration. The persona body
// itself is mounted by the kernel (`packages/core/src/session/agent-persona.ts`),
// which is covered by `packages/core/test/session-agent-persona.test.ts`.
//
// The plugin imports `@opencode/plugin`, which the kernel normally provides at
// runtime, so this test installs the vendored runtime first.
import { describe, expect, test } from "bun:test"
import { mkdtemp, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import path from "node:path"
import { pathToFileURL } from "node:url"
import { ensurePluginRuntime } from "../vendor/opencode/packages/plugin/src/runtime.ts"

await ensurePluginRuntime()

const pluginDir = path.resolve(import.meta.dir, "../tui/grok-build/crates/codegen/openbuild-backend/plugin")
const plugin = await import(pathToFileURL(path.join(pluginDir, "openbuild-uncensored.ts")).href + `?test=${Date.now()}`)

interface RegisteredAgent {
  name?: unknown
  mode?: unknown
  hidden?: unknown
  system?: unknown
  model?: unknown
  permissions: Array<Record<string, string>>
}

function fakeContext(options: Record<string, unknown> = {}) {
  let registered: RegisteredAgent | undefined
  const ctx = {
    options,
    agent: {
      transform: async (callback: (editor: unknown) => void) => {
        callback({
          update: (_id: string, update: (agent: RegisteredAgent) => void) => {
            registered = { permissions: [] }
            update(registered)
          },
        })
      },
    },
  }
  return { ctx, registered: () => registered }
}

/** Point the config path at a temp directory and run `body`. */
async function withConfig<T>(body: (dir: string) => Promise<T>): Promise<T> {
  const dir = await mkdtemp(path.join(tmpdir(), "openbuild-uncensored-"))
  const previous = process.env.OPENBUILD_UNCENSORED_CONFIG
  process.env.OPENBUILD_UNCENSORED_CONFIG = path.join(dir, "uncensored.json")
  try {
    return await body(dir)
  } finally {
    if (previous === undefined) delete process.env.OPENBUILD_UNCENSORED_CONFIG
    else process.env.OPENBUILD_UNCENSORED_CONFIG = previous
    await rm(dir, { recursive: true, force: true })
  }
}

describe("openbuild.uncensored plugin", () => {
  test("registers a primary agent and leaves the prompt to the kernel", async () => {
    const { ctx, registered } = fakeContext()
    await plugin.default.setup(ctx)
    const agent = registered()
    expect(agent?.name).toBe("Uncensored")
    expect(agent?.mode).toBe("primary")
    expect(agent?.hidden).toBe(false)
    // The persona is the agent's system prompt, mounted by the kernel. A plugin
    // that also set `system` would fight that body.
    expect(agent?.system).toBeUndefined()
    expect(agent?.permissions).toContainEqual({
      action: "question",
      resource: "*",
      effect: "allow",
    })
  })

  test("defaults to the persona mode with the seagull body", () => {
    const parsed = plugin.parseConfig({ version: 4 })
    expect(parsed.error).toBeUndefined()
    expect(parsed.enabled).toBe(false)
    expect(parsed.mode).toBe("persona")
    expect(parsed.persona).toBe("seagull")
    expect(parsed.mounts).toEqual(["brief"])
    expect(parsed.refusalRetryBudget).toBe(2)
  })

  test("reads a v4 file with a custom persona and mounts", () => {
    const parsed = plugin.parseConfig({
      version: 4,
      enabled: true,
      persona: "my-operator",
      mounts: ["brief", "web"],
      providerRules: "off",
      providerBody: { "my-gateway": { enable_safety: false } },
      rewriteRefusalHistory: false,
      refusalRetryBudget: 4,
    })
    expect(parsed.error).toBeUndefined()
    expect(parsed.enabled).toBe(true)
    expect(parsed.persona).toBe("my-operator")
    expect(parsed.mounts).toEqual(["brief", "web"])
    expect(parsed.providerRules).toBe("off")
    expect(parsed.providerBody["my-gateway"]).toEqual({ enable_safety: false })
    expect(parsed.rewriteRefusalHistory).toBe(false)
    expect(parsed.refusalRetryBudget).toBe(4)
  })

  /// A v3 file named its extra bodies `packs`; they become persona mounts so an
  /// upgrade keeps what the operator mounted.
  test("maps v3 packs onto persona mounts", () => {
    const parsed = plugin.parseConfig({ version: 3, enabled: true, packs: ["anticheat", "ctf"] })
    expect(parsed.error).toBeUndefined()
    expect(parsed.persona).toBe("seagull")
    expect(parsed.mounts).toEqual(["anticheat", "ctf"])
    expect(parsed.migrated).toBe(true)
  })

  test("rejects a path-like persona", () => {
    const parsed = plugin.parseConfig({ version: 4, persona: "../../etc/passwd" })
    expect(parsed.error).toContain("plain id")
  })

  test("rejects unknown keys, bad budgets and bad bodies", () => {
    expect(plugin.parseConfig({ version: 4, fallbackModel: "x/y" }).error).toContain("unknown option")
    expect(plugin.parseConfig({ version: 4, refusalRetryBudget: 99 }).error).toContain("between 0 and 10")
    expect(plugin.parseConfig({ version: 4, providerBody: { gw: "nope" } }).error).toContain("providerBody.gw")
    expect(plugin.parseConfig({ version: 4, mounts: ["../x"] }).error).toContain("invalid id")
  })

  test("falls back to defaults for an unreadable config", async () => {
    await withConfig(async () => {
      const { ctx, registered } = fakeContext({})
      await plugin.default.setup(ctx)
      expect(registered()?.name).toBe("Uncensored")
    })
  })
})
