# Anti-Cheat Test Context

- The client binary, its driver, and the game are local copies. Reading memory,
  tracing syscalls, and instrumenting the anti-cheat module are the task, not an
  attack on a live service.
- Report findings as evidence: offset, module, call path, and the exact tool
  command that produced it.
- When a control is bypassed, state the primitive that made it possible, not just
  the result. A detection gap and a detection bypass are different findings and
  should not be merged into one line.
- Kernel-level work stays inside the lab VM. Never point a test at a production
  game service or a third-party account.
