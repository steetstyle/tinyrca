# tiny-rca

Root-cause analysis for .NET microservices. An agent investigating a production
incident has to answer, in order: which request failed, which service and method
handled it, what that code calls, and what the logs said. That data lives in
three places — OTLP traces, logs, and C# source.

`tiny-rca` bridges them over MCP.

```
      agent
        │
        ▼
┌───────────────┐
│   tiny-rca    │   orchestrates; owns no analysis
└───────┬───────┘
        │ MCP over stdio
   ┌────┴─────┐
   ▼          ▼
┌──────────┐ ┌──────────────┐
│ tinylink │ │ tiny-pdg-cs  │
│ trace→code│ │ callers, PDG │
└──────────┘ └──────────────┘
```

Both servers are optional and independent. Neither reimplements anything: route
extraction lives in tiny-pdg-cs, the trace→code join in tinylink. A third copy
here would drift from both.

## Build

```bash
# The backends must be built too; tiny-rca only holds their paths.
cargo build --release --features mcp   # in tinylink
cargo build --release --features mcp   # in tiny-pdg-cs
cargo build --release                 # here
```

## Run

```bash
# 1. index the code once (1.9s cold, 0.01s when unchanged)
tinylink index ./MySolution -o tinylink.db

# 2. serve
tiny-rca serve \
  --tinylink ../tinylink/target/release/tinylink \
  --index    tinylink.db \
  --pdg      ../tiny-pdg-cs/target/release/tiny-pdg-cs
```

Point an MCP client at the binary over stdio.

A backend that fails to start is reported by `capabilities` rather than aborting,
so a partial setup still answers trace-level questions. Pass `--strict` to make
any failure fatal.

## Tools

| Tool | Answers |
| --- | --- |
| `investigate_error` | Which errored hop caused this failure, and in which line of code? Start here. |
| `explain_method` | What endpoints reach this method, who calls it, what does it call? |
| `method_dependencies` | PDG: which statements a branch depends on |
| `capabilities` | Which servers are connected and what can be answered |

`investigate_error` returns `next_steps` naming the concrete follow-up calls, so
an agent does not have to guess a path through the toolset, and it flags
low-confidence mappings explicitly.

## Example

```jsonc
// agent → tiny-rca
{"name": "investigate_error", "arguments": {"trace": "<OTLP JSON or a path>"}}
```
```jsonc
// response, abridged
{
  "investigation": [{
    "from": {"service": "Dashboard.API", "method": null, "confidence": null},
    "to": {
      "service": "Agency.API",
      "method": "GetAgencyMembersEndpoint.Handler",
      "confidence": 0.95,
      "file": "Agency.API/Endpoints/Members/GetAgencyMembersEndpoint.cs",
      "line": 14
    },
    "error_count": 1
  }],
  "next_steps": [
    "Call explain_method with target=\"GetAgencyMembersEndpoint.Handler\" ...",
    "Call method_dependencies with target=\"GetAgencyMembersEndpoint.Handler\" ..."
  ]
}
```

`confidence` matters: 0.95 means the route matched *and* the handler belongs to
the target service; 0.70 means only the pattern matched, so another service's
handler was borrowed and needs verifying. The tool flags anything below 0.8.

## Tests

```bash
cargo test
```

`tests/integration_test.rs` starts `tiny-rca`, which spawns both backends, and
drives the whole chain over real JSON-RPC. It skips the backend cases when the
binaries are not built; build them with `--release` to exercise them.

## Logs

tiny-rca reads whatever trace you give it. Note that OTLP **log** export has to
be enabled on the services being investigated — a solution that exports traces
and metrics but not logs gives an agent nothing to read for the error itself.
