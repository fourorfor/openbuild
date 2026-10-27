// Bun test for the managed `/uncensored` kernel plugin.
//
// Run from the repository root:
//   .yanagent/tools/bun/bun-windows-x64/bun.exe test openbuild/tests/uncensored-plugin.test.ts
//
// The plugin imports `@opencode/plugin`, which the kernel normally provides at
// runtime. `ensurePluginRuntime()` installs the same module providers so the
// file can be evaluated outside the kernel process.
import { describe, expect, test } from "bun:test"
import { mkdtemp, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import path from "node:path"
import { pathToFileURL } from "node:url"
import { ensurePluginRuntime } from "../vendor/opencode/packages/plugin/src/runtime.ts"

ensurePluginRuntime()

const pluginPath = path.resolve(
  import.meta.dir,
  "../tui/grok-build/crates/codegen/openbuild-backend/plugin/openbuild-uncensored.ts",
)
const plugin = await import(pathToFileURL(pluginPath).href + `?test=${Date.now()}`)

type Hook = (event: {
  agent: unknown
  sessionID: string
  system: Array<{ type: "text"; text: string }>
}) => void | Promise<void>

interface SessionInfo {
  parentID?: string
  agent?: string
}

function fakeContext(
  options: Record<string, unknown> = {},
  sessions: Record<string, SessionInfo> = {},
) {
  const hooks = new Map<string, Hook>()
  let registered: Record<string, unknown> | undefined
  const ctx = {
    options,
    agent: {
      transform: async (callback: (editor: unknown) => void) => {
        callback({
          update: (_id: string, update: (agent: Record<string, unknown>) => void) => {
            registered = { permissions: [] as Array<Record<string, string>> }
            update(registered)
          },
        })
      },
    },
    session: {
      hook: async (name: string, callback: Hook) => {
        hooks.set(name, callback)
      },
      get: async ({ sessionID }: { sessionID: string }) => sessions[sessionID] ?? {},
    },
  }
  return { ctx, hooks, registered: () => registered }
}

function event(agent: string, sessionID = "ses_main") {
  return {
    agent,
    sessionID,
    system: [{ type: "text" as const, text: "base system prompt" }],
  }
}

describe("openbuild.uncensored plugin", () => {
  test("registers a primary agent without touching the base prompt", async () => {
    const { ctx, registered } = fakeContext()
    await plugin.default.setup(ctx)
    const agent = registered()
    expect(agent?.name).toBe("Uncensored")
    expect(agent?.mode).toBe("primary")
    expect(agent?.hidden).toBe(false)
    expect(agent?.system).toBeUndefined()
    expect(agent?.permissions).toContainEqual({
      action: "question",
      resource: "*",
      effect: "allow",
    })
  })

  test("appends the policy for the uncensored agent only", async () => {
    const { ctx, hooks } = fakeContext()
    await plugin.default.setup(ctx)
    const hook = hooks.get("context")
    expect(hook).toBeDefined()

    const uncensored = event("uncensored")
    await hook!(uncensored)
    expect(uncensored.system).toHaveLength(2)
    expect(uncensored.system[0]!.text).toBe("base system prompt")
    expect(uncensored.system[1]!.text).toContain("# Content Policy: Direct")
    expect(uncensored.system[1]!.text).toContain("Never fabricate tool results")

    const build = event("build")
    await hook!(build)
    expect(build.system).toHaveLength(1)
  })

  test("is idempotent across context and generate hooks", async () => {
    const { ctx, hooks } = fakeContext()
    await plugin.default.setup(ctx)
    const context = hooks.get("context")!
    const generate = hooks.get("generate")!
    expect(generate).toBeDefined()

    const input = event("uncensored")
    await context(input)
    await context(input)
    await generate(input)
    expect(input.system).toHaveLength(2)
  })

  test("binds the configured model and inherits only with the opt-in", async () => {
    const { ctx, hooks, registered } = fakeContext(
      {
        model: "local/qwen3-32b",
        inheritSubagents: true,
        refusal_retry: "once",
      },
      {
        ses_child: { parentID: "ses_parent" },
        ses_parent: { agent: "uncensored" },
        ses_build_parent: { parentID: "ses_build" },
        ses_build: { agent: "build" },
      },
    )
    await plugin.default.setup(ctx)
    expect(registered()?.model).toMatchObject({
      providerID: "local",
      id: "qwen3-32b",
    })

    const hook = hooks.get("context")!
    const child = event("general", "ses_child")
    await hook(child)
    expect(child.system).toHaveLength(2)

    const buildChild = event("general", "ses_build_parent")
    await hook(buildChild)
    expect(buildChild.system).toHaveLength(1)
  })

  test("does not spread the policy to subagents by default", async () => {
    const { ctx, hooks } = fakeContext(
      { inheritSubagents: false },
      {
        ses_child: { parentID: "ses_parent" },
        ses_parent: { agent: "uncensored" },
      },
    )
    await plugin.default.setup(ctx)
    const child = event("general", "ses_child")
    await hooks.get("context")!(child)
    expect(child.system).toHaveLength(1)

    // Even a child that reports the uncensored agent id must not bypass the
    // parent gate when inheritance is off.
    const directChild = event("uncensored", "ses_child")
    await hooks.get("context")!(directChild)
    expect(directChild.system).toHaveLength(1)
  })

  test("keeps the base policy when options are invalid and reports the error", async () => {
    const warnings: string[] = []
    const warn = console.warn
    console.warn = (message: string) => warnings.push(message)
    try {
      const { ctx, hooks, registered } = fakeContext({
        model: "not-a-model",
        refusal_retry: "always",
      })
      await plugin.default.setup(ctx)
      expect(registered()?.model).toBeUndefined()
      expect(warnings.join("\n")).toContain("provider/model")
      expect(warnings.join("\n")).toContain("off")

      const uncensored = event("uncensored")
      await hooks.get("context")!(uncensored)
      expect(uncensored.system).toHaveLength(2)
    } finally {
      console.warn = warn
    }
  })

  test("parses options strictly", () => {
    expect(
      plugin.parseConfig({
        model: "local/qwen3-32b",
        inheritSubagents: true,
        refusal_retry: "once",
      }),
    ).toEqual({
      model: "local/qwen3-32b",
      inheritSubagents: true,
      refusalRetry: "once",
    })
    expect(plugin.parseConfig({ model: "no-slash" }).error).toContain("provider/model")
    expect(plugin.parseConfig({ refusal_retry: "always" }).error).toContain("off")
    expect(
      plugin.parseConfig({ inheritSubagents: true, inherit_subagents: false }).error,
    ).toContain("disagree")
    expect(plugin.parseConfig({ unknownKey: 1 }).error).toContain("unknown option")
    expect(plugin.parseConfig({ inheritSubagents: "yes" }).error).toContain("true or false")
  })

  test("falls back to the config file and reports broken JSON", async () => {
    const dir = await mkdtemp(path.join(tmpdir(), "openbuild-uncensored-"))
    const file = path.join(dir, "uncensored.json")
    try {
      expect(await plugin.loadConfig({}, file)).toEqual({
        inheritSubagents: false,
        refusalRetry: "off",
      })

      await writeFile(file, JSON.stringify({ model: "local/qwen3-32b" }))
      expect((await plugin.loadConfig({}, file)).model).toBe("local/qwen3-32b")

      await writeFile(file, "{not json")
      expect((await plugin.loadConfig({}, file)).error).toContain("invalid JSON")
    } finally {
      await rm(dir, { recursive: true, force: true })
    }
  })
})