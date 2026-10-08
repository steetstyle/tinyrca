//! Tool implementations that span the two backend servers.
//!
//! Each tool orchestrates calls rather than analysing anything: the trace→code
//! join comes from tinylink, the call graph and PDG come from tiny-pdg-cs.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::backends::Backends;

type Result<T> = std::result::Result<T, String>;

// ───────────────────────── investigate_error ─────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct InvestigateArgs {
    /// OTLP trace: inline JSON or a path to a `.json` file.
    pub trace: String,
    /// Limit how many errored hops to report.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Also fetch the PDG of the failing method. Costs one extra round trip, so
    /// it is opt-in.
    #[serde(default)]
    pub include_dependencies: bool,
    /// Also fetch the logs of each failing trace.
    ///
    /// Off by default because it costs one round trip per failing trace, and most
    /// services do not export logs at all. When it is on and nothing comes back,
    /// the response says whether the store is empty or simply has no records for
    /// that trace — the two mean very different things.
    #[serde(default)]
    pub include_logs: bool,
    /// How many log records to attach per trace.
    #[serde(default)]
    pub log_limit: Option<usize>,
    /// Also fetch the runtime paths that ended in an exception, with the steps they
    /// take that the busiest healthy path for the same scope does not.
    ///
    /// Off by default because it costs two round trips per failing hop, and only
    /// the telemetry server can answer it. When it is on, each hop reports which
    /// exception type it threw and which paths ended that way.
    #[serde(default)]
    pub include_runtime_paths: bool,
}

/// The incident entry point: trace in, mapped hops out.
pub async fn investigate_error(backends: &Arc<Backends>, args: InvestigateArgs) -> Result<Value> {
    let link = backends.link_required("investigate_error")?;

    let limit = args.limit.unwrap_or(10).max(1);
    let raw = link
        .call(
            "explain_error",
            json!({"trace": args.trace, "limit": limit}),
        )
        .await?;

    let edges = raw["error_edges"].as_array().cloned().unwrap_or_default();

    // Enrich each hop with its endpoint callers, when the code server is up.
    // Best-effort: a failure here must not lose the primary answer.
    let mut investigated = Vec::new();
    for edge in &edges {
        let mut item = edge.clone();
        let target = item["to"]["method"].as_str().map(str::to_string);

        if let Some(method) = target.as_deref() {
            if let Some(pdg) = &backends.pdg {
                if pdg.has_tool("find_callers") {
                    match pdg
                        .call(
                            "find_callers",
                            json!({
                                "path": "",
                                "class": method.split('.').next().unwrap_or(""),
                                "method": method.split_once('.').map(|(_, m)| m).unwrap_or(""),
                            }),
                        )
                        .await
                    {
                        Ok(value) => {
                            item["registered_routes"] =
                                value.get("routes").cloned().unwrap_or(json!([]));
                        }
                        // The code server needs a source path per call; without one
                        // the lookup is not possible here. That is a configuration
                        // gap, not a finding, so it is reported as such.
                        Err(_) => {
                            item["registered_routes"] = json!([]);
                        }
                    }
                }
            }
        }

        if args.include_logs {
            // A hop's trace ids are what tie code to logs. The hop that carries a
            // trace id is the one whose logs explain the failure, so query per
            // trace rather than per hop.
            let traces: Vec<String> = item["trace_ids"]
                .as_array()
                .map(|ids| {
                    ids.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let log_limit = args.log_limit.unwrap_or(20).max(1);
            let mut collected = Vec::new();
            for trace in &traces {
                match search_logs(
                    backends,
                    SearchLogsArgs {
                        trace_id: Some(trace.clone()),
                        limit: Some(log_limit),
                        ..Default::default()
                    },
                )
                .await
                {
                    Ok(v) => collected.push(v),
                    Err(message) => collected.push(json!({ "error": message })),
                }
            }
            if !traces.is_empty() {
                item["logs"] = Value::Array(collected);
            } else {
                item["logs"] = json!(null);
            }
        }

        if args.include_runtime_paths {
            item["runtime_paths"] = match runtime_paths_for(backends, &item).await {
                Ok(value) => value,
                Err(message) => json!({ "error": message }),
            };
        }

        if args.include_dependencies {
            if let Some(method) = target.as_deref() {
                item["dependencies"] = match method_dependencies(
                    backends,
                    DependenciesArgs {
                        target: method.to_string(),
                        path: None,
                    },
                )
                .await
                {
                    Ok(v) => v,
                    Err(message) => json!({"error": message}),
                };
            }
        }

        investigated.push(item);
    }

    let mut steps = next_steps(&investigated);
    add_path_step(&mut steps, backends, &investigated);

    Ok(json!({
        "investigation": investigated,
        "error_edge_count": edges.len(),
        "total_edges": raw["total_edges"],
        "total_traces": raw["total_traces"],
        "error_traces": raw["error_traces"],
        "unavailable_backends": backends.missing_report().await,
        "next_steps": steps,
    }))
}

/// Resolve one failing hop to the runtime paths that ended in an exception.
///
/// This is the join that makes the telemetry server worth connecting. Everything above
/// this function works from a trace: which service, which method, which line. Nothing in
/// a trace says which *branch* of the code got there — that is recorded by the service's
/// own instrumentation and written to the path catalog, and exists nowhere else.
///
/// Two calls, because the join key has to be discovered rather than assumed. The first
/// reads the trace out of the telemetry database to learn what it actually threw; the
/// second finds every path that ended the same way and compares it against the busiest
/// healthy path for its scope. Filtering paths by the hop's method name would be the
/// obvious shortcut and it is wrong: catalog scopes are instrumentation scopes and do not
/// match method names.
async fn runtime_paths_for(backends: &Arc<Backends>, hop: &Value) -> Result<Value> {
    let telemetry = backends.telemetry_required("include_runtime_paths")?;

    let trace_id = hop["trace_ids"]
        .as_array()
        .and_then(|ids| ids.iter().filter_map(|v| v.as_str()).next())
        .map(str::to_string);

    let Some(trace_id) = trace_id else {
        return Ok(json!({
            "trace": Value::Null,
            "note": "This hop carries no trace id, so there is nothing to look up. The \
                     edge was mapped from the service graph rather than from a trace, \
                     which happens when the parent hop's trace stopped at a service \
                     boundary.",
        }));
    };

    let trace = telemetry
        .call(
            "explain_trace",
            json!({"traceId": trace_id, "maxSpans": 200}),
        )
        .await
        .map_err(|e| format!("could not read trace {trace_id} from the telemetry server: {e}"))?;

    // The dominant exception type is the join key. Most traces of a failing endpoint
    // throw the same thing, and the one that throws most is the one worth chasing;
    // a trace that throws several unrelated things has a dependency problem rather than
    // a code path problem, and that shows up as a list rather than a single name.
    let mut counts: Vec<(String, usize)> = Vec::new();
    for span in trace["flat"].as_array().into_iter().flatten() {
        let Some(kind) = span["exceptionType"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        match counts.iter_mut().find(|(k, _)| k == kind) {
            Some((_, n)) => *n += 1,
            None => counts.push((kind.to_string(), 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let exception_types: Vec<String> = counts.iter().map(|(k, _)| k.clone()).collect();
    let dominant = exception_types.first().cloned();

    let Some(exception_type) = dominant else {
        return Ok(json!({
            "trace": trace_id,
            "exceptionTypes": json!([]),
            "paths": json!([]),
            "note": "No span in this trace recorded an exception. Either the failure was \
                     a status code with nothing thrown, or the exporter's exception \
                     filter does not cover this type -- capture_coverage lists which \
                     exceptions are recorded.",
        }));
    };

    let paths = telemetry
        .call(
            "error_paths",
            json!({"windowMinutes": 43200, "exceptionType": exception_type, "limit": 5}),
        )
        .await
        .map_err(|e| format!("telemetry error_paths failed for {exception_type}: {e}"))?;

    let mut out = json!({
        "trace": trace_id,
        "exceptionType": exception_type,
        "exceptionTypes": exception_types,
        "pathIdsInThisTrace": trace["paths"]
            .as_array()
            .map(|p| p.iter().filter_map(|v| v["pathId"].as_str()).collect::<Vec<_>>()),
        "paths": paths["paths"].clone(),
        "windowMinutes": 43200,
    });

    if exception_types.len() > 1 {
        out["note"] = json!(
            "This trace threw more than one exception type. The paths below are for the \
             most frequent one; the others usually mean a dependency failed rather than \
             this endpoint's own code."
        );
    }

    // Say when the paths are not the ones this trace took. The catalog is across all
    // traffic, so "the paths that throw X" is a broader question than "the path this
    // request took", and an agent that does not know that will read too much into it.
    let in_trace = out["pathIdsInThisTrace"].as_array().cloned().unwrap_or_default();
    let matched = paths["paths"]
        .as_array()
        .map(|p| {
            p.iter().any(|path| {
                let wanted = path["pathId"].as_str().unwrap_or("");
                in_trace.iter().any(|t| *t == wanted)
            })
        })
        .unwrap_or(false);
    if !in_trace.is_empty() && !matched {
        out["note"] = json!(
            "None of these paths is one this trace took, so they are the general shape \
             of this failure rather than its specific route through the code. Use \
             path_diff with the trace's own path id to narrow it."
        );
    }

    Ok(out)
}

/// Tell the agent where to look next, so it does not have to guess a path
/// through the toolset.
fn next_steps(edges: &[Value]) -> Vec<String> {
    let mut steps = Vec::new();
    let first_method = edges
        .iter()
        .find_map(|e| e["to"]["method"].as_str())
        .filter(|m| !m.is_empty());

    match first_method {
        Some(method) => {
            steps.push(format!(
                "Call explain_method with target=\"{method}\" for its endpoints, callers and callees."
            ));
            steps.push(format!(
                "Call method_dependencies with target=\"{method}\" for the control and data dependences."
            ));
        }
        None => {
            steps.push(
                "No edge mapped to a method. Check capabilities: the code index may not \
                 cover the services in this trace."
                    .to_string(),
            );
        }
    }

    if edges
        .iter()
        .any(|e| e["to"]["confidence"].as_f64().unwrap_or(1.0) < 0.8)
    {
        steps.push(
            "At least one mapping has low confidence (below 0.8); verify the handler \
             before concluding."
                .to_string(),
        );
    }

    // Point at logs only when there is a trace to join on, otherwise the advice is
    // a dead end.
    let has_trace = edges
        .iter()
        .any(|e| e["trace_ids"].as_array().is_some_and(|ids| !ids.is_empty()));
    if has_trace {
        steps.push(
            "Call investigate_error with include_logs=true, or search_logs with the trace id, \
             to read what the failing request actually logged."
                .to_string(),
        );
    }
    steps
}

/// Add the runtime-path suggestion, but only when the telemetry server can answer it.
///
/// A next_steps entry that names a tool the server does not have is worse than no
/// entry: an agent will spend a turn finding that out.
fn add_path_step(steps: &mut Vec<String>, backends: &Arc<Backends>, edges: &[Value]) {
    let Some(telemetry) = &backends.telemetry else {
        return;
    };
    if !telemetry.has_tool("error_paths") || !telemetry.has_tool("explain_trace") {
        return;
    }
    if !edges.iter().any(|e| {
        e["trace_ids"]
            .as_array()
            .is_some_and(|ids| !ids.is_empty())
    }) {
        return;
    }
    steps.push(
        "Call investigate_error with include_runtime_paths=true: which branch of the code \
         this request actually took, and which steps it takes that the busiest healthy \
         path does not. Nothing in a trace records that -- it comes from the path \
         catalog the running service wrote."
            .to_string(),
    );
}

// ───────────────────────── explain_method ─────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ExplainMethodArgs {
    /// `Class.Method`, or a bare class name.
    pub target: String,
    /// Directory holding the sources, needed by the code-analysis server.
    pub path: Option<String>,
}

/// Endpoint reach, callers and callees for a method, from both backends.
pub async fn explain_method(backends: &Arc<Backends>, args: ExplainMethodArgs) -> Result<Value> {
    let mut out = json!({ "method": args.target, "sources": {} });

    // tinylink owns the index, so endpoint reachability comes from there.
    if let Ok(link) = backends.link_required("explain_method") {
        if link.has_tool("blast_radius") {
            match link
                .call(
                    "blast_radius",
                    json!({ "target": args.target, "service": Value::Null }),
                )
                .await
            {
                Ok(value) => out["endpoints_and_calls"] = value,
                Err(message) => out["endpoints_and_calls_error"] = json!(message),
            }
        }
    }

    // tiny-pdg-cs adds transitive callers and pattern context.
    if let Ok(pdg) = backends.pdg_required("explain_method") {
        if let Some(ref path) = args.path {
            if let Some((class, method)) = args.target.split_once('.') {
                if pdg.has_tool("find_callers") {
                    match pdg
                        .call(
                            "find_callers",
                            json!({ "path": path, "class": class, "method": method }),
                        )
                        .await
                    {
                        Ok(value) => out["transitive_callers"] = value,
                        Err(message) => out["transitive_callers_error"] = json!(message),
                    }
                }
            } else if pdg.has_tool("find_patterns") {
                match pdg.call("find_patterns", json!({"path": path})).await {
                    Ok(value) => out["patterns"] = value,
                    Err(message) => out["patterns_error"] = json!(message),
                }
            }
        } else {
            out["sources"]["note"] =
                json!("pass `path` to enable code-level analysis (callers, PDGs, patterns)");
        }
    }

    out["unavailable_backends"] = json!(backends.missing_report().await);
    Ok(out)
}

// ───────────────────────── method_dependencies ─────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DependenciesArgs {
    /// `Class.Method`.
    pub target: String,
    /// Directory holding the sources, required by the code-analysis server.
    #[serde(default)]
    pub path: Option<String>,
}

/// PDG for a method. Delegates to tiny-pdg-cs, which owns PDG construction.
pub async fn method_dependencies(
    backends: &Arc<Backends>,
    args: DependenciesArgs,
) -> Result<Value> {
    let pdg = backends.pdg_required("method_dependencies")?;

    let path = args.path.as_deref().ok_or_else(|| {
        format!(
            "method_dependencies needs `path` (the C# source directory). \
             {} has no configured source root; pass the directory to analyse.",
            pdg.name()
        )
    })?;

    let (class, method) = args.target.split_once('.').ok_or_else(|| {
        format!(
            "target must be Class.Method, got '{}' — \
             an unqualified name cannot be resolved to a PDG",
            args.target
        )
    })?;

    pdg.call(
        "method_pdg",
        json!({"path": path, "class": class, "method": method}),
    )
    .await
}

// ───────────────────────── capabilities ─────────────────────────

/// What is connected, what it can do, and what is missing.
pub async fn capabilities(backends: &Arc<Backends>) -> Result<Value> {
    let mut servers = Vec::new();

    match &backends.link {
        Some(link) => servers.push(json!({
            "server": link.name(),
            "connected": true,
            "tools": link.tools(),
            "role": "maps OpenTelemetry traces to indexed C# methods",
        })),
        None => servers.push(json!({
            "server": "tinylink",
            "connected": false,
            "role": "maps OpenTelemetry traces to indexed C# methods",
        })),
    }

    match &backends.pdg {
        Some(link) => servers.push(json!({
            "server": link.name(),
            "connected": true,
            "tools": link.tools(),
            "role": "static code analysis: callers, PDGs, routes, patterns",
        })),
        None => servers.push(json!({
            "server": "tiny-pdg-cs",
            "connected": false,
            "role": "static code analysis: callers, PDGs, routes, patterns",
        })),
    }

    match &backends.telemetry {
        Some(link) => servers.push(json!({
            "server": link.name(),
            "connected": true,
            "tools": link.tools(),
            "role": "the deployment's own telemetry database: live traces, logs, baggage, \
                     and the runtime path catalog",
            "unique_to_this_server": [
                "runtime paths: which branch of the code a request actually took, with \
                 file and line per step. Recorded by the running service's \
                 instrumentation; a trace capture cannot tell you this.",
                "path diffs between a failing path and the busiest healthy path for the \
                 same scope, which is what isolates the code that fails rather than the \
                 message it fails with.",
                "baggage scope: whether a set of failures shares a tenant, feature flag \
                 or region.",
                "endpoint health over live traffic, with latency percentiles.",
            ],
        })),
        None => servers.push(json!({
            "server": "telemetry",
            "connected": false,
            "role": "the deployment's own telemetry database: live traces, logs, baggage, \
                     and the runtime path catalog",
            "not_started_with": "--telemetry <dll> or --telemetry-url <url>",
            "what_is_missing": "Runtime paths and baggage scope. Which branch of the code \
                                a request took is recorded by the running service, so \
                                nothing in a trace capture can reconstruct it.",
        })),
    }

    // Log support needs the tinylink backend *and* a server new enough to expose
    // the tools. Reporting it as available on the strength of the connection alone
    // would have the agent call a tool that does not exist.
    let can_search_logs = match backends.link.as_ref() {
        Some(link) => link.has_tool("search_logs") && link.has_tool("ingest_logs"),
        None => false,
    };
    let can_search_database_logs = backends
        .telemetry
        .as_ref()
        .is_some_and(|link| link.has_tool("search_logs"));

    Ok(json!({
        "servers": servers,
        "unavailable": backends.missing_report().await,
        "can_map_traces_to_code": backends.link.is_some(),
        "can_analyse_code": backends.pdg.is_some(),
        "can_search_logs": can_search_logs,
        "can_search_database_logs": can_search_database_logs,
        "can_resolve_runtime_paths": backends.telemetry.is_some(),
        // Said up front because it is the step most likely to be missing: traces
        // arrive per call, but logs have to be loaded once before they can be
        // searched.
        // Two log stores, and the difference matters enough to state rather than let an
        // agent pick one at random.
        //
        //   source=store     OTLP payloads you ingested into tinylink. Available with no
        //                    database, but only covers traffic you captured.
        //   source=database  the deployment's telemetry database. Every log the service
        //                    wrote, and — unlike OTLP — the *rendered* message as well as
        //                    the template, so an interpolated value like a tenant id is
        //                    findable as text.
        //
        // They are not merged: the database has strictly more, and merging would mean
        // silently preferring whichever answered.
        "log_sources": {
            "store": {
                "available": can_search_logs,
                "what": "OTLP payloads ingested via ingest_logs, in tinylink's store",
                "limit": "only the traffic you captured; body is the unrendered template",
            },
            "database": {
                "available": can_search_database_logs,
                "what": "the telemetry database the running services write to",
                "limit": "needs the telemetry server; body and rendered message both \
                          searchable",
            },
        },
        "log_guidance": if can_search_logs || can_search_database_logs {
            "Pass source=store for captured OTLP payloads, or source=database for the \
             deployment's own logs. The database is the better source when it is \
             available: it carries the rendered message, which OTLP does not."
                .to_string()
        } else {
            "Log search is unavailable: it needs a tinylink server exposing ingest_logs \
             and search_logs, or a telemetry server."
                .to_string()
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn investigate_error_explains_the_missing_backend() {
        let backends = Arc::new(Backends::new());
        let err = investigate_error(
            &backends,
            InvestigateArgs {
                trace: "{}".into(),
                limit: None,
                include_dependencies: false,
                include_logs: false,
                log_limit: None,
                include_runtime_paths: false,
            },
        )
        .await
        .expect_err("no backend means no investigation");
        assert!(err.contains("investigate_error"), "{err}");
        assert!(err.contains("tinylink"), "{err}");
    }

    /// Without the telemetry server, the pass-through has to say how to turn it on and
    /// what is lost -- not just "unavailable".
    #[tokio::test]
    async fn telemetry_call_without_a_backend_says_how_to_start_it() {
        let backends = Arc::new(Backends::new());
        let err = telemetry_call(
            &backends,
            TelemetryCallArgs {
                tool: "error_paths".into(),
                arguments: Default::default(),
            },
        )
        .await
        .expect_err("no telemetry server, so no paths");

        assert!(err.contains("--telemetry"), "{err}");
        assert!(
            err.contains("only source of runtime paths"),
            "the error has to say what is missing, not just how to fix the flag: {err}"
        );
    }

    /// A server that was asked for and did not come up is a different problem from one
    /// nobody asked for, and the message should not pretend otherwise.
    #[tokio::test]
    async fn telemetry_call_distinguishes_not_started_from_failed_to_start() {
        let backends = Arc::new(Backends::new());
        backends
            .note_missing("telemetry (runtime paths, live logs): spawn failed".into())
            .await;
        let err = telemetry_call(
            &backends,
            TelemetryCallArgs {
                tool: "error_paths".into(),
                arguments: Default::default(),
            },
        )
        .await
        .expect_err("still no server");

        assert!(
            err.contains("requested but did not start"),
            "{err}"
        );
        assert!(
            !err.contains("--telemetry <dll>"),
            "a server that failed to start is not fixed by re-adding the flag: {err}"
        );
    }

    #[tokio::test]
    async fn capabilities_reports_nothing_connected() {
        let backends = Arc::new(Backends::new());
        let out = capabilities(&backends).await.expect("capabilities");
        assert_eq!(out["can_map_traces_to_code"], false);
        assert_eq!(out["can_analyse_code"], false);
        let servers = out["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 3, "{out}");
        assert!(servers.iter().all(|s| s["connected"] == false), "{out}");

        // The telemetry entry has to be present while disconnected, and has to say how
        // to turn it on. An agent reading capabilities on a bare instance should learn
        // that runtime paths are missing *and* that nothing it has can replace them.
        let telemetry = servers
            .iter()
            .find(|s| s["server"] == "telemetry")
            .expect("telemetry entry");
        assert!(
            telemetry["not_started_with"]
                .as_str()
                .unwrap()
                .contains("--telemetry"),
            "{telemetry}"
        );
        assert!(
            telemetry["what_is_missing"]
                .as_str()
                .unwrap()
                .contains("Runtime paths"),
            "{telemetry}"
        );

        // Both log sources are reported as unavailable rather than one being silently
        // preferred, and the database's advantage over OTLP is stated while it is off.
        assert_eq!(out["log_sources"]["store"]["available"], false, "{out}");
        assert_eq!(out["log_sources"]["database"]["available"], false, "{out}");
        assert_eq!(out["can_resolve_runtime_paths"], false, "{out}");
        assert_eq!(out["can_search_database_logs"], false, "{out}");
    }

    #[tokio::test]
    async fn capabilities_lists_missing_servers_with_their_reason() {
        let backends = Arc::new(Backends::new());
        backends
            .note_missing("tinylink binary not found at /x".into())
            .await;
        let out = capabilities(&backends).await.unwrap();
        let unavailable = out["unavailable"].as_array().unwrap();
        assert_eq!(unavailable.len(), 1);
        assert!(unavailable[0].as_str().unwrap().contains("/x"));
    }

    #[tokio::test]
    async fn method_dependencies_needs_the_code_server_first() {
        let backends = Arc::new(Backends::new());
        // With no code server the first failure is the missing server, not the
        // missing path — an agent must be told which dependency to fix first.
        let err = method_dependencies(
            &backends,
            DependenciesArgs {
                target: "A.B".into(),
                path: None,
            },
        )
        .await
        .expect_err("no code server means no PDG");
        assert!(err.contains("tiny-pdg-cs"), "{err}");
    }

    #[tokio::test]
    async fn explain_method_works_with_no_backends_and_says_so() {
        let backends = Arc::new(Backends::new());
        let out = explain_method(
            &backends,
            ExplainMethodArgs {
                target: "A.B".into(),
                path: None,
            },
        )
        .await
        .expect("explain_method should still answer");
        assert_eq!(out["method"], "A.B");
        // An agent must be able to tell "no data" from "server broken".
        assert!(out["unavailable_backends"].is_array());
    }

    #[test]
    fn next_steps_point_at_the_discovered_method() {
        let edges = vec![json!({
            "to": {"method": "GetMembersEndpoint.Handler", "confidence": 0.95}
        })];
        let steps = next_steps(&edges);
        assert!(
            steps
                .iter()
                .any(|s| s.contains("GetMembersEndpoint.Handler")),
            "{steps:?}"
        );
        assert!(steps.iter().any(|s| s.contains("explain_method")));
        assert!(steps.iter().any(|s| s.contains("method_dependencies")));
    }

    #[test]
    fn next_steps_flag_low_confidence_mappings() {
        let edges = vec![json!({
            "to": {"method": "Something.Handler", "confidence": 0.6}
        })];
        let steps = next_steps(&edges);
        assert!(
            steps.iter().any(|s| s.contains("low confidence")),
            "a low-confidence mapping must be flagged: {steps:?}"
        );
    }

    #[test]
    fn next_steps_explain_an_unmapped_trace() {
        let steps = next_steps(&[json!({"to": {"method": Value::Null}})]);
        assert!(
            steps.iter().any(|s| s.contains("capabilities")),
            "with nothing mapped, suggest checking capabilities: {steps:?}"
        );
    }
}

// ───────────────────────── telemetry pass-through ─────────────────────────

/// Arguments for reaching a telemetry-database tool directly.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TelemetryCallArgs {
    /// Which telemetry tool to call. One of the twelve the server exposes; run
    /// `capabilities` for the list as the server actually reports it.
    pub tool: String,
    /// That tool's own arguments, passed through unchanged.
    #[serde(default)]
    pub arguments: serde_json::Map<String, Value>,
}

/// Call one of the telemetry database's own tools.
///
/// A pass-through rather than twelve re-declared tools, on purpose. Mirroring them would
/// put a second copy of every schema in this repository, and the copy would be wrong the
/// first time the telemetry server changed a parameter — while still claiming to be
/// authoritative. A pass-through cannot drift.
///
/// All twelve telemetry tools are read-only, so nothing here writes. `find_by_baggage`
/// looks like it might ingest something; it only queries. Writing telemetry is the
/// application's job.
///
/// The ones worth knowing by name, because they answer questions nothing else can:
///
/// - `error_paths` — runtime paths that ended in an exception, with the steps they take
///   that the busiest healthy path for the same scope does not
/// - `path_diff` / `path_profile` / `new_paths` — what a path does, how two differ, what
///   is new since a date
/// - `find_by_baggage` — which traces carried a tenant or feature flag, and how they fared
/// - `endpoint_health` — error rate and latency percentiles per route over live traffic
/// - `capture_coverage` — what is configured to be recorded and when each table last got
///   a row; call this when anything else returns nothing
pub async fn telemetry_call(
    backends: &Arc<Backends>,
    args: TelemetryCallArgs,
) -> Result<Value> {
    let telemetry = backends.telemetry_required(&args.tool)?;
    if !telemetry.has_tool(&args.tool) {
        let mut names = telemetry.tools().to_vec();
        names.sort();
        return Err(format!(
            "the telemetry server has no tool '{}'. It offers: {}",
            args.tool,
            names.join(", ")
        ));
    }
    telemetry.call(&args.tool, Value::Object(args.arguments)).await
}

// ───────────────────────── search_logs ─────────────────────────

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchLogsArgs {
    /// Restrict to one trace. Case-insensitive.
    pub trace_id: Option<String>,
    /// Restrict to one correlation id (database source only).
    pub correlation_id: Option<String>,
    /// Restrict to one service, exactly as the exporter named it.
    pub service: Option<String>,
    /// Restrict to one exception type, by full type name (database source only).
    pub exception_type: Option<String>,
    /// Minimum severity: `Trace`, `Debug`, `Info`, `Warn`, `Error`, `Fatal`.
    pub min_severity: Option<String>,
    /// Case-insensitive substring, matched against the body and attribute values.
    pub contains: Option<String>,
    /// Search attribute values only, ignoring the body.
    pub attributes_only: Option<bool>,
    /// Only records having an attribute with this key.
    pub attribute_key: Option<String>,
    /// Inclusive lower bound, Unix nanoseconds.
    pub since_unix_nano: Option<u64>,
    /// Exclusive upper bound, Unix nanoseconds.
    pub until_unix_nano: Option<u64>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// Which store to read: `store` for OTLP payloads ingested into tinylink,
    /// `database` for the deployment's telemetry database. Defaults to `store`.
    ///
    /// Not merged, on purpose. The database carries the rendered message as well as
    /// the template; OTLP only ever carries the template. Merging them would mean one
    /// row sometimes having a rendered body and sometimes not, which is a distinction
    /// the caller cannot make afterwards.
    pub source: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct IngestLogsArgs {
    /// An OTLP payload file, or a directory with one payload per file.
    pub source: String,
}

/// Search the log store that `tinylink` maintains.
///
/// This is a pass-through, and deliberately so: the filters, the paging and the
/// fields that explain a short answer are all decided in one place, in tinylink,
/// where they can be tested against a real exporter capture. Re-deriving any of
/// that here would be a second implementation to keep in step with the first.
pub async fn search_logs(backends: &Arc<Backends>, args: SearchLogsArgs) -> Result<Value> {
    let source = args.source.as_deref().unwrap_or("store");

    match source {
        "store" => search_logs_in_store(backends, args).await,
        "database" => search_logs_in_database(backends, args).await,
        other => {
            return Err(format!(
                "unknown log source '{other}'. Use 'store' for OTLP payloads ingested \
                 into tinylink, or 'database' for the deployment's telemetry database. \
                 Run capabilities to see which are available."
            ))
        }
    }
}

/// Search the OTLP payloads ingested into tinylink's store.
async fn search_logs_in_store(
    backends: &Arc<Backends>,
    args: SearchLogsArgs,
) -> Result<Value> {
    let link = backends.link_required("search_logs")?;

    let mut payload = serde_json::Map::new();
    let mut insert = |key: &str, value: Option<Value>| {
        if let Some(v) = value {
            payload.insert(key.to_string(), v);
        }
    };
    insert("trace_id", args.trace_id.map(Value::String));
    insert("service", args.service.map(Value::String));
    insert("min_severity", args.min_severity.map(Value::String));
    insert("contains", args.contains.map(Value::String));
    insert("attributes_only", args.attributes_only.map(Value::Bool));
    insert("attribute_key", args.attribute_key.map(Value::String));
    insert(
        "since_unix_nano",
        args.since_unix_nano.map(|v| json!(v.to_string())),
    );
    insert(
        "until_unix_nano",
        args.until_unix_nano.map(|v| json!(v.to_string())),
    );
    insert("limit", args.limit.map(|v| json!(v)));
    insert("offset", args.offset.map(|v| json!(v)));

    let mut out = link.call("search_logs", Value::Object(payload)).await?;
    out["source"] = json!("store");
    // Say plainly when the store holds nothing, because "no logs found" and "logs
    // were never ingested" lead to opposite conclusions.
    if out["store_records"] == json!(0) {
        out["note"] = json!(
            "The log store is empty. Ingest OTLP payloads with `tinylink logs-ingest <path>` \
             or the tinylink ingest_logs tool before searching, or use \
             source=database for the deployment's own logs."
        );
    }
    Ok(out)
}

/// Search the telemetry database the running services write to.
///
/// The parameter names are translated rather than passed through: a caller who learned
/// `trace_id` and `min_severity` from the OTLP store should not have to learn a second
/// vocabulary for the same question, and the telemetry server's own tool names are not
/// an agent-facing contract.
async fn search_logs_in_database(
    backends: &Arc<Backends>,
    args: SearchLogsArgs,
) -> Result<Value> {
    let telemetry = backends.telemetry_required("search_logs")?;

    // The database stores nanoseconds as a plain integer column and takes the window as
    // minutes, so a caller who gave absolute bounds gets them honoured where they can
    // be. Without a bound the query is unbounded, which on a real deployment is the
    // difference between an answer and a scan.
    let window_minutes = args
        .since_unix_nano
        .map(|since| {
            let elapsed = now_unix_nanos().saturating_sub(since) / 60_000_000_000;
            elapsed.clamp(1, 60 * 24 * 30) as i64
        })
        .unwrap_or(60 * 24);

    let level = args.min_severity.as_deref().and_then(severity_to_database_name);

    let payload = json!({
        "windowMinutes": window_minutes,
        "limit": args.limit.unwrap_or(50),
        "offset": args.offset.unwrap_or(0),
        "traceId": args.trace_id,
        "correlationId": args.correlation_id,
        "service": args.service,
        "text": args.contains,
        "exceptionType": args.exception_type,
        "minimumLevel": level,
    });

    let mut out = telemetry.call("search_logs", payload).await?;

    // The database's rows are a different shape from the store's, so the answer says
    // which shape it is rather than leaving the caller to notice that `body` is missing.
    out["source"] = json!("database");
    out["note"] = json!(
        "Rows come from the telemetry database, not the OTLP store. Each carries \
         `message` (the template the service logged) and `formattedMessage` (the \
         rendered text), where an OTLP record only ever has the template."
    );
    Ok(out)
}

/// Translate an OTLP severity name into the name the database store uses.
///
/// Not a rename: the two vocabularies genuinely differ — OTLP says `info`, `warn` and
/// `fatal` where the .NET logging levels say `information`, `warning` and `critical` —
/// so this is the one place the difference is written down. An unrecognised name
/// returns None and is dropped, which leaves the query unfiltered rather than guessing a
/// threshold: a wrong floor hides logs, and no floor shows them.
fn severity_to_database_name(name: &str) -> Option<&'static str> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "trace" => "trace",
        "debug" => "debug",
        "info" | "information" => "information",
        "warn" | "warning" => "warning",
        "error" => "error",
        "fatal" | "critical" => "critical",
        _ => return None,
    })
}

fn now_unix_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Ingest OTLP log payloads into tinylink's log store.
///
/// Bridges to tinylink rather than opening the store here: the schema, the
/// migration and the payload sniffing live with the code that owns the file, and a
/// second writer would have to agree with all of it.
pub async fn ingest_logs(backends: &Arc<Backends>, args: IngestLogsArgs) -> Result<Value> {
    let link = backends.link_required("ingest_logs")?;
    if !link.has_tool("ingest_logs") {
        return Err("the connected tinylink server predates log support; rebuild it".to_string());
    }
    link.call("ingest_logs", json!({ "source": args.source }))
        .await
}
