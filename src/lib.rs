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

    fn pdg_endpoint(&self) -> Option<backends::Endpoint> {
        if let Some(url) = self.pdg_url.as_ref() {
            return Some(backends::Endpoint::Http {
                url: url.as_str().into(),
            });
        }
        self.pdg.as_ref().map(|binary| backends::Endpoint::Stdio {
            binary: binary.clone(),
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

        for note in notes {
            backends.note_missing(note).await;
        }
        Ok(Self::new(Arc::new(backends)))
    }

    /// Serve over Streamable HTTP until the process is stopped.
    ///
    /// Stdio is the default because an MCP client launches the binary; HTTP is
    /// for shared or remote deployments. Same four tools either way.
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
        description = "Start a root-cause investigation of a failed request. Give it an OTLP trace (inline JSON or a file path) and it returns every errored hop mapped to the handling method, file and line, plus the HTTP endpoint and how confident the mapping is. Start every incident here.",
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
        description = "Given Class.Method, report its file and line range, the HTTP endpoints that reach it, and its callers and callees. Use after investigate_error to move from \"this method failed\" to \"what does this method touch\".",
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
        description = "Program dependence graph for a method: basic blocks with line ranges plus control and data dependences. Use to reason about which statements a failing branch depends on.",
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
            "Root-cause analysis for .NET microservices. Combines OpenTelemetry traces with \
             the C# source to answer \"why did this request fail\". All tools are read-only. \
             Typical flow: capabilities to see what is available, then investigate_error with \
             the failing trace to map each errored hop to a method, file and line with a \
             confidence score, then explain_method or method_dependencies on the result to \
             understand the code. Trace data is supplied per call — pass OTLP JSON inline or a \
             file path."
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_registers_the_four_rca_tools() {
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
            "capabilities",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
        assert_eq!(names.len(), 4, "unexpected tool set: {names:?}");
    }

    #[test]
    fn tools_are_read_only_and_described() {
        let router = RcaServer::tool_router();
        for tool in router.list_all() {
            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{} has no annotations", tool.name));
            assert_eq!(annotations.read_only_hint, Some(true), "{}", tool.name);
            assert!(tool.description.as_deref().unwrap().len() > 40);
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
