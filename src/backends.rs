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

use rmcp::service::RunningService;
use rmcp::ServiceExt;
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::RwLock;

/// Handle to one connected MCP server.
pub struct McpLink {
    name: String,
    /// Tool names the server advertises, so a tool can check the target
    /// actually has it before calling.
    tools: Vec<String>,
    service: RunningService<rmcp::RoleClient, ()>,
}

impl McpLink {
    /// Spawn a server binary over stdio and complete the MCP handshake.
    pub async fn spawn(name: &str, binary: &Path, args: &[String]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            binary.exists(),
            "{} server binary not found at {}. Build it first, e.g. \
             `cargo build --release --features mcp` in that project, or pass its path.",
            name,
            binary.display()
        );

        let mut cmd = Command::new(binary);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The child's stderr is not part of the protocol; discard it so a
            // chatty server cannot fill a pipe and block.
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let transport = rmcp::transport::TokioChildProcess::new(cmd)
            .map_err(|e| anyhow::anyhow!("failed to spawn {}: {e}", binary.display()))?;

        let service = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            let service = ().serve(transport).await?;
            let tools = service.list_all_tools().await?;
            anyhow::Ok((service, tools))
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "{} did not complete the MCP handshake within 20s",
                binary.display()
            )
        })?
        .map_err(|e| anyhow::anyhow!("{} handshake failed: {e}", binary.display()))?;

        let (service, tool_list) = service;
        let tools: Vec<String> = tool_list.iter().map(|t| t.name.to_string()).collect();

        tracing::info!(server = name, tools = tools.len(), "connected");

        Ok(Self {
            name: name.to_string(),
            tools,
            service,
        })
    }

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

        serde_json::from_str(&text)
            .map_err(|e| format!("{} server returned non-JSON from '{tool}': {e}", self.name))
    }
}

/// The two servers tiny-rca bridges, each optional.
pub struct Backends {
    /// OTLP trace → code linking.
    pub link: Option<Arc<McpLink>>,
    /// Static code analysis.
    pub pdg: Option<Arc<McpLink>>,
    /// Human-readable notes about servers that could not be started, returned by
    /// tools so the agent knows the view is partial instead of guessing.
    pub unavailable: RwLock<Vec<String>>,
}

impl Backends {
    pub fn new() -> Self {
        Self {
            link: None,
            pdg: None,
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
