//! MCP clients for the two servers tiny-rca bridges.
//!
//! tiny-rca owns no analysis of its own. It holds two MCP clients:
//!
//! - `tinylink serve` — joins OTLP traces to indexed C# code
//! - `tiny-pdg-cs serve` — static analysis: callers, PDGs, patterns
//!
//! and exposes a few tools that span both. Keeping the analysis in the servers
//! that already own it means there is one implementation of route extraction and
//! one of PDG construction, rather than a third copy here.
//!
//! Either server may be absent: a missing child is reported per tool rather than
//! aborting startup, so a deployment can run only the half it needs.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use rmcp::service::{NotificationContext, RequestContext, RunningService, Service, ServiceRole};
use rmcp::ErrorData as McpError;
use rmcp::ServiceExt;
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::RwLock;

/// Pins the protocol version this client proposes in `initialize`.
///
/// The reason this exists is a genuine cross-SDK mismatch. This client sends whatever
/// its SDK considers latest — `2026-07-28` on rmcp 3.5 — while `ModelContextProtocol`
/// 2.2.0 on the .NET side supports up to `2025-11-25`. The .NET server does not
/// negotiate down: an unrecognised version fails the handshake outright, with
///
/// ```text
/// UnsupportedProtocolVersionException: Protocol version '2026-07-28' is not
/// available through the initialize handshake.
/// ```
///
/// so a .NET MCP server cannot be reached at all, however it is addressed. Proposing
/// `LATEST_WITH_INITIALIZE` costs nothing: the Rust SDK implements it, and negotiating
/// down is what the spec's version field is for.
///
/// Raise this when the .NET SDK catches up. Nothing here is specific to the telemetry
/// server — it applies to every .NET MCP server, and this repo has seven of them.
struct PinnedVersion<S>(S);

impl<S: Service<rmcp::RoleClient>> Service<rmcp::RoleClient> for PinnedVersion<S> {
    async fn handle_request(
        &self,
        request: <rmcp::RoleClient as ServiceRole>::PeerReq,
        context: RequestContext<rmcp::RoleClient>,
    ) -> Result<<rmcp::RoleClient as ServiceRole>::Resp, McpError> {
        self.0.handle_request(request, context).await
    }

    async fn handle_notification(
        &self,
        notification: <rmcp::RoleClient as ServiceRole>::PeerNot,
        context: NotificationContext<rmcp::RoleClient>,
    ) -> Result<(), McpError> {
        self.0.handle_notification(notification, context).await
    }

    fn get_info(&self) -> <rmcp::RoleClient as ServiceRole>::Info {
        self.0
            .get_info()
            .with_protocol_version(rmcp::model::ProtocolVersion::LATEST_WITH_INITIALIZE)
    }
}

/// How to reach one backend server.
#[derive(Debug, Clone)]
pub enum Endpoint {
    /// Spawn a binary and talk over its stdio.
    Stdio {
        binary: std::path::PathBuf,
        args: Vec<String>,
        /// Extra environment for the child.
        ///
        /// The telemetry server needs two variables set to start in stdio mode at all:
        /// it refuses to come up as a child process with authentication required,
        /// because there is no client to present a token. Passing them here rather than
        /// expecting the caller to export them means `--telemetry <dll>` works on its
        /// own.
        env: Vec<(String, String)>,
    },
    /// Connect to a Streamable HTTP endpoint.
    ///
    /// Use this when the backend runs as a shared server — several tiny-rca
    /// instances against one index, or a backend in its own container. Spawning
    /// a process per tiny-rca would reparse the solution each time.
    Http { url: std::sync::Arc<str> },
}

/// Handle to one connected MCP server.
pub struct McpLink {
    name: String,
    /// Tool names the server advertises, so a tool can check the target
    /// actually has it before calling.
    tools: Vec<String>,
    service: RunningService<rmcp::RoleClient, PinnedVersion<()>>,
}

impl McpLink {
    /// Connect to a backend and complete the MCP handshake.
    ///
    /// stdio and Streamable HTTP are separate types with no common trait object,
    /// so each arm does its own handshake and they agree on the result.
    pub async fn connect(name: &str, endpoint: Endpoint) -> anyhow::Result<Self> {
        let (service, tool_list) = match &endpoint {
            Endpoint::Stdio { binary, args, env } => {
                // Only check the filesystem when the argument looks like a path. A bare
                // name — "dotnet", for the telemetry server, which is a .NET assembly and
                // so is launched as `dotnet <dll>` — is a command for the OS to resolve on
                // PATH, and checking Path::exists on it looks in the working directory
                // and reports a perfectly good command as missing.
                let looks_like_path = binary.components().count() > 1;
                anyhow::ensure!(
                    !looks_like_path || binary.exists(),
                    "{} server binary not found at {}. Build it first, e.g. \
                     `cargo build --release --features mcp` in that project, or pass its path.",
                    name,
                    binary.display()
                );
                // Inherit the parent's telemetry connection settings — where the database
                // is, which provider — and set only what is specific to being launched
                // this way. The telemetry server refuses to start as a child process with
                // authentication required, because there is no client to present a token.
                let mut cmd = Command::new(binary);
                for (key, value) in env {
                    cmd.env(key, value);
                }
                cmd.args(args)
                    .env("TELEMETRY_MCP_TRANSPORT", "stdio")
                    .env("TELEMETRY_MCP_AUTH", "disabled");

                // The child's stderr is not part of the protocol, but it is drained into
                // the log rather than discarded. Discarding it protects against a chatty
                // server filling a pipe and blocking, and draining protects against the
                // same thing — so draining costs nothing, and it means a backend that
                // refuses to start says why instead of merely being reported missing.
                let (transport, stderr) = rmcp::transport::TokioChildProcess::builder(cmd)
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|e| anyhow::anyhow!("failed to spawn {}: {e}", binary.display()))?;

                if let Some(stderr) = stderr {
                    // Owned, because the drain outlives this function and `name` is a
                    // borrow of the caller's.
                    let tag = name.to_string();
                    tokio::spawn(async move {
                        use tokio::io::AsyncBufReadExt;
                        let mut lines = tokio::io::BufReader::new(stderr).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            tracing::warn!("[{tag}] {line}");
                        }
                    });
                }

                handshake(transport, name, &endpoint).await?
            }
            Endpoint::Http { url } => {
                anyhow::ensure!(
                    url.starts_with("http://") || url.starts_with("https://"),
                    "{} URL must start with http:// or https://, got '{}'",
                    name,
                    url
                );
                let transport = rmcp::transport::StreamableHttpClientTransport::from_uri(
                    std::sync::Arc::clone(url),
                );
                handshake(transport, name, &endpoint).await?
            }
        };

        let tools: Vec<String> = tool_list.iter().map(|t| t.name.to_string()).collect();
        tracing::info!(server = name, tools = tools.len(), ?endpoint, "connected");

        Ok(Self {
            name: name.to_string(),
            tools,
            service,
        })
    }

    /// Spawn a server binary over stdio and complete the MCP handshake.
    pub async fn spawn(name: &str, binary: &Path, args: &[String]) -> anyhow::Result<Self> {
        Self::connect(
            name,
            Endpoint::Stdio {
                binary: binary.to_path_buf(),
                args: args.to_vec(),
                env: Vec::new(),
            },
        )
        .await
    }

    /// Call a tool and return whatever it answered with, JSON or not.
    ///
    /// For passthroughs, whose tools are not required to answer in JSON. Use [`call`] for a
    /// tool of this server's own, where a non-JSON answer is a real fault.
    pub async fn call_raw(&self, tool: &str, args: Value) -> Result<Value, String> {
        let text = self.call_for_text(tool, args).await?;
        Ok(decode_tool_answer(text.clone()).unwrap_or_else(|_| wrap_prose_answer(text)))
    }

    /// The text a tool answered with, without deciding what shape it is in.
    async fn call_for_text(&self, tool: &str, args: Value) -> Result<String, String> {
        if !self.has_tool(tool) {
            return Err(format!(
                "{} server has no tool '{tool}'. It offers {} tool(s)",
                self.name,
                self.tools.len()
            ));
        }

        let result = self
            .service
            .call_tool(
                rmcp::model::CallToolRequestParams::new(tool.to_string())
                    .with_arguments(args.as_object().cloned().unwrap_or_default()),
            )
            .await
            .map_err(|e| format!("{} server call to '{tool}' failed: {e}", self.name))?;

        let text = result
            .content
            .iter()
            .filter_map(|c| match c {
                rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .next()
            .ok_or_else(|| format!("{} server returned no text content", self.name))?
            .to_string();

        if result.is_error.unwrap_or(false) {
            return Err(format!("{} server reported: {}", self.name, text));
        }
        Ok(text)
    }

    // `call` keeps its own body rather than sharing with `call_raw`. The duplication is
    // four lines of protocol, and the alternative is one function with a flag that means
    // two different things to its caller.

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn tools(&self) -> &[String] {
        &self.tools
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t == name)
    }

    /// Call a tool and return its decoded JSON payload.
    ///
    /// Servers return a single text content block containing JSON; anything else
    /// is surfaced as an error naming the server, because a silent empty result
    /// would make a downstream tool report "no error found" for what is really a
    /// transport problem.
    pub async fn call(&self, tool: &str, args: Value) -> Result<Value, String> {
        if !self.has_tool(tool) {
            return Err(format!(
                "{} server has no tool '{}'; it offers {} tool(s)",
                self.name,
                tool,
                self.tools.len()
            ));
        }

        let result = self
            .service
            .call_tool(
                rmcp::model::CallToolRequestParams::new(tool.to_string())
                    .with_arguments(args.as_object().cloned().unwrap_or_default()),
            )
            .await
            .map_err(|e| format!("{} server call to '{tool}' failed: {e}", self.name))?;

        let is_error = result.is_error.unwrap_or(false);
        let text = result
            .content
            .iter()
            .filter_map(|c| match c {
                rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .next()
            .ok_or_else(|| format!("{} server returned no text content", self.name))?
            .to_string();

        if is_error {
            return Err(format!("{} server reported: {}", self.name, text));
        }

        // The text goes into the message. "expected value at line 1 column 1" says
        // nothing about which of two servers produced it or what it said instead, and
        // a backend returning a different content shape is exactly the case this has to
        // make diagnosable from the other side.
        serde_json::from_str(&text).map_err(|e| {
            let head: String = text.chars().take(200).collect();
            format!(
                "{} server returned non-JSON from '{tool}': {e}. First bytes: {}",
                self.name,
                if head.is_empty() {
                    "<empty>".to_string()
                } else {
                    head
                }
            )
        })
    }
}

/// Turn a tool's text answer into a value, or say why it cannot.
///
/// Split out from the two callers so it can be tested without a server: the decision is
/// "is this JSON or not", and that is the whole of what differs between them.
fn decode_tool_answer(text: String) -> Result<Value, String> {
    serde_json::from_str(&text).map_err(|e| {
        let head: String = text.chars().take(200).collect();
        format!(
            "returned non-JSON: {e}. First bytes: {}",
            if head.is_empty() { "<empty>" } else { &head }
        )
    })
}

/// Wrap a prose answer so the caller still gets one shape, and say that is what happened.
///
/// A passthrough forwards another server's tools verbatim, and those tools answer in
/// prose when there is nothing to report -- `endpoint_health` returns "No rows matched:
/// routes with at least 20 requests ..." for an empty result. That is a correct answer,
/// and requiring JSON turns it into a failure that reads like a transport problem and
/// points whoever is reading it at the wrong thing.
fn wrap_prose_answer(text: String) -> Value {
    serde_json::json!({
        "text": text,
        "note": "this tool answered in prose rather than JSON, and it is passed through \
                 unchanged. The text above is the answer, not an error.",
    })
}

/// Initialize a transport and list its tools, with a timeout.
///
/// A backend that accepts the connection but never completes the handshake —
/// wrong URL, a plain HTTP server that is not MCP — would otherwise hang the
/// agent indefinitely.
async fn handshake<T, E, A>(
    transport: T,
    name: &str,
    endpoint: &Endpoint,
) -> anyhow::Result<(
    RunningService<rmcp::RoleClient, PinnedVersion<()>>,
    Vec<rmcp::model::Tool>,
)>
where
    T: rmcp::transport::IntoTransport<rmcp::RoleClient, E, A> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
    A: Send + 'static,
{
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        // Pinned, so a backend on an older MCP SDK can still be reached; see
        // PinnedVersion.
        let service = PinnedVersion(()).serve(transport).await?;
        let tools = service.list_all_tools().await?;
        anyhow::Ok((service, tools))
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!("{name} did not complete the MCP handshake within 20s ({endpoint:?})")
    })?
    .map_err(|e| anyhow::anyhow!("{name} handshake failed: {e}"))
}

/// The two servers tiny-rca bridges, each optional.
pub struct Backends {
    /// OTLP trace → code linking.
    pub link: Option<Arc<McpLink>>,
    /// Static code analysis.
    pub pdg: Option<Arc<McpLink>>,
    /// The deployment's telemetry database: live traces, logs, and the runtime path
    /// catalog. Optional, and the only source of paths.
    pub telemetry: Option<Arc<McpLink>>,
    /// Human-readable notes about servers that could not be started, returned by
    /// tools so the agent knows the view is partial instead of guessing.
    pub unavailable: RwLock<Vec<String>>,
}

impl Backends {
    pub fn new() -> Self {
        Self {
            link: None,
            pdg: None,
            telemetry: None,
            unavailable: RwLock::new(Vec::new()),
        }
    }

    pub async fn note_missing(&self, message: String) {
        self.unavailable.write().await.push(message);
    }

    pub async fn missing_report(&self) -> Vec<String> {
        self.unavailable.read().await.clone()
    }

    pub fn link_required(&self, tool: &str) -> Result<&Arc<McpLink>, String> {
        self.link
            .as_ref()
            .ok_or_else(|| format!("{tool} needs the tinylink server, which is not running"))
    }

    /// The telemetry server, or a reason it is not there.
    ///
    /// The message distinguishes "you did not ask for it" from "you asked and it did
    /// not come up", because the first is a configuration choice and the second is
    /// something to fix.
    pub fn telemetry_required(&self, tool: &str) -> Result<&Arc<McpLink>, String> {
        self.telemetry.as_ref().ok_or_else(|| {
            let asked = self
                .unavailable
                .try_read()
                .map(|u| u.iter().any(|n| n.starts_with("telemetry")))
                .unwrap_or(false);
            if asked {
                format!(
                    "{tool} needs the telemetry server, which was requested but did not \
                     start. Check its stderr: it reads the telemetry database directly, \
                     so TELEMETRY_CLICKHOUSE_HOST or TELEMETRY_POSTGRESQL_HOST must \
                     resolve, and TELEMETRY_MCP_AUTH must be disabled for stdio."
                )
            } else {
                format!(
                    "{tool} needs the telemetry server, which this instance was not \
                     started with. Pass --telemetry <dll> or --telemetry-url <url>. It is \
                     the only source of runtime paths: which branch of the code a request \
                     took is recorded by the running service, not by a trace capture."
                )
            }
        })
    }

    pub fn pdg_required(&self, tool: &str) -> Result<&Arc<McpLink>, String> {
        self.pdg
            .as_ref()
            .ok_or_else(|| format!("{tool} needs the tiny-pdg-cs server, which is not running"))
    }
}

impl Default for Backends {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawning_a_missing_binary_explains_how_to_build_it() {
        let err = McpLink::spawn("tinylink", Path::new("/nonexistent/tinylink"), &[])
            .await
            .err()
            .expect("missing binary must fail");
        let message = format!("{:#}", err);
        assert!(message.contains("not found"), "{message}");
        assert!(
            message.contains("cargo build"),
            "error should say how to build it: {message}"
        );
    }

    #[tokio::test]
    async fn a_binary_that_exits_immediately_reports_a_handshake_failure() {
        // /bin/true exits at once, so no handshake can complete. The failure
        // must be reported rather than hanging forever.
        let err = McpLink::spawn("noop", Path::new("/bin/true"), &[])
            .await
            .err()
            .expect("a process that exits at once must not connect");
        assert!(!format!("{:#}", err).is_empty());
    }

    #[tokio::test]
    async fn required_backend_reports_which_tool_needs_it() {
        let backends = Backends::new();
        let err = backends
            .link_required("investigate_error")
            .err()
            .expect("no link backend configured");
        assert!(err.contains("investigate_error"), "{err}");
        assert!(err.contains("tinylink"), "{err}");

        let err = backends
            .pdg_required("inspect_method")
            .err()
            .expect("no pdg backend configured");
        assert!(err.contains("tiny-pdg-cs"), "{err}");
    }

    /// A passthrough must not turn a prose answer into a transport failure.
    ///
    /// `endpoint_health` answers "No rows matched: routes with at least 20 requests ..."
    /// when there is nothing to report. That is the correct answer, and it was arriving
    /// as an error naming JSON -- which points whoever is reading it at the transport
    /// instead of at the empty result.
    #[test]
    fn a_prose_answer_is_passed_through_with_a_note() {
        let answer = "No rows matched: routes with at least 20 requests in the last 1440 minutes.";
        let value = wrap_prose_answer(answer.to_string());

        assert_eq!(value["text"], answer);
        let note = value["note"].as_str().expect("the wrapper explains itself");
        assert!(note.contains("prose"), "{note}");
        assert!(
            note.contains("not an error"),
            "a caller must not read the wrapper as a failure: {note}"
        );
    }

    /// The same text through the JSON path is still an error, because a tool of this
    /// server's own that promises JSON and does not deliver it is a different fault.
    #[test]
    fn a_prose_answer_is_still_a_failure_for_a_json_caller() {
        let err = decode_tool_answer("No rows matched: ...".to_string())
            .expect_err("a JSON caller must not accept prose");
        assert!(err.contains("non-JSON"), "{err}");
        // And it quotes what came back, because "expected value at line 1 column 1" on its
        // own says nothing about what was actually said.
        assert!(err.contains("No rows matched"), "{err}");
    }

    #[test]
    fn a_json_answer_passes_through_unchanged() {
        let value =
            decode_tool_answer(r#"{"total":3,"items":[]}"#.to_string()).expect("JSON is JSON");
        assert_eq!(value["total"], 3);
        assert!(
            value.get("note").is_none(),
            "no wrapper when none is needed"
        );
    }

    /// An empty answer is a fault, not prose to be passed along.
    #[test]
    fn an_empty_answer_is_reported_as_empty() {
        let err = decode_tool_answer(String::new()).expect_err("empty is not a value");
        assert!(err.contains("<empty>"), "{err}");
    }

    #[tokio::test]
    async fn missing_servers_are_recorded_for_the_report() {
        let backends = Backends::new();
        backends
            .note_missing("tinylink server is not running".into())
            .await;
        let report = backends.missing_report().await;
        assert_eq!(report.len(), 1);
        assert!(report[0].contains("tinylink"));
    }
}
