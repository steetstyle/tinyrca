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

Both backends are optional and independent. Neither is reimplemented here: route
extraction lives in tiny-pdg-cs, the trace→code join in tinylink. A third copy
would drift from both, and a drifted route table is worse than no route table.

tiny-rca holds four files. The value is in the ordering and the handoffs, not in
the analysis.

## Build

```bash
# The backends must be built too; tiny-rca only holds their paths.
cargo build --release --features mcp   # in tinylink
cargo build --release --features mcp   # in tiny-pdg-cs
cargo build --release                 # here
```

tiny-rca has no feature flags: it is an MCP client and an MCP server, and neither
half is optional. `cargo build --release` gives you everything it has.

## Run

```bash
# 1. index the code once (~2s cold, ~0.003s when unchanged)
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

**tiny-rca as a server** — `--http` exposes the same tools remotely:

```bash
tiny-rca serve --tinylink-url http://127.0.0.1:8082/mcp --http --port 8080
```

Both sides bind loopback by default and log a warning on a non-loopback bind,
because neither has authentication. The MCP authorization spec expects an OAuth
flow that is not implemented here, so put an authenticating proxy in front before
exposing either.

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

### What it actually returns

`capabilities` reports which backends are up and, importantly, what they expose —
so an agent knows the ceiling before it starts:

```json
{"can_analyse_code":true,"can_map_traces_to_code":true,"can_search_logs":true,
 "log_guidance":"Call ingest_logs with an OTLP payload file, or a directory with one payload per file, before search_logs. An empty store makes every log query return nothing.",
 "servers":[
   {"connected":true,"server":"tinylink","role":"maps OpenTelemetry traces to indexed C# methods",
    "tools":["blast_radius","explain_error","find_methods","ingest_logs","list_indexed_services","search_logs"]},
   {"connected":true,"server":"tiny-pdg-cs","role":"static code analysis: callers, PDGs, routes, patterns",
    "tools":["diff_impact","find_callers","find_patterns","list_routes","method_callees","method_pdg","project_summary"]}],
 "unavailable":[]}
```

The backend tool lists are reported rather than merged into tiny-rca's own: they
change when a backend changes, and a stale copy here would tell an agent to call
a tool that no longer exists.

## Example

A trace where `Dashboard.API` calls `Agency.API` and the second hop returns 500:

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"tools/call",
       "params":{"name":"investigate_error","arguments":{"trace":"<OTLP JSON or a path>"}}}' \
  | tiny-rca serve --tinylink ./tinylink --index tinylink.db --pdg ./tiny-pdg-cs
```

```jsonc
// response, abridged
{
  "error_edge_count": 1,
  "investigation": [{
    "from": {"service": "Dashboard.API", "method": null, "confidence": null},
    "to": {
      "service": "Agency.API",
      "method": "GetAgencyMembersEndpoint.Handler",
      "confidence": 0.95,
      "file": "Agency.API/Endpoints/Members/GetAgencyMembersEndpoint.cs",
      "line": 14
    },
    "error_count": 1,
    "trace_ids": ["err001"],
    "avg_latency_ms": 350.0
  }],
  "next_steps": [
    "Call explain_method with target=\"GetAgencyMembersEndpoint.Handler\" for its endpoints, callers and callees.",
    "Call method_dependencies with target=\"GetAgencyMembersEndpoint.Handler\" for the control and data dependences."
  ],
  "unavailable_backends": []
}
```

`confidence` matters: 0.95 means the route matched *and* the handler belongs to
the target service; 0.70 means only the pattern matched, so another service's
handler was borrowed and needs verifying. The tool flags anything below 0.8.

Following that hint:

```jsonc
// explain_method
{"method":"GetAgencyMembersEndpoint.Handler",
 "results":[{
   "service":"Agency.API",
   "file":"example-projects/unicpeak-analytics-api/Agency.API/Endpoints/Members/GetAgencyMembersEndpoint.cs",
   "line_start":14,"line_end":44,
   "signature":"Task<IResult> Handler(Guid, GetAgencyMembersQueryHandler, int, int)",
   "routes":[{"method":"GET","pattern":"/api/agency/{agencyId}/member","service":"Agency.API"}],
   "callers":[],
   "callees":[{"method":"HandleAsync","line":23},{"method":"ToList","line":36},{"method":"ToString","line":40}]}]}
```

## Logs

tiny-rca reads whatever trace you give it, but logs work differently: traces
arrive per call, logs are loaded once and then queried.

```jsonc
// 1. load the payloads — a file, or a directory with one payload per file
{"name": "ingest_logs", "arguments": {"source": "/var/log/otel"}}

// 2. either query them directly...
{"name": "search_logs", "arguments": {"min_severity": "Error", "contains": "InvalidOperationException"}}

// ...or attach them to the investigation
{"name": "investigate_error", "arguments": {"trace": "trace.json", "include_logs": true}}
```

Ingesting a real capture:

```jsonc
{"payloads_read":1,"records_inserted":8,"store_records":8,
 "distinct_traces":2,"distinct_services":1,"services":["Agency.API"],"problems":[]}
```

`problems` is a list rather than a single error on purpose: a directory of
payloads where one file is truncated should load the other nineteen, and say which
one it could not read.

Searching it back:

```jsonc
{"count":2,"has_more":false,"available_services":["Agency.API"],
 "logs":[{
   "body":"Handler {HandlerName} failed: {Message}",
   "service":"Agency.API","severity":"Error",
   "attributes":{
     "HandlerName":"GetAgencyMembers",
     "Message":"no name for agency 9f1c",
     "exception.type":"InvalidOperationException",
     "exception.message":"no name for agency 9f1c"}}]}
```

With `include_logs`, each failing hop carries the records its own trace produced —
so the answer names both the method that failed and the exception it threw, which
is the join an incident actually needs. A hop whose trace id appears in no log
gets `"logs": null` rather than an empty array, because those are different
answers: "no logs loaded for this trace" versus "this trace logged nothing".

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

## Project structure

```
src/
  main.rs       binary entry point: clap, logging, serve wiring
  lib.rs        RcaServer: the MCP server, its six tools, backend connections
  backends.rs   spawning or connecting to tinylink and tiny-pdg-cs
  tools.rs      the tool bodies and the arguments they take
tests/
  integration_test.rs  drives the whole chain over real JSON-RPC
  http_test.rs         Streamable HTTP in both directions
```

`lib.rs` holds the MCP surface and nothing else; `tools.rs` holds the logic and
knows nothing about MCP. That split is what lets the tool bodies be unit-tested
without a transport.

## Requirements

- Rust 2021 edition
- `rmcp` 3 with the stdio, child-process, and Streamable HTTP client/server
  transports — the same version tinylink and tiny-pdg-cs use, so one client drives
  all three
- Both backend binaries built with `--features mcp`

## Tests

```bash
cargo test
```

**31 tests.** `tests/integration_test.rs` starts a real `tiny-rca`, which spawns
both backends, and drives the whole chain over real JSON-RPC — a trace in, a
method, file and line out. It skips the backend cases when the binaries are not
built; build them with `--release` to exercise them.

## Why

An agent handed a trace id and a repository will guess. It will grep the codebase
for the exception type, find nothing, and report that the error was not
reproducible in source. The mapping from an OTLP hop to a method exists and is
cheap to compute — the route table is in the code — but doing it well means
knowing which service owns which route, which only a real parse gives you.

tiny-rca is the small amount of glue that makes that mapping available over a
protocol an agent already speaks, and that refuses to answer rather than answering
confidently when a backend is missing.

## License

MIT OR Apache-2.0