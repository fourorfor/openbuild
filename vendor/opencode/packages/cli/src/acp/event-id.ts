/**
 * Allocates the `eventId` counter for outbound session updates.
 *
 * Grok's TUI keeps a per-session highwater and drops any live update whose
 * counter is at or below it, because it expects `eventId` to grow across a
 * session's entire history (the Rust shell mints its ids from one process-wide
 * counter for the same reason). A counter scoped to one turn restarts at 1 on
 * every prompt, so from the second turn on every update looked like a stale
 * duplicate and the reply never rendered.
 *
 * One counter per process: replay and live turns share it, so a client that
 * tracks the highwater never sees the replayed tail out of order.
 */
let counter = 0

export function nextEventId(sessionId: string) {
  counter += 1
  return `${sessionId}-${counter}`
}

export * as ACPEventId from "./event-id"
