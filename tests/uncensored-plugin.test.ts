// Bun test for the managed `/uncensored` kernel plugin.
//
// Run from the repository root:
//   .yanagent/tools/bun/bun-windows-x64/bun.exe test openbuild/tests/uncensored-plugin.test.ts
//
// The plugin imports `@opencode/plugin`, which the kernel normally provides at
// runtime. `ensurePluginRuntime()` installs the same module providers so the
// file can be evaluated outside the kernel process.
import { describe, expect, test } from "bun:test"
import path from "node:path"
import { pathToFileURL } from "node:url"
import { ensurePluginRuntime } from "../vendor/opencode/packages/plugin/src/runtime.ts"

ensurePluginRuntime()

const pluginPath = path.resolve(
  import.meta.dir,
  "../tui/grok-build/crates/codegen/openbuild-backend/plugin/openbuild-uncensored.ts",
)
const plugin = await import(pathToFileURL(pluginPath).href + `?test=${Date.now()}`)

type Hook = (event: { agent: unknown; system: Array<{ type: "text"; text: string }> }) => void

function fakeContext() {
  const hooks = new Map<string, Hook>()
  let registered: Record<string, unknown> | undefined
  const ctx = {
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
    },
  }
  return { ctx, hooks, registered: () => registered }
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

    const uncensored = {
      agent: "uncensored",
      system: [{ type: "text" as const, text: "base system prompt" }],
    }
    hook!(uncensored)
    expect(uncensored.system).toHaveLength(2)
    expect(uncensored.system[0]!.text).toBe("base system prompt")
    expect(uncensored.system[1]!.text).toContain("# Content Policy: Direct")
    expect(uncensored.system[1]!.text).toContain("Never fabricate tool results")

    const build = {
      agent: "build",
      system: [{ type: "text" as const, text: "base system prompt" }],
    }
    hook!(build)
    expect(build.system).toHaveLength(1)
  })

  test("is idempotent across context and generate hooks", async () => {
    const { ctx, hooks } = fakeContext()
    await plugin.default.setup(ctx)
    const context = hooks.get("context")!
    const generate = hooks.get("generate")!
    expect(generate).toBeDefined()

    const event = {
      agent: "uncensored",
      system: [{ type: "text" as const, text: "base" }],
    }
    context(event)
    context(event)
    generate(event)
    expect(event.system).toHaveLength(2)
  })
})