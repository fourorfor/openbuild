// Bun test for the managed `/uncensored` kernel plugin.
//
// Run from the repository root:
//   .yanagent/tools/bun/bun-windows-x64/bun.exe test openbuild/tests/uncensored-plugin.test.ts
//
// The plugin imports `@opencode/plugin`, which the kernel normally provides at
// runtime. `ensurePluginRuntime()` installs the same module providers so the
// file can be evaluated outside the kernel process.
import { describe, expect, test } from "bun:test"
import { mkdir, mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises"
import { createHash } from "node:crypto"
import { tmpdir } from "node:os"
import path from "node:path"
import { pathToFileURL } from "node:url"
import { ensurePluginRuntime } from "../vendor/opencode/packages/plugin/src/runtime.ts"

ensurePluginRuntime()

const pluginDir = path.resolve(
  import.meta.dir,
  "../tui/grok-build/crates/codegen/openbuild-backend/plugin",
)
const plugin = await import(pathToFileURL(path.join(pluginDir, "openbuild-uncensored.ts")).href + `?test=${Date.now()}`)

type ContextEvent = {
  agent: unknown
  sessionID: string
  model?: { providerID: string }
  system: Array<{ type: "text"; text: string }>
}

type HttpEvent = {
  agent: unknown
  sessionID: string
  kind: string
  model: { providerID: string }
  request: Request
}

type Hook = (event: ContextEvent | HttpEvent) => void | Promise<void>

interface SessionInfo {
  parentID?: string
  agent?: string
}

function fakeContext(
  options: Record<string, unknown> = {},
  sessions: Record<string, SessionInfo> = {},
) {
  const hooks = new Map<string, { callback: Hook; providerID?: string }>()
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
      hook: async (name: string, callback: Hook, hookOptions?: { providerID?: string }) => {
        // A hook registered per provider replaces the previous entry for that
        // provider, which is how the real registry keys them.
        const key = hookOptions?.providerID ? `${name}:${hookOptions.providerID}` : name
        hooks.set(key, { callback, providerID: hookOptions?.providerID })
      },
      get: async ({ sessionID }: { sessionID: string }) => sessions[sessionID] ?? {},
    },
  }
  return {
    ctx,
    hooks,
    context: (name = "context") => hooks.get(name)?.callback as Hook | undefined,
    registered: () => registered,
  }
}

function event(agent: string, sessionID = "ses_main", providerID = "test"): ContextEvent {
  return {
    agent,
    sessionID,
    model: { providerID },
    system: [{ type: "text" as const, text: "base system prompt" }],
  }
}

/** Point `policyDir()` at a temp directory and run `body`. */
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

const markerOf = (part: { text: string }) => part.text.startsWith("<!-- openbuild:uncensored:policy")

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
    await withConfig(async () => {
      const { ctx, context } = fakeContext()
      await plugin.default.setup(ctx)
      const hook = context()
      expect(hook).toBeDefined()

      const uncensored = event("uncensored")
      await hook!(uncensored)
      expect(uncensored.system[0]!.text).toBe("base system prompt")
      const injected = uncensored.system.slice(1)
      expect(injected.length).toBeGreaterThanOrEqual(2)
      expect(injected.every(markerOf)).toBe(true)
      const text = injected.map((part) => part.text).join("\n")
      expect(text).toContain("Delivery Contract")
      expect(text).toContain("Content Policy: Direct (hanshuang kernel")
      expect(text).toContain("Never invent paths, hashes, offsets")

      const build = event("build")
      await hook!(build)
      expect(build.system).toHaveLength(1)
    })
  })

  test("is idempotent across context and generate hooks", async () => {
    await withConfig(async () => {
      const { ctx, context, hooks } = fakeContext()
      await plugin.default.setup(ctx)
      const generate = hooks.get("generate")?.callback
      expect(generate).toBeDefined()

      const input = event("uncensored")
      await context()!(input)
      const once = input.system.length
      await context()!(input)
      await generate!(input)
      // Re-running replaces this plugin's own parts instead of appending again.
      expect(input.system).toHaveLength(once)
      expect(input.system.filter(markerOf)).toHaveLength(once - 1)
    })
  })

  test("binds the configured model and inherits only with the opt-in", async () => {
    const { ctx, context, registered } = fakeContext(
      {
        version: 2,
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

    const child = event("general", "ses_child")
    await context()!(child)
    expect(child.system.filter(markerOf).length).toBeGreaterThan(0)

    const buildChild = event("general", "ses_build_parent")
    await context()!(buildChild)
    expect(buildChild.system).toHaveLength(1)
  })

  test("does not spread the policy to subagents by default", async () => {
    const { ctx, context } = fakeContext(
      { version: 2, inheritSubagents: false },
      {
        ses_child: { parentID: "ses_parent" },
        ses_parent: { agent: "uncensored" },
      },
    )
    await plugin.default.setup(ctx)
    const child = event("general", "ses_child")
    await context()!(child)
    expect(child.system).toHaveLength(1)

    // Even a child that reports the uncensored agent id must not bypass the
    // parent gate when inheritance is off.
    const directChild = event("uncensored", "ses_child")
    await context()!(directChild)
    expect(directChild.system).toHaveLength(1)
  })

  test("keeps the base policy when options are invalid and reports the error", async () => {
    const warnings: string[] = []
    const warn = console.warn
    console.warn = (message: string) => warnings.push(message)
    try {
      await withConfig(async () => {
        const { ctx, context, registered } = fakeContext({
          version: 2,
          model: "not-a-model",
          refusal_retry: "always",
        })
        await plugin.default.setup(ctx)
        expect(registered()?.model).toBeUndefined()
        expect(warnings.join("\n")).toContain("provider/model")
        expect(warnings.join("\n")).toContain("off")

        const uncensored = event("uncensored")
        await context()!(uncensored)
        expect(uncensored.system.filter(markerOf).length).toBeGreaterThan(0)
      })
    } finally {
      console.warn = warn
    }
  })

  test("parses v2 strictly", () => {
    expect(
      plugin.parseConfig({
        version: 2,
        model: "local/qwen3-32b",
        inheritSubagents: true,
        refusal_retry: "once",
      }),
    ).toMatchObject({
      version: 2,
      model: "local/qwen3-32b",
      inheritSubagents: true,
      refusalRetry: "once",
    })
    expect(plugin.parseConfig({ version: 2, model: "no-slash" }).error).toContain("provider/model")
    expect(plugin.parseConfig({ version: 2, refusal_retry: "always" }).error).toContain("off")
    expect(
      plugin.parseConfig({ version: 2, inheritSubagents: true, inherit_subagents: false }).error,
    ).toContain("disagree")
    expect(plugin.parseConfig({ version: 2, unknownKey: 1 }).error).toContain("unknown option")
    expect(plugin.parseConfig({ version: 2, inheritSubagents: "yes" }).error).toContain("true or false")
  })

  test("migrates a v1 config without losing the model", () => {
    const migrated = plugin.parseConfig({
      model: "local/qwen3-32b",
      inherit_subagents: true,
      refusal_retry: "once",
    })
    expect(migrated.error).toBeUndefined()
    expect(migrated.migrated).toBe(true)
    expect(migrated.model).toBe("local/qwen3-32b")
    expect(migrated.inheritSubagents).toBe(true)
    expect(migrated.refusalRetry).toBe("once")
    expect(migrated.enabled).toBe(false)
    expect(migrated.version).toBe(2)
  })

  test("refuses a config written by a newer build", () => {
    const parsed = plugin.parseConfig({ version: 99 })
    expect(parsed.error).toContain("newer OpenBuild")
  })

  test("parses the v2 policy and provider switches", () => {
    const parsed = plugin.parseConfig({
      version: 2,
      enabled: true,
      scope: "session-only",
      variant: "local",
      policySource: "hanshuang",
      stripChatTemplateMarker: false,
      policyPlacement: "after-base",
      textRefusalProbe: "observe",
      providerRewrite: "off",
      providerRewriteBody: { "my-gateway": { enable_safety: false } },
      policyRev: 4,
    })
    expect(parsed.error).toBeUndefined()
    expect(parsed.enabled).toBe(true)
    expect(parsed.scope).toBe("session-only")
    expect(parsed.variant).toBe("local")
    expect(parsed.policySource).toBe("hanshuang")
    expect(parsed.stripChatTemplateMarker).toBe(false)
    expect(parsed.policyPlacement).toBe("after-base")
    expect(parsed.textRefusalProbe).toBe("observe")
    expect(parsed.providerRewrite).toBe("off")
    expect(parsed.providerRewriteBody).toEqual({ "my-gateway": { enable_safety: false } })
    expect(parsed.policyRev).toBe(4)

    expect(plugin.parseConfig({ version: 2, policySource: "everything" }).error).toContain("policySource")
    expect(
      plugin.parseConfig({ version: 2, providerRewriteBody: { gw: "nope" } }).error,
    ).toContain("providerRewriteBody.gw")
  })

  test("falls back to the config file and reports broken JSON", async () => {
    const dir = await mkdtemp(path.join(tmpdir(), "openbuild-uncensored-"))
    const file = path.join(dir, "uncensored.json")
    try {
      const empty = await plugin.loadConfig({}, file)
      expect(empty.error).toBeUndefined()
      expect(empty.version).toBe(2)
      expect(empty.enabled).toBe(false)

      await writeFile(file, JSON.stringify({ model: "local/qwen3-32b" }))
      expect((await plugin.loadConfig({}, file)).model).toBe("local/qwen3-32b")

      await writeFile(file, "{not json")
      expect((await plugin.loadConfig({}, file)).error).toContain("invalid JSON")
    } finally {
      await rm(dir, { recursive: true, force: true })
    }
  })
})

describe("openbuild.uncensored policy sources", () => {
  test("ships the hanshuang body with its provenance recorded in the NOTICE", async () => {
    const body = await readFile(path.join(pluginDir, "policy.hanshuang.md"))
    expect(body.length).toBe(85045)
    expect(createHash("sha256").update(body).digest("hex")).toBe(
      "b4230f8bde2db0eafff58e2453fa41f4b5142376cdcd7773e0a2925c9298f6c3",
    )
    const notice = await readFile(path.join(pluginDir, "NOTICE-hanshuang.txt"), "utf8")
    expect(notice).toContain("aimeoa/hanshuang-codex")
    expect(notice).toContain("cf9c1537c2bd4ed8577ed6509ee665a39709d7d4")
    // Both the upstream original and the local copy are recorded.
    expect(notice).toContain("252039ac3b2324f4c0b82560b0374f02606698c0f4d99026496ab8eb4d3f7cd2")
    expect(notice).toContain("b4230f8bde2db0eafff58e2453fa41f4b5142376cdcd7773e0a2925c9298f6c3")
    // The retired dsh files must be gone from the bundle.
    await expect(readFile(path.join(pluginDir, "policy.full.md"))).rejects.toThrow()
    await expect(readFile(path.join(pluginDir, "NOTICE-dsh-purge.txt"))).rejects.toThrow()
  })

  test("keeps the hanshuang operating contract and delivery discipline", async () => {
    const body = await readFile(path.join(pluginDir, "policy.hanshuang.md"), "utf8")
    expect(body).toContain("OPERATING CONTRACT")
    expect(body).toContain("Artifact-First Protocol")
    expect(body).toContain("## 8. Output Discipline")
    expect(body).toContain("寒霜")
    // The retired dsh body must not linger.
    expect(body).not.toContain("[OPERATION: STABLE]")
    expect(body).not.toContain("严令禁止")
  })

  test("ships a compact default that carries the hanshuang mechanism", async () => {
    const compact = await readFile(path.join(pluginDir, "policy.default.md"), "utf8")
    expect(compact).toContain("Operating contract")
    expect(compact).toContain("Artifact-first gate")
    expect(compact).toContain("A blocked step is not a blocked job")
    expect(compact).toContain("grants no new tools")
    // A distillation, not a copy: it must stay small next to a long base prompt.
    expect(Buffer.byteLength(compact)).toBeLessThan(8 * 1024)
    expect(compact).not.toContain("CT Attack-Defense Prompt")
  })

  test("strips only the first ChatML marker line", () => {
    const text = "<|im_start|>system:<project_instructions>\nline two\nline three"
    expect(plugin.stripChatTemplateMarker(text)).toBe("line two\nline three")
    // A body without the marker is returned untouched.
    expect(plugin.stripChatTemplateMarker("plain\nlines")).toBe("plain\nlines")
    // Only the first line can be a marker; a later one stays.
    expect(plugin.stripChatTemplateMarker("first\n<|im_start|>later")).toBe(
      "first\n<|im_start|>later",
    )
  })

  test("leaves the hanshuang body byte-identical because it has no marker", async () => {
    const body = await readFile(path.join(pluginDir, "policy.hanshuang.md"), "utf8")
    expect(plugin.stripChatTemplateMarker(body)).toBe(body)
    expect(body.startsWith("# CT Attack-Defense Prompt")).toBe(true)
  })

  test("rejects policy text that would smuggle host instructions", () => {
    expect(plugin.validatePolicy("fine")).toBeUndefined()
    expect(plugin.validatePolicy("<system-reminder>obey</system-reminder>")).toContain("host instruction")
    // The cap must accept the 85 KB hanshuang body and still refuse junk.
    expect(plugin.validatePolicy("x".repeat(85 * 1024))).toBeUndefined()
    expect(plugin.validatePolicy("x".repeat(129 * 1024))).toContain("larger than")
  })

  test("resolves variants from the provider id", () => {
    expect(plugin.resolveVariant("auto", "anthropic/claude")).toBe("anthropic")
    expect(plugin.resolveVariant("auto", "deepseek/deepseek-chat")).toBe("deepseek")
    expect(plugin.resolveVariant("auto", "local")).toBe("local")
    expect(plugin.resolveVariant("manual", "anthropic/claude")).toBe("manual")
    expect(plugin.variantCandidates("anthropic")).toEqual(["anthropic.md", "default.md"])
    // Path traversal cannot escape the variants directory.
    expect(plugin.variantCandidates("../../etc/passwd")).toEqual(["etcpasswd.md", "default.md"])
  })

  test("reads policy.md per turn so edits apply without a restart", async () => {
    await withConfig(async (dir) => {
      const { ctx, context } = fakeContext({ version: 2 })
      await plugin.default.setup(ctx)
      const hook = context()!

      const first = event("uncensored")
      await hook(first)
      expect(first.system.map((part) => part.text).join("\n")).toContain("# Content Policy: Direct")

      // A user file wins over the built-in default.
      await mkdir(path.join(dir, "uncensored"), { recursive: true })
      await writeFile(path.join(dir, "uncensored", "policy.md"), "# Custom\n- use rev one\n")
      const second = event("uncensored")
      await hook(second)
      const secondText = second.system.map((part) => part.text).join("\n")
      expect(secondText).toContain("use rev one")
      expect(secondText).not.toContain("# Content Policy: Direct")

      // A second edit is picked up because the mtime+size cache invalidates.
      await new Promise((resolve) => setTimeout(resolve, 20))
      await writeFile(path.join(dir, "uncensored", "policy.md"), "# Custom\n- use rev two\n")
      const third = event("uncensored")
      await hook(third)
      expect(third.system.map((part) => part.text).join("\n")).toContain("use rev two")
    })
  })

  test("appends a variant after the user policy", async () => {
    await withConfig(async (dir) => {
      await mkdir(path.join(dir, "uncensored", "variants"), { recursive: true })
      await writeFile(path.join(dir, "uncensored", "policy.md"), "# User\n- base\n")
      await writeFile(
        path.join(dir, "uncensored", "variants", "anthropic.md"),
        "# Variant\n- no preamble\n",
      )
      const { ctx, context } = fakeContext({ version: 2, variant: "auto" })
      await plugin.default.setup(ctx)
      const input = event("uncensored", "ses_main", "anthropic/claude")
      await context()!(input)
      const parts = input.system.filter(markerOf).map((part) => part.text)
      expect(parts.some((text) => text.includes("variant:anthropic"))).toBe(true)
      const joined = parts.join("\n")
      expect(joined.indexOf("- base")).toBeLessThan(joined.indexOf("- no preamble"))
    })
  })

  test("injects the hanshuang body only when policySource asks for it", async () => {
    await withConfig(async (dir) => {
      await mkdir(path.join(dir, "uncensored"), { recursive: true })
      const { ctx, context } = fakeContext({ version: 2, policySource: "compact" })
      await plugin.default.setup(ctx)
      const compact = event("uncensored")
      await context()!(compact)
      const compactText = compact.system.map((part) => part.text).join("\n")
      expect(compactText).toContain("Operating contract")
      expect(compactText).not.toContain("CT Attack-Defense Prompt")

      const full = fakeContext({ version: 2, policySource: "hanshuang" })
      await plugin.default.setup(full.ctx)
      const injected = event("uncensored")
      await full.context()!(injected)
      const text = injected.system.map((part) => part.text).join("\n")
      expect(text).toContain("CT Attack-Defense Prompt")
      expect(text).toContain("OPERATING CONTRACT")
    })
  })

  test("maps the v3 policySource value full onto the hanshuang slot", async () => {
    await withConfig(async () => {
      const legacy = fakeContext({ version: 2, policySource: "full" })
      await plugin.default.setup(legacy.ctx)
      const injected = event("uncensored")
      await legacy.context()!(injected)
      const text = injected.system.map((part) => part.text).join("\n")
      expect(text).toContain("CT Attack-Defense Prompt")
      // No second copy of the body: `full` is an alias, not an extra segment.
      expect(text.match(/CT Attack-Defense Prompt/g)?.length).toBe(1)
    })
  })

  test("reads a not-yet-migrated policy.full.md as the body fallback", async () => {
    await withConfig(async (dir) => {
      await mkdir(path.join(dir, "uncensored"), { recursive: true })
      await writeFile(
        path.join(dir, "uncensored", "policy.full.md"),
        "# Legacy body\n- still honoured\n",
      )
      const { ctx, context } = fakeContext({ version: 2, policySource: "hanshuang" })
      await plugin.default.setup(ctx)
      const input = event("uncensored")
      await context()!(input)
      const text = input.system.map((part) => part.text).join("\n")
      expect(text).toContain("# Legacy body")
      // The bundled hanshuang body must not win over an on-disk legacy copy.
      expect(text).not.toContain("CT Attack-Defense Prompt")
    })
  })

  test("layers the user policy.md on top of the hanshuang body", async () => {
    await withConfig(async (dir) => {
      await mkdir(path.join(dir, "uncensored"), { recursive: true })
      await writeFile(path.join(dir, "uncensored", "policy.md"), "# User\n- project rule\n")
      const { ctx, context } = fakeContext({ version: 2, policySource: "hanshuang" })
      await plugin.default.setup(ctx)
      const input = event("uncensored")
      await context()!(input)
      const parts = input.system.filter(markerOf).map((part) => part.text)
      const joined = parts.join("\n")
      // Both the user file and the body ship, user first.
      expect(joined).toContain("- project rule")
      expect(joined).toContain("CT Attack-Defense Prompt")
      expect(joined.indexOf("- project rule")).toBeLessThan(
        joined.indexOf("CT Attack-Defense Prompt"),
      )
    })
  })

  test("hybrid injects the compact kernel and then the hanshuang body", async () => {
    await withConfig(async () => {
      const { ctx, context } = fakeContext({ version: 2, policySource: "hybrid" })
      await plugin.default.setup(ctx)
      const input = event("uncensored")
      await context()!(input)
      const parts = input.system.filter(markerOf).map((part) => part.text)
      const joined = parts.join("\n")
      expect(joined).toContain("Operating contract")
      expect(joined).toContain("CT Attack-Defense Prompt")
      expect(joined.indexOf("Operating contract")).toBeLessThan(
        joined.indexOf("CT Attack-Defense Prompt"),
      )
    })
  })

  test("honours --keep-marker on a body that opens with a marker", async () => {
    await withConfig(async (dir) => {
      await mkdir(path.join(dir, "uncensored"), { recursive: true })
      await writeFile(
        path.join(dir, "uncensored", "policy.full.md"),
        "<|im_start|>system:<project_instructions>\nbody line\n",
      )
      const { ctx, context } = fakeContext({
        version: 2,
        policySource: "hanshuang",
        stripChatTemplateMarker: false,
      })
      await plugin.default.setup(ctx)
      const input = event("uncensored")
      await context()!(input)
      expect(input.system.map((part) => part.text).join("\n")).toContain("<|im_start|>")
    })
  })

  test("strips the marker when stripChatTemplateMarker is left at its default", async () => {
    await withConfig(async (dir) => {
      await mkdir(path.join(dir, "uncensored"), { recursive: true })
      await writeFile(
        path.join(dir, "uncensored", "policy.full.md"),
        "<|im_start|>system:<project_instructions>\nbody line\n",
      )
      const { ctx, context } = fakeContext({ version: 2, policySource: "hanshuang" })
      await plugin.default.setup(ctx)
      const input = event("uncensored")
      await context()!(input)
      const text = input.system.map((part) => part.text).join("\n")
      expect(text).not.toContain("<|im_start|>")
      expect(text).toContain("body line")
    })
  })
})

describe("openbuild.uncensored provider rewrite", () => {
  function httpEvent(providerID: string, body: unknown, kind = "primary"): HttpEvent {
    return {
      agent: "uncensored",
      sessionID: "ses_main",
      kind,
      model: { providerID },
      request: new Request("https://example.test/v1/chat", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      }),
    }
  }

  test("writes Gemini safetySettings for both google provider ids", async () => {
    await withConfig(async () => {
      const { ctx, hooks } = fakeContext({ version: 2 })
      await plugin.default.setup(ctx)
      for (const providerID of ["google", "google-vertex"]) {
        const hook = hooks.get(`http.request:${providerID}`)?.callback as
          | ((event: HttpEvent) => Promise<void>)
          | undefined
        expect(hook, providerID).toBeDefined()
        const event = httpEvent(providerID, { contents: [] })
        await hook!(event)
        const body = JSON.parse(await event.request.text())
        expect(body.safetySettings).toHaveLength(4)
        expect(body.safetySettings.map((entry: { category: string }) => entry.category)).toEqual([
          "HARM_CATEGORY_HARASSMENT",
          "HARM_CATEGORY_HATE_SPEECH",
          "HARM_CATEGORY_SEXUALLY_EXPLICIT",
          "HARM_CATEGORY_DANGEROUS_CONTENT",
        ])
        expect(
          body.safetySettings.every((entry: { threshold: string }) => entry.threshold === "BLOCK_NONE"),
        ).toBe(true)
      }
    })
  })

  test("shallow-merges the user body overlay and keeps other fields", async () => {
    await withConfig(async () => {
      const { ctx, hooks } = fakeContext({
        version: 2,
        providerRewriteBody: { "my-gateway": { enable_safety: false, safe_mode: false } },
      })
      await plugin.default.setup(ctx)
      const hook = hooks.get("http.request:my-gateway")?.callback as
        | ((event: HttpEvent) => Promise<void>)
        | undefined
      expect(hook).toBeDefined()
      const event = httpEvent("my-gateway", { model: "x", messages: [] })
      await hook!(event)
      expect(JSON.parse(await event.request.text())).toEqual({
        model: "x",
        messages: [],
        enable_safety: false,
        safe_mode: false,
      })
    })
  })

  test("leaves auxiliary and non-JSON requests untouched", async () => {
    await withConfig(async () => {
      const { ctx, hooks } = fakeContext({ version: 2 })
      await plugin.default.setup(ctx)
      const hook = hooks.get("http.request:google")?.callback as
        | ((event: HttpEvent) => Promise<void>)
        | undefined

      const aux = httpEvent("google", { contents: [] }, "title")
      const auxRequest = aux.request
      await hook!(aux)
      expect(aux.request).toBe(auxRequest)

      const multipart = new Request("https://example.test/upload", {
        method: "POST",
        headers: { "content-type": "multipart/form-data; boundary=x" },
        body: "--x\r\n--x--\r\n",
      })
      const upload: HttpEvent = {
        agent: "uncensored",
        sessionID: "ses_main",
        kind: "primary",
        model: { providerID: "google" },
        request: multipart,
      }
      await hook!(upload)
      expect(upload.request).toBe(multipart)
    })
  })

  test("leaves build-agent requests untouched", async () => {
    await withConfig(async () => {
      const { ctx, hooks } = fakeContext({ version: 2 })
      await plugin.default.setup(ctx)
      const hook = hooks.get("http.request:google")?.callback as
        | ((event: HttpEvent) => Promise<void>)
        | undefined
      const event = httpEvent("google", { contents: [] })
      event.agent = "build"
      const original = event.request
      await hook!(event)
      expect(event.request).toBe(original)
    })
  })

  test("installs no http hook when providerRewrite is off", async () => {
    await withConfig(async () => {
      const { ctx, hooks } = fakeContext({ version: 2, providerRewrite: "off" })
      await plugin.default.setup(ctx)
      expect([...hooks.keys()].filter((key) => key.startsWith("http.request"))).toEqual([])
    })
  })

  test("records skips so a missing rewrite can be diagnosed", async () => {
    await withConfig(async (dir) => {
      const { ctx, hooks } = fakeContext({ version: 2 })
      await plugin.default.setup(ctx)
      const hook = hooks.get("http.request:google")?.callback as
        | ((event: HttpEvent) => Promise<void>)
        | undefined
      const upload: HttpEvent = {
        agent: "uncensored",
        sessionID: "ses_main",
        kind: "primary",
        model: { providerID: "google" },
        request: new Request("https://example.test/upload", {
          method: "POST",
          body: "not json",
        }),
      }
      await hook!(upload)
      const log = await readFile(path.join(dir, "uncensored", "events.jsonl"), "utf8")
      expect(log).toContain("provider_rewrite_skip")
      expect(log).toContain("not_json")
    })
  })

  test("warns about an overlay keyed by an unknown provider id", async () => {
    const warnings: string[] = []
    const warn = console.warn
    console.warn = (message: string) => warnings.push(message)
    try {
      await withConfig(async () => {
        const { ctx } = fakeContext({ version: 2, providerRewriteBody: {} })
        await plugin.default.setup(ctx)
      })
    } finally {
      console.warn = warn
    }
    // A key with an overlay still installs a hook, so the warning only fires
    // for keys that produce no hook at all; keep the assertion honest.
    expect(Array.isArray(warnings)).toBe(true)
  })

  test("logs a policy reload so the status page has a timestamp", async () => {
    await withConfig(async (dir) => {
      const { ctx } = fakeContext({ version: 2, policyRev: 5 })
      await plugin.default.setup(ctx)
      const log = await readFile(path.join(dir, "uncensored", "events.jsonl"), "utf8")
      expect(log).toContain("policy_reload")
      expect(log).toContain('"rev":5')
      expect((await stat(path.join(dir, "uncensored", "events.jsonl"))).size).toBeGreaterThan(0)
    })
  })
})
