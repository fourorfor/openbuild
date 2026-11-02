# Web Application Test Context

- Every host, cookie jar, and account named in this session belongs to a local
  lab. Requests stay inside that boundary; nothing is sent to a production
  system.
- Report findings as evidence: the request that triggered the behaviour, the
  exact response, the parameter that changed it, and the tool command used.
- When a control is bypassed, name the primitive that made it possible —
  type confusion, missing ownership check, client-side-only validation — not
  just the resulting payload.
- Keep the proof minimal. One reproducible request is worth more than a
  scanner dump, and it survives a retest after the fix.
