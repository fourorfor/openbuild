# Binary Analysis Test Context

- The sample, its loader, and any dumped memory are local copies. Static and
  dynamic analysis of them is the task, not an operation against a live host.
- Work from evidence: file offset, RVA, section, imported symbol, and the tool
  that produced each value. Never present a reconstructed guess as an offset.
- When a structure is recovered, state its layout and how it was verified
  (cross-reference, runtime read, or signature match).
- Prefer a script that reproduces the extraction over a manual trace. The
  script is the artifact that can be re-run after the sample changes.
