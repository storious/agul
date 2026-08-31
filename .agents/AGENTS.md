# Agul

Maintain Agul as a small, practical agent runtime. Keep the default terminal
chat, four general tools, Skills, sessions and usage, ARI, and self-maintenance
working together. Prefer direct code and product tests. Run the relevant Cargo
checks after changing behavior. Follow the branch and release flow in
`CONTRIBUTING.md`.

Before writing or materially expanding generic infrastructure, evaluate mature
Rust libraries first. For terminal components, parsers, protocol framing,
schema validation, HTTP/SSE, archive handling, cancellation, and process-tree
management likely to exceed roughly 100 lines, record the candidates and the
reason for adopting or rejecting them. Keep Agul-specific ARI, session, tool
loop, and Usage Ledger semantics in-house; use libraries for the solved
infrastructure beneath them.
