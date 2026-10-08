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

# 2. serve — stdio, which is what an MCP client launches
tiny-rca serve \
  --tinylink ../tinylink/target/release/tinylink \
  --index    tinylink.db \
  --pdg      ../tiny-pdg-cs/target/release/tiny-pdg-cs
```

A backend that fails to start is reported by `capabilities` rather than aborting,
so a partial setup still answers trace-level questions. Pass `--strict` to make
any failure fatal.

## Transports

stdio or Streamable HTTP, on both sides. SSE is not offered: the MCP spec
deprecated the standalone SSE transport and the Rust SDK ships no server for it.

**tiny-rca as a client** — spawn the backends, or connect to ones already
running:

```bash
# spawn (local agent)
tiny-rca serve --tinylink ./tinylink --index tinylink.db --pdg ./tiny-pdg-cs

# connect to shared servers (container, or one index for many agents)
tiny-pdg-cs serve --http --port 8081
tinylink      serve --http --port 8082 --index tinylink.db
tiny-rca serve \
  --tinylink-url http://127.0.0.1:8082/mcp \
  --pdg-url     http://127.0.0.1:8081/mcp
```

Connecting by URL is the deployment that matters at scale: spawning a process per
tiny-rca would reparse the whole solution each time, which is the cost the index
exists to remove.

**tiny-rca as a server** — `--http` exposes the same four tools remotely:

```bash
tiny-rca serve --tinylink-url http://127.0.0.1:8082/mcp --http --port 8080
```

Both sides bind loopback by default and log a warning on a non-loopback bind,
because neither has authentication. The MCP authorization spec expects an OAuth
flow that is not implemented here, so put an authenticating proxy in front
before exposing either.

## Tools

| Tool | Answers |
| --- | --- |
| `investigate_error` | Which errored hop caused this failure, and in which line of code? Start here. |
| `explain_method` | What endpoints reach this method, who calls it, what does it call? |
| `method_dependencies` | PDG: which statements a branch depends on |
| `search_logs` | What did this trace, service or error string actually log? |
| `ingest_logs` | Load OTLP log payloads into the log store (the one tool that writes) |
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

tiny-rca reads whatever trace you give it, but logs work differently: traces
arrive per call, logs are loaded once and then queried.

```jsonc
// 1. load the payloads — a file, or a directory with one payload per file
{"name": "ingest_logs", "arguments": {"source": "/var/log/otel"}}

// 2. either query them directly...
{"name": "search_logs", "arguments": {"min_severity": "Error", "contains": "TimeoutException"}}

// ...or attach them to the investigation
{"name": "investigate_error", "arguments": {"trace": "trace.json", "include_logs": true}}
```

With `include_logs`, each failing hop carries the records its trace produced —
so the answer names both the method that failed and the exception it threw, which
is the join an incident actually needs.

Two things about OTLP log data are worth knowing, both established by decoding a
real `AddOtlpExporter` capture rather than reading the schema:

- **The body is the unrendered message template.** A log exports as
  `Handler {HandlerName} failed` with the values in separate attributes, so
  free-text search covers attribute values too. Without that, the string you paste
  in during an incident would not match the log containing it.
- **The .NET SDK exports over gRPC on 4317** unless you set
  `Protocol = OtlpExportProtocol.HttpProtobuf`. Capturing from a collector is the
  usual way to get readable payloads.

`services` must actually export logs. A solution that exports traces and metrics
but not logs gives an agent nothing to read for the error itself, and
`capabilities` says so rather than letting every log query come back empty.
