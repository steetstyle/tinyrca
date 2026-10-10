//! MCP server for root-cause analysis of .NET microservices.
//!
//! # What this is
//!
//! An agent investigating a production incident has to answer, in order: which
//! request failed, which service and method handled it, what that code calls, and
//! what the logs said. The data for those questions lives in three different
//! places — OTLP traces, logs, and the C# source.
//!
//! This server holds MCP clients to two other servers and answers the questions
//! that span them:
//!
//! | Server | Owns |
//! | --- | --- |
//! | [`tinylink`] | trace → method/file/line mapping, with confidence |
//! | [`tiny-pdg-cs`] | callers, PDGs, routes, pattern detection |
//!
//! It deliberately owns no analysis of its own. A third implementation of route
//! extraction or PDG construction would drift from the other two.
//!
//! [`tinylink`]: https://github.com/steetstyle/tinylink
//! [`tiny-pdg-cs`]: https://github.com/steetstyle/tinypdg-cs

pub mod backends;
pub mod tools;

use std::path::PathBuf;
use std::sync::Arc;

use backends::Backends;
use rmcp::{
    handler::server::router::tool::ToolRouter, handler::server::wrapper::Parameters,
    model::ServerConfig, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};

pub struct RcaServer {
    tool_router: ToolRouter<RcaServer>,
    backends: Arc<Backends>,
}

#[derive(Debug, Clone, clap::Parser)]
pub struct ServeArgs {
    /// tinylink binary (`tinylink serve --index ...`)
    #[arg(long, value_name = "PATH")]
    pub tinylink: Option<PathBuf>,
    /// tinylink index database, required when --tinylink is given
    #[arg(long, value_name = "PATH")]
    pub index: Option<PathBuf>,
    /// URL of a tinylink server already running over Streamable HTTP
    #[arg(long, value_name = "URL")]
    pub tinylink_url: Option<String>,
    /// tiny-pdg-cs binary (`tiny-pdg-cs serve`)
    #[arg(long, value_name = "PATH")]
    pub pdg: Option<PathBuf>,
    /// URL of a tiny-pdg-cs server already running over Streamable HTTP
    #[arg(long, value_name = "URL")]
    pub pdg_url: Option<String>,
    /// Telemetry.Mcp.Server assembly, run as a stdio child. Reads the deployment's
    /// own telemetry database, which is the only source of runtime paths.
    #[arg(long, value_name = "DLL")]
    pub telemetry: Option<PathBuf>,
    /// URL of a Telemetry.Mcp.Server already running over Streamable HTTP
    #[arg(long, value_name = "URL")]
    pub telemetry_url: Option<String>,
    /// Directory containing the C# sources, for the code-analysis server
    #[arg(long, value_name = "DIR")]
    pub sources: Option<PathBuf>,
    /// Serve tiny-rca over Streamable HTTP instead of stdio
    #[arg(long)]
    pub http: bool,
    /// Bind address for --http (loopback by default; no authentication)
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: std::net::IpAddr,
    #[arg(long, default_value = "8080")]
    pub port: u16,
    #[arg(long, default_value = "mcp")]
    pub path: String,
    /// Fail at startup if a requested server cannot be reached
    #[arg(long)]
    pub strict: bool,
}

impl ServeArgs {
    /// How to reach tinylink, if requested at all.
    fn tinylink_endpoint(&self) -> Option<backends::Endpoint> {
        if let Some(url) = self.tinylink_url.as_ref() {
            return Some(backends::Endpoint::Http {
                url: url.as_str().into(),
            });
        }
        self.tinylink
            .as_ref()
            .map(|binary| backends::Endpoint::Stdio {
                binary: binary.clone(),
                env: Vec::new(),
                args: {
                    let mut args = vec!["serve".to_string()];
                    if let Some(ref index) = self.index {
                        args.push("--index".to_string());
                        args.push(index.display().to_string());
                    }
                    args
                },
            })
    }

    /// How to reach the telemetry server, if requested at all.
    ///
    /// Over stdio it is launched as `dotnet <dll>`, which is why the argument is an
    /// assembly path rather than a native binary: the MCP server is a .NET program and
    /// asking for the managed assembly keeps the caller from having to know where the
    /// host is installed.
    fn telemetry_endpoint(&self) -> Option<backends::Endpoint> {
        if let Some(url) = self.telemetry_url.as_ref() {
            return Some(backends::Endpoint::Http {
                url: url.as_str().into(),
            });
        }
        self.telemetry
            .as_ref()
            .map(|dll| backends::Endpoint::Stdio {
                binary: std::path::PathBuf::from("dotnet"),
                args: vec![dll.display().to_string()],
                env: Vec::new(),
            })
    }

    fn pdg_endpoint(&self) -> Option<backends::Endpoint> {
        if let Some(url) = self.pdg_url.as_ref() {
            return Some(backends::Endpoint::Http {
                url: url.as_str().into(),
            });
        }
        self.pdg.as_ref().map(|binary| backends::Endpoint::Stdio {
            binary: binary.clone(),
            env: Vec::new(),
            // tiny-pdg-cs takes the source path per call, so nothing to pass here.
            args: vec!["serve".to_string()],
        })
    }
}

impl RcaServer {
    pub fn new(backends: Arc<Backends>) -> Self {
        Self {
            tool_router: Self::tool_router(),
            backends,
        }
    }

    /// Start the servers named on the command line.
    ///
    /// A server that fails to start is recorded and reported by the tools rather
    /// than aborting: an agent running without the code index can still answer
    /// questions from traces alone, which beats not starting at all. Pass
    /// `--strict` to make any failure fatal instead.
    pub async fn connect(args: ServeArgs) -> anyhow::Result<Self> {
        let mut backends = Backends::new();
        let mut notes: Vec<String> = Vec::new();

        if let Some(endpoint) = args.tinylink_endpoint() {
            match backends::McpLink::connect("tinylink", endpoint).await {
                Ok(link) => {
                    tracing::info!(tools = ?link.tools(), "tinylink connected");
                    backends.link = Some(Arc::new(link));
                }
                Err(e) if args.strict => return Err(e),
                Err(e) => {
                    tracing::warn!("tinylink unavailable: {e:#}");
                    notes.push(format!("tinylink (trace→code linking): {e:#}"));
                }
            }
        }

        if let Some(endpoint) = args.pdg_endpoint() {
            match backends::McpLink::connect("tiny-pdg-cs", endpoint).await {
                Ok(link) => {
                    tracing::info!(tools = ?link.tools(), "tiny-pdg-cs connected");
                    backends.pdg = Some(Arc::new(link));
                }
                Err(e) if args.strict => return Err(e),
                Err(e) => {
                    tracing::warn!("tiny-pdg-cs unavailable: {e:#}");
                    notes.push(format!("tiny-pdg-cs (static analysis): {e:#}"));
                }
            }
        }

        if let Some(endpoint) = args.telemetry_endpoint() {
            match backends::McpLink::connect("telemetry", endpoint).await {
                Ok(link) => {
                    tracing::info!(tools = ?link.tools(), "telemetry connected");
                    backends.telemetry = Some(Arc::new(link));
                }
                Err(e) if args.strict => return Err(e),
                Err(e) => {
                    tracing::warn!("telemetry unavailable: {e:#}");
                    notes.push(format!("telemetry (runtime paths, live logs): {e:#}"));
                }
            }
        }

        for note in notes {
            backends.note_missing(note).await;
        }
        Ok(Self::new(Arc::new(backends)))
    }

    /// Serve over Streamable HTTP until the process is stopped.
    ///
    /// Stdio is the default because an MCP client launches the binary; HTTP is
    /// for shared or remote deployments. Same six tools either way.
    pub async fn serve_http(self, args: ServeArgs) -> anyhow::Result<()> {
        use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
        use rmcp::transport::streamable_http_server::tower::StreamableHttpService;

        let addr = std::net::SocketAddr::new(args.bind, args.port);
        if !args.bind.is_loopback() {
            tracing::warn!(
                "binding {addr} on a non-loopback address without authentication; \
                 put an authenticating proxy in front before exposing it"
            );
        }

        let path = if args.path.starts_with('/') {
            args.path.clone()
        } else {
            format!("/{}", args.path)
        };

        let shared = Arc::clone(&self.backends);
        let service: StreamableHttpService<RcaServer, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(RcaServer::new(Arc::clone(&shared))),
                Default::default(),
                Default::default(),
            );

        let router = axum::Router::new().nest_service(&path, service);
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| anyhow::anyhow!("cannot bind {addr}: {e}"))?;
        let local = listener.local_addr().unwrap_or(addr);
        // stderr, not stdout: stdout belongs to the stdio transport.
        eprintln!("tiny-rca MCP listening on http://{local}{path} (streamable HTTP)");

        axum::serve(listener, router)
            .await
            .map_err(|e| anyhow::anyhow!("server stopped: {e}"))
    }
}

#[tool_router(router = tool_router)]
impl RcaServer {
    #[tool(
        name = "investigate_error",
        description = "Start a root-cause investigation of a failed request. Give it an OTLP trace (inline JSON or a file path) and it returns every errored hop mapped to the handling method, file and line, plus the HTTP endpoint and how confident the mapping is. Start every incident here. Set include_runtime_paths=true to also get the runtime paths that ended in an exception, with the steps they take that the busiest healthy path for the same scope does not. Nothing in a trace records which branch of the code a request took; that comes from the path catalog the running service wrote, so it needs the telemetry server.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub async fn investigate_error(
        &self,
        args: Parameters<tools::InvestigateArgs>,
    ) -> Result<String, McpError> {
        tools::investigate_error(&self.backends, args.0)
            .await
            .map(render)
            .map_err(to_mcp)
    }

    #[tool(
        name = "explain_method",
        description = "Given Class.Method, report its file and line range, the HTTP endpoints that reach it, and its callers and callees. Use after investigate_error to move from \"this method failed\" to \"what does this method touch\". Set path to a directory, or to gh:owner/repo@ref and the code server will fetch the repository from GitHub and cache the checkout, which is what to do when the sources are not on this machine. This server fetches nothing: the specifier is passed through to the code server unchanged.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn explain_method(
        &self,
        args: Parameters<tools::ExplainMethodArgs>,
    ) -> Result<String, McpError> {
        tools::explain_method(&self.backends, args.0)
            .await
            .map(render)
            .map_err(to_mcp)
    }

    #[tool(
        name = "method_dependencies",
        description = "Program dependence graph for a method: basic blocks with line ranges plus control and data dependences. Use to reason about which statements a failing branch depends on. Set path to a directory, or to gh:owner/repo@ref and the code server will fetch the repository from GitHub and cache the checkout, which is what to do when the sources are not on this machine. This server fetches nothing: the specifier is passed through to the code server unchanged.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn method_dependencies(
        &self,
        args: Parameters<tools::DependenciesArgs>,
    ) -> Result<String, McpError> {
        tools::method_dependencies(&self.backends, args.0)
            .await
            .map(render)
            .map_err(to_mcp)
    }

    /// Nested single-entry, single-exit regions of a method.
    #[tool(
        name = "method_hammocks",
        description = "Restructure a method into hammock blocks: nested single-entry, single-exit regions, where a loop body sits inside a conditional inside the method. Returns a containment forest, so each region names its parent and its depth — which is what lets a traversal zoom back out after it has gone too deep. Prefer this to method_dependencies when the question is which structured region a symptom falls in: far fewer and larger regions, and the parent link is what a flat PDG cannot give. Set path to a directory, or to gh:owner/repo@ref and the code server will fetch the repository from GitHub and cache the checkout, which is what to do when the sources are not on this machine. This server fetches nothing: the specifier is passed through to the code server unchanged.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn method_hammocks(
        &self,
        args: Parameters<tools::HammocksArgs>,
    ) -> Result<String, McpError> {
        tools::method_hammocks(&self.backends, args.0)
            .await
            .map(render)
            .map_err(to_mcp)
    }

    /// Compare two versions and list the commits behind the change.
    #[tool(
        name = "diff_impact",
        description = "Compare two versions of a codebase: what changed, which of those changes reach further, and the commits between the two versions with their messages. Give both arguments as gh:owner/repo@ref and the answer comes back with the commit list -- sha, subject and body, author, date -- because a caller that disappeared is a fact and the commit that removed it is the explanation.\n\nPass a branch and a commit, or a commit and its parent. Local directories also work and give the call-graph diff, but no history: they answer \"what changed\", not \"why\". `class` and `method` narrow the impact graph to one target; omit them and every changed method is traced, capped at 25 targets and reported as capped.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub async fn diff_impact(
        &self,
        args: Parameters<tools::DiffImpactArgs>,
    ) -> Result<String, McpError> {
        tools::diff_impact(&self.backends, args.0)
            .await
            .map(render)
            .map_err(to_mcp)
    }

    /// Search the log store.
    #[tool(
        name = "search_logs",
        description = "Search logs by trace id, service, minimum severity, free text or attribute key. Free text matches attribute values as well as the body, which matters because the .NET exporter sends the unrendered message template as the body. Combine it with a trace id from investigate_error to read what a failing request actually logged.\n\nTwo stores, chosen with `source`: 'store' reads OTLP payloads you ingested into tinylink and covers only the traffic you captured; 'database' reads the deployment's own telemetry database and carries the rendered message as well as the template, so an interpolated value like a tenant id is findable as text. They are not merged. Run capabilities to see which are available.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn search_logs(
        &self,
        args: Parameters<tools::SearchLogsArgs>,
    ) -> Result<String, McpError> {
        tools::search_logs(&self.backends, args.0)
            .await
            .map(render)
            .map_err(to_mcp)
    }

    /// Load OTLP log payloads.
    #[tool(
        name = "ingest_logs",
        description = "Read OTLP log payloads (protobuf or JSON, gzipped or not) into the log store that search_logs queries. Takes a payload file or a directory with one payload per file, and reports what failed to load.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn ingest_logs(
        &self,
        args: Parameters<tools::IngestLogsArgs>,
    ) -> Result<String, McpError> {
        tools::ingest_logs(&self.backends, args.0)
            .await
            .map(render)
            .map_err(to_mcp)
    }

    #[tool(
        name = "telemetry_call",
        description = "Call one of the telemetry database's own read-only tools and get its answer verbatim.\n\nUse this for anything about live telemetry that the trace and code tools cannot answer: runtime paths (`error_paths`, `path_diff`, `path_profile`, `new_paths`), baggage scope (`find_by_baggage`), route health (`endpoint_health`), or what is being recorded at all (`capture_coverage`). Run `capabilities` first to see which are available and what each server contributes.\n\nNot a convenience wrapper: the arguments are the telemetry tool's own, passed through unchanged, so nothing here can drift from what that server accepts.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn telemetry_call(
        &self,
        args: Parameters<tools::TelemetryCallArgs>,
    ) -> Result<String, McpError> {
        tools::telemetry_call(&self.backends, args.0)
            .await
            .map(render)
            .map_err(to_mcp)
    }

    #[tool(
        name = "capabilities",
        description = "Report which analysis servers are connected, what tools they expose, and what is unavailable. Use this to find out whether code-level analysis is possible before planning an investigation.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn capabilities(&self) -> Result<String, McpError> {
        tools::capabilities(&self.backends)
            .await
            .map(render)
            .map_err(to_mcp)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RcaServer {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::default();
        info.instructions = Some(
            "Root-cause analysis for .NET microservices. Combines OpenTelemetry traces and \
             logs with the C# source to answer \"why did this request fail\". Every tool is \
             read-only except ingest_logs, which writes into the log store. \
             \
             Typical flow: capabilities to see what is available, then investigate_error with \
             the failing trace to map each errored hop to a method, file and line with a \
             confidence score, then explain_method or method_dependencies on the result to \
             understand the code. Trace data is supplied per call — pass OTLP JSON inline or a \
             file path. \
             \
             Logs are not supplied per call: they are loaded once with ingest_logs (a payload \
             file, or a directory with one payload per file) and then queried with search_logs \
             or with investigate_error's include_logs. Note that the .NET exporter sends the \
             unrendered message template as the log body, so free-text search also covers \
             attribute values — the interpolated text is not in the body."
                .into(),
        );
        info.capabilities = rmcp::model::ServerCapabilities::builder()
            .enable_tools()
            .build();
        info
    }
}

/// Render a tool result as a JSON string so an agent can read it without
/// guessing a format.
fn render(value: serde_json::Value) -> String {
    value.to_string()
}

fn to_mcp(message: String) -> McpError {
    McpError::invalid_params(message, None)
}

/// The tool names `tiny-rca tools` prints.
///
/// Declared here and asserted against the router rather than written out again in
/// main.rs, because two hand-maintained lists of the same thing eventually disagree.
pub const LISTED_TOOLS: [&str; 9] = [
    "investigate_error",
    "explain_method",
    "method_dependencies",
    "method_hammocks",
    "diff_impact",
    "search_logs",
    "ingest_logs",
    "telemetry_call",
    "capabilities",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_registers_the_rca_tools() {
        let router = RcaServer::tool_router();
        let names: Vec<String> = router
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        for expected in [
            "investigate_error",
            "explain_method",
            "method_dependencies",
            "method_hammocks",
            "diff_impact",
            "search_logs",
            "ingest_logs",
            "telemetry_call",
            "capabilities",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
        assert_eq!(names.len(), 9, "unexpected tool set: {names:?}");
    }

    #[test]
    fn tools_are_described_and_flag_whether_they_write() {
        // `ingest_logs` writes to the log store. A client that gates approvals on
        // the annotation would otherwise let a state change through unprompted.
        const WRITES: [&str; 1] = ["ingest_logs"];

        let router = RcaServer::tool_router();
        for tool in router.list_all() {
            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{} has no annotations", tool.name));
            assert_eq!(
                annotations.read_only_hint,
                Some(!WRITES.contains(&tool.name.as_ref())),
                "{}",
                tool.name
            );
            assert!(tool.description.as_deref().unwrap().len() > 40);
        }
    }

    /// `tiny-rca tools` and the router must list the same tools.
    ///
    /// They are written out separately, so they can disagree, and they did:
    /// `diff_impact` reached the router and the test asserting nine tools passed while
    /// the CLI listed eight. The listing is what a reader consults to find out what to
    /// point an MCP client at, so a tool missing from it is a tool nobody finds.
    #[test]
    fn the_command_line_listing_matches_the_router() {
        let from_router: std::collections::BTreeSet<String> = RcaServer::tool_router()
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();

        let from_cli: std::collections::BTreeSet<String> =
            LISTED_TOOLS.iter().map(|t| t.to_string()).collect();

        assert_eq!(
            from_router, from_cli,
            "`tiny-rca tools` and the MCP router disagree"
        );
    }

    /// Every tool must say whether it writes, because a client that gates approvals on
    /// the annotation has nothing else to go on.
    #[test]
    fn tools_say_whether_they_change_anything() {
        // ingest_logs writes to the log store; diff_impact and the rest only read, and
        // diff_impact only reads because the repositories it fetches are cached -- a
        // fetch is a write to a cache directory, not to anything a caller owns.
        const WRITES: [&str; 1] = ["ingest_logs"];

        let router = RcaServer::tool_router();
        for tool in router.list_all() {
            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{} has no annotations", tool.name));
            assert_eq!(
                annotations.read_only_hint,
                Some(!WRITES.contains(&tool.name.as_ref())),
                "{}",
                tool.name
            );
            assert!(
                tool.description.as_deref().unwrap().len() > 40,
                "{}",
                tool.name
            );
        }
    }

    #[test]
    fn server_info_names_the_investigation_entry_point() {
        let server = RcaServer::new(Arc::new(Backends::new()));
        let info = server.get_info();
        assert!(info.capabilities.tools.is_some());
        let instructions = info.instructions.expect("instructions");
        assert!(instructions.contains("investigate_error"));
        assert!(instructions.contains("capabilities"));
    }
}
