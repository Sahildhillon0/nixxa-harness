# contracts — the shared language

Types, traits, events and error enums that every other module speaks. No logic.

| | |
|---|---|
| **Owns** | Domain types, module traits (`Brain`, `Sandbox`, `Store`, `Knowledge`, `Credentials`, `ApprovalPort`, `EventSink`), `Event`, error enums, conformance test suites |
| **Does not own** | Any logic or I/O |
| **Contract** | Defines all contracts |
| **Depends on** | nothing in the workspace |
| **Comes alive** | Stage 1 |

The Rust doc comments in `src/` are the authoritative API. Conformance suites live in
`src/conformance/` behind the `conformance` feature; enable it as a dev-dependency feature in
any crate that implements a trait and run the matching `run` function from a test.

See [ARCHITECTURE.md § 3](../ARCHITECTURE.md#3-contracts) for the overview.
