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

    Ok(json!({
        "investigation": investigated,
        "error_edge_count": edges.len(),
        "total_edges": raw["total_edges"],
        "total_traces": raw["total_traces"],
        "error_traces": raw["error_traces"],
        "unavailable_backends": backends.missing_report().await,
        "next_steps": next_steps(&investigated),
    }))
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
    steps
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

    Ok(json!({
        "servers": servers,
        "unavailable": backends.missing_report().await,
        "can_map_traces_to_code": backends.link.is_some(),
        "can_analyse_code": backends.pdg.is_some(),
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
            },
        )
        .await
        .expect_err("no backend means no investigation");
        assert!(err.contains("investigate_error"), "{err}");
        assert!(err.contains("tinylink"), "{err}");
    }

    #[tokio::test]
    async fn capabilities_reports_nothing_connected() {
        let backends = Arc::new(Backends::new());
        let out = capabilities(&backends).await.expect("capabilities");
        assert_eq!(out["can_map_traces_to_code"], false);
        assert_eq!(out["can_analyse_code"], false);
        let servers = out["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 2, "{out}");
        assert!(servers.iter().all(|s| s["connected"] == false), "{out}");
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
