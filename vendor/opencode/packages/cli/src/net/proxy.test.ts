import { describe, expect, test } from "bun:test"
import { isTransportFailure, normalise, readConfig, route } from "./proxy"

/** A transport that records calls and answers with the given behaviour. */
function fakeTransport(answer: (init: RequestInit | undefined, call: number) => Promise<Response>) {
  const calls: Array<{ url: string; init: RequestInit | undefined; env: Record<string, string | undefined> }> = []
  const transport = (async (input: RequestInfo | URL, init?: RequestInit) => {
    calls.push({ url: String(input), init, env: { ...process.env } })
    return answer(init, calls.length)
  }) as typeof globalThis.fetch
  return { transport, calls }
}

const ok = () => Promise.resolve(new Response("ok", { status: 200 }))

describe("readConfig", () => {
  test("OPENBUILD_PROXY is decisive", () => {
    expect(readConfig({ OPENBUILD_PROXY: "127.0.0.1:10808", HTTPS_PROXY: "http://other:1" })).toEqual({
      mode: "proxy",
      url: "http://127.0.0.1:10808",
      source: "OPENBUILD_PROXY",
    })
  })

  test("off/off-like values force direct even with an ambient proxy", () => {
    for (const value of ["off", "none", "direct", "false", "0", " OFF "]) {
      expect(readConfig({ OPENBUILD_PROXY: value, HTTPS_PROXY: "http://127.0.0.1:10808" })).toEqual({
        mode: "direct",
        source: "OPENBUILD_PROXY",
      })
    }
  })

  test("ambient variables are used when OPENBUILD_PROXY is absent", () => {
    expect(readConfig({ HTTPS_PROXY: "http://127.0.0.1:10808" })).toEqual({
      mode: "proxy",
      url: "http://127.0.0.1:10808",
      source: "HTTPS_PROXY",
    })
  })

  test("no configuration stays direct", () => {
    expect(readConfig({})).toEqual({ mode: "direct", source: "none" })
  })

  test("normalise adds a scheme only when missing", () => {
    expect(normalise("127.0.0.1:10808")).toBe("http://127.0.0.1:10808")
    expect(normalise("socks5://127.0.0.1:1080/")).toBe("socks5://127.0.0.1:1080")
  })
})

describe("isTransportFailure", () => {
  test("a refused proxy connection counts", () => {
    expect(isTransportFailure(new TypeError("ECONNREFUSED: Unable to connect"))).toBe(true)
  })

  test("an aborted request does not", () => {
    const error = new Error("aborted")
    error.name = "AbortError"
    expect(isTransportFailure(error)).toBe(false)
  })

  test("an ordinary error does not", () => {
    expect(isTransportFailure(new Error("model not found"))).toBe(false)
    expect(isTransportFailure("ECONNREFUSED")).toBe(false)
  })
})

describe("route", () => {
  test("a configured proxy is passed per request", async () => {
    const { transport, calls } = fakeTransport(ok)
    const router = route({ env: { OPENBUILD_PROXY: "http://127.0.0.1:10808" }, transport })
    await router.fetch("https://example.test/v1/models")
    expect(calls).toHaveLength(1)
    expect((calls[0]!.init as { proxy?: string }).proxy).toBe("http://127.0.0.1:10808")
  })

  test("a transport failure retries the same request directly", async () => {
    const { transport, calls } = fakeTransport((init, call) => {
      if (call === 1) return Promise.reject(new TypeError("ECONNREFUSED: Unable to connect"))
      expect((init as { proxy?: string } | undefined)?.proxy).toBeUndefined()
      return ok()
    })
    const env: Record<string, string | undefined> = { OPENBUILD_PROXY: "http://127.0.0.1:9" }
    const router = route({ env, transport })
    const response = await router.fetch("https://example.test/v1/models")
    expect(response.status).toBe(200)
    expect(calls).toHaveLength(2)
    expect(router.state.failures).toBe(1)
    // the proxy variables are restored for the rest of the process
    expect(env.OPENBUILD_PROXY).toBe("http://127.0.0.1:9")
  })

  test("the direct retry runs without proxy environment variables", async () => {
    let seen: Record<string, string | undefined> = {}
    let calls = 0
    const transport = (async () => {
      calls += 1
      if (calls === 1) throw new TypeError("ECONNREFUSED: Unable to connect")
      seen = {
        HTTPS_PROXY: process.env.HTTPS_PROXY,
        HTTP_PROXY: process.env.HTTP_PROXY,
        ALL_PROXY: process.env.ALL_PROXY,
      }
      return new Response("ok")
    }) as unknown as typeof globalThis.fetch
    const saved = { ...process.env }
    // `route` defaults to `process.env`, which is what production uses.
    Object.assign(process.env, {
      OPENBUILD_PROXY: "http://127.0.0.1:9",
      HTTPS_PROXY: "http://127.0.0.1:9",
      HTTP_PROXY: "http://127.0.0.1:9",
      ALL_PROXY: "http://127.0.0.1:9",
    })
    try {
      const router = route({ transport })
      await router.fetch("https://example.test/v1/models")
      expect(calls).toBe(2)
      expect(seen).toEqual({ HTTPS_PROXY: undefined, HTTP_PROXY: undefined, ALL_PROXY: undefined })
      // restored immediately after the synchronous fetch call
      expect(process.env.HTTPS_PROXY).toBe("http://127.0.0.1:9")
    } finally {
      for (const name of ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "OPENBUILD_PROXY"]) {
        if (saved[name] === undefined) delete process.env[name]
        else process.env[name] = saved[name]
      }
    }
  })

  test("an HTTP error response is not retried", async () => {
    const { transport, calls } = fakeTransport(() => Promise.resolve(new Response("boom", { status: 500 })))
    const router = route({ env: { OPENBUILD_PROXY: "http://127.0.0.1:10808" }, transport })
    const response = await router.fetch("https://example.test/v1/models")
    expect(response.status).toBe(500)
    expect(calls).toHaveLength(1)
    expect(router.state.failures).toBe(0)
  })

  test("a failed proxy is skipped during the cool-down", async () => {
    let clock = 1_000
    const { transport, calls } = fakeTransport((init, call) => {
      if (call === 1) return Promise.reject(new TypeError("ECONNREFUSED"))
      return ok()
    })
    const router = route({ env: { OPENBUILD_PROXY: "http://127.0.0.1:9" }, transport, now: () => clock })
    await router.fetch("https://example.test/1")
    await router.fetch("https://example.test/2")
    expect(calls).toHaveLength(3)
    expect((calls[2]!.init as { proxy?: string } | undefined)?.proxy).toBeUndefined()
    // after the cool-down the proxy is tried again
    clock += 61_000
    await router.fetch("https://example.test/3")
    expect((calls[3]!.init as { proxy?: string }).proxy).toBe("http://127.0.0.1:9")
  })

  test("a direct configuration sends no proxy option", async () => {
    const { transport, calls } = fakeTransport(ok)
    const router = route({ env: {}, transport })
    await router.fetch("https://example.test/v1/models")
    expect((calls[0]!.init as { proxy?: string } | undefined)?.proxy).toBeUndefined()
  })
})
