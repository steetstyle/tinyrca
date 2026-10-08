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
┌───────────────────┐
│     tiny-rca      │   orchestrates; owns no analysis
└─────┬───────┬─────┘
      │ MCP over stdio
 ┌────┴──┐  ┌──┴────────┐  ┌───────────────┐
 ▼       ▼  ▼           ▼  ▼               ▼
┌──────────┐ ┌──────────────┐ ┌──────────────────────┐
│ tinylink │ │ tiny-pdg-cs  │ │ Telemetry.Mcp.Server │
│ trace→code│ │ callers, PDG │ │ live telemetry, and  │
└──────────┘ └──────────────┘ │ the runtime path     │
                              │ catalog              │
                              └──────────────────────┘
```

All three are optional and independent. None is reimplemented here: route
extraction lives in tiny-pdg-cs, the trace→code join in tinylink, and reading the
deployment's own telemetry database in the .NET server. A copy would drift from
the original, and a drifted route table is worse than no route table.

The third one is a different kind of dependency. The first two are built from the
source you are investigating. The telemetry server reads the database the running
services wrote — which is the only place a **runtime path catalog** exists. Which
branch of the code a request actually took is recorded by the service's own
instrumentation; a trace capture cannot reconstruct it, and neither can a static
analysis of the repository.

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
  --tinylink  ../tinylink/target/release/tinylink \
  --index     tinylink.db \
  --pdg       ../tiny-pdg-cs/target/release/tiny-pdg-cs \
  --telemetry ../unicpeak-analytics-api/Telemetry.Mcp.Server/bin/Debug/net10.0/Telemetry.Mcp.Server.dll
```

`--telemetry` takes a **.NET assembly**, not a native binary, and is launched as
`dotnet <dll>`. It also needs to reach the telemetry database, so it inherits your
`TELEMETRY_CLICKHOUSE_*` / `TELEMETRY_POSTGRESQL_*` environment; point those at the
database you are investigating. The assembly path comes from a build, so a
`--telemetry-url` is usually easier once the server is deployed.

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
| `telemetry_call` | Any of the telemetry database's own read-only tools, passed through |
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

## The telemetry database: runtime paths and live logs

The third backend is the only source of answers the other two cannot produce. Reached
through `telemetry_call`, which passes its arguments through unchanged:

```jsonc
{"name": "telemetry_call", "arguments": {
   "tool": "error_paths",
   "arguments": {"windowMinutes": 43200, "limit": 2}}}
```

```jsonc
{"provider": "ClickHouse", "windowMinutes": 43200, "returned": 2, "paths": [
  {"pathId": "34424", "scope": "Scope.Agency.Handle", "errorSpans": 2523,
   "exceptionType": "System.InvalidOperationException",
   "sharedWithHealthyPath": "8264",
   "distinguishingSteps": [],
   "sameStepsAsHealthyPath": true,
   "readNext": "This path takes the same steps as the busiest healthy path for its scope, so the difference is not control flow. Compare what those steps returned, and the trace's logs, rather than the path itself."}]}
```

That is the join the other two backends cannot make on their own. `investigate_error`
says *which line* threw; `error_paths` says *which path through the code* led there,
and which steps it takes that the busiest healthy path for the same scope does not.
`path_diff` then narrows that to the step where two paths part.

Worth knowing by name, because nothing else answers them:

| Tool | Answers |
| --- | --- |
| `error_paths` | which runtime paths ended in an exception, and how they differ from the healthy one |
| `path_diff` | the exact step where two paths part |
| `path_profile` | every distinct path one scope takes, busiest first |
| `new_paths` | behaviour that did not happen before — a new branch, a flag turned on, a regression |
| `find_by_baggage` | did these failures all belong to one tenant, flag or region |
| `endpoint_health` | error rate and latency percentiles per route over live traffic |
| `capture_coverage` | what is configured to be recorded, and when each table last got a row |

**`capture_coverage` is the one to call first.** A query that returns nothing is
ambiguous: nothing happened, the capture filter excluded it, the code is not
instrumented, or the path flusher has not run yet. Those four look identical from
the outside, and only one of them is about your application.

### Why a pass-through and not twelve mirrored tools

Mirroring them would put a second copy of every schema in this repository, and the
copy would be wrong the first time the telemetry server changed a parameter — while
still claiming to be authoritative. A pass-through cannot drift. All twelve tools are
read-only, so nothing reachable here writes.

### Two log stores, and the difference that matters

`search_logs` takes a `source`:

| `source` | Reads | Limitation |
| --- | --- | --- |
| `store` | OTLP payloads ingested into tinylink | only the traffic you captured; body is the **unrendered template** |
| `database` | the deployment's telemetry database | needs the telemetry server; body **and rendered message** both searchable |

They are not merged. The database carries strictly more — an interpolated value like
a tenant id is findable as text there and is not in OTLP, because the .NET exporter
sends the template and puts the values in attributes. Merging them would produce rows
that sometimes have a rendered body and sometimes do not, which is a distinction the
caller cannot make after the fact.

`capabilities` reports both, with their availability, so an agent does not have to
guess which one to reach for.

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
  lib.rs        RcaServer: the MCP server, its seven tools, backend connections
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

**33 tests.** `tests/integration_test.rs` starts a real `tiny-rca`, which spawns
both backends, and drives the whole chain over real JSON-RPC — a trace in, a
method, file and line out. It skips the backend cases when the binaries are not
built; build them with `--release` to exercise them.

## Speaking to a .NET MCP server

If you point any of these backends at a .NET MCP server — and there are seven in
the unicpeak solution — the handshake fails on protocol version:

```text
UnsupportedProtocolVersionException: Protocol version '2026-07-28' is not
available through the initialize handshake.
({"requested":"2026-07-28","supported":["2024-11-05","2025-03-26","2025-06-18","2025-11-25"]})
```

rmcp 3.5 proposes `2026-07-28`; `ModelContextProtocol` 2.2.0 supports up to
`2025-11-25`. The .NET server does **not** negotiate down — an unrecognised version
fails outright — so no .NET MCP server is reachable at all, however it is addressed.

`PinnedVersion` in `backends.rs` proposes `LATEST_WITH_INITIALIZE` instead. Nothing is
lost: the Rust SDK implements it, and choosing a version is what the field is for. Raise
it when the .NET SDK catches up.

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