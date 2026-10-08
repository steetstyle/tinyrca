//! End-to-end Streamable HTTP test for tiny-rca.
//!
//! Covers what the stdio integration test cannot:
//!
//! - tiny-rca serving over HTTP instead of stdio
//! - tiny-rca *connecting* to a backend over HTTP rather than spawning it, which
//!   is the deployment where the backends are shared services
//!
//! Real backend processes, real sockets, real handshakes. A transport that
//! compiles and does not actually connect passes every other test.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;
use serde_json::{json, Value};

/// A backend binary built in release mode with the HTTP transport enabled.
struct Backends {
    tinylink: PathBuf,
    /// Located but not used by these cases: the pdg backend is exercised over
    /// HTTP by tiny-pdg-cs's own test suite, and here it would only add a second
    /// process to wait on.
    #[allow(dead_code)]
    pdg: PathBuf,
}

impl Backends {
    fn locate() -> Option<Self> {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()?
            .to_path_buf();
        let tinylink = repo.join("tinylink/target/release/tinylink");
        let pdg = repo.join("tiny-pdg-cs/target/release/tiny-pdg-cs");
        (tinylink.exists() && pdg.exists()).then_some(Self { tinylink, pdg })
    }
}

fn setup_project(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("tiny_rca_http_{}", name));
    let _ = std::fs::remove_dir_all(&root);
    let svc = root.join("Agency.API");
    std::fs::create_dir_all(&svc).unwrap();
    std::fs::write(
        svc.join("Handler.cs"),
        r#"
namespace Agency.API;
public class GetMembersEndpoint
{
    public static IResult Handler(HttpContext ctx)
    {
        var rows = Load();
        if (rows is null) { return StatusCode(503); }
        return Ok(rows);
    }
    static object Load() => null;
    static IResult Ok(object r) => null;
    static IResult StatusCode(int code) => null;
}
"#,
    )
    .unwrap();
    std::fs::write(
        svc.join("Ext.cs"),
        "var api = app.MapGroup(\"\");\napi.MapGet(\"/api/agency/{agencyId:guid}/member\", GetMembersEndpoint.Handler);\n",
    )
    .unwrap();

    let db = root.join("index.db");
    let dirs = vec![svc.display().to_string()];
    let built = tinylink::link::index::build(&dirs).expect("build index");
    tinylink::link::index::write(&built, &db).expect("write index");
    (root, db)
}

const FAILING_TRACE: &str = r#"{"resourceSpans":[
 {"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"Dashboard.API"}}]},
  "scopeSpans":[{"spans":[
   {"traceId":"t-http","spanId":"s1","parentSpanId":"","name":"GET","kind":3,
    "attributes":[{"key":"http.method","value":{"stringValue":"GET"}},
                  {"key":"http.target","value":{"stringValue":"/api/agency/9f1c/member"}}],
    "startTimeUnixNano":0,"endTimeUnixNano":100000000}]}]},
 {"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"Agency.API"}}]},
  "scopeSpans":[{"spans":[
   {"traceId":"t-http","spanId":"s2","parentSpanId":"s1","name":"GET","kind":2,
    "attributes":[{"key":"http.method","value":{"stringValue":"GET"}},
                  {"key":"http.route","value":{"stringValue":"/api/agency/{agencyId}/member"}},
                  {"key":"http.status_code","value":{"intValue":503}}],
    "startTimeUnixNano":5000000,"endTimeUnixNano":90000000}]}]}
]}"#;

/// A server process listening on a fixed port.
struct HttpServer {
    child: Child,
    port: u16,
}

impl HttpServer {
    fn spawn(binary: &PathBuf, args: &[String], port: u16) -> Self {
        let child = Command::new(binary)
            .args(args)
            .args(["--http", "--port", &port.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn server");
        Self { child, port }
    }

    fn url(&self) -> std::sync::Arc<str> {
        format!("http://127.0.0.1:{}/mcp", self.port).into()
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Like [`wait_for_port`], but fails with the child's stderr.
///
/// A server that exits during startup is the common case here — a missing index,
/// a bad flag — and "did not come up" on its own says nothing about why.
fn wait_for_port_or_panic(server: &mut HttpServer, timeout: Duration) {
    if wait_for_port(server.port, timeout) {
        return;
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = server.child.stderr.take() {
        use std::io::Read as _;
        let _ = pipe.read_to_string(&mut stderr);
    }
    panic!(
        "server on port {} did not come up within {timeout:?}. stderr:\n{stderr}",
        server.port
    );
}

/// Connect an MCP client over Streamable HTTP and call one tool.
///
/// Uses the SDK client rather than hand-rolled HTTP: the session handshake has
/// several steps, and getting them subtly wrong produces a test that fails for
/// reasons unrelated to the server.
async fn call_tool(url: &str, name: &str, args: Value) -> Result<Value, String> {
    let uri: std::sync::Arc<str> = url.into();
    let service = tokio::time::timeout(
        Duration::from_secs(30),
        ().serve(StreamableHttpClientTransport::from_uri(uri)),
    )
    .await
    .map_err(|_| format!("connecting to {url} timed out"))?
    .map_err(|e| format!("connecting to {url} failed: {e}"))?;

    let result = tokio::time::timeout(
        Duration::from_secs(60),
        service.call_tool(
            rmcp::model::CallToolRequestParams::new(name.to_string())
                .with_arguments(args.as_object().cloned().unwrap_or_default()),
        ),
    )
    .await
    .map_err(|_| format!("calling {name} timed out"))?
    .map_err(|e| format!("calling {name} failed: {e}"))?;

    let text = result
        .content
        .iter()
        .filter_map(|c| match c {
            rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .next()
        .ok_or_else(|| format!("{name} returned no text content"))?
        .to_string();

    if result.is_error.unwrap_or(false) {
        return Err(text);
    }
    serde_json::from_str(&text).map_err(|e| format!("{name} returned non-JSON ({e}): {text}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn tiny_rca_serves_over_http_and_reaches_an_http_tinylink_backend() {
    let Some(backends) = Backends::locate() else {
        eprintln!("skipping: backend binaries not built with mcp-http");
        return;
    };

    let (root, db) = setup_project("chain");
    let tl_port = free_port();
    let rca_port = free_port();

    // 1. tinylink as a shared HTTP server holding the index.
    let mut tl = HttpServer::spawn(
        &backends.tinylink,
        &[
            "serve".to_string(),
            "--index".to_string(),
            db.display().to_string(),
        ],
        tl_port,
    );
    wait_for_port_or_panic(&mut tl, Duration::from_secs(40));

    // 2. tiny-rca over HTTP, reaching tinylink by URL rather than spawning it.
    let rca_bin = PathBuf::from(env!("CARGO_BIN_EXE_tiny-rca"));
    let mut rca = HttpServer::spawn(
        &rca_bin,
        &[
            "serve".to_string(),
            "--tinylink-url".to_string(),
            tl.url().to_string(),
        ],
        rca_port,
    );
    wait_for_port_or_panic(&mut rca, Duration::from_secs(60));

    // 3. An agent talks to tiny-rca over HTTP.
    let url = rca.url().to_string();
    let caps = call_tool(&url, "capabilities", json!({}))
        .await
        .expect("capabilities over HTTP");
    assert_eq!(
        caps["can_map_traces_to_code"], true,
        "tiny-rca should have connected to the HTTP tinylink backend: {caps}"
    );
    let servers = caps["servers"].as_array().unwrap();
    let tl_server = servers.iter().find(|s| s["server"] == "tinylink").unwrap();
    assert_eq!(tl_server["connected"], true, "{caps}");
    assert!(
        !tl_server["tools"].as_array().unwrap().is_empty(),
        "the HTTP backend's tools must be listed: {caps}"
    );

    // 4. And the investigation works across both HTTP hops.
    let out = call_tool(&url, "investigate_error", json!({"trace": FAILING_TRACE}))
        .await
        .expect("investigate_error over HTTP");
    let hops = out["investigation"].as_array().expect("investigation");
    assert_eq!(hops.len(), 1, "{out}");
    assert_eq!(
        hops[0]["to"]["method"], "GetMembersEndpoint.Handler",
        "the 503 must resolve through the HTTP backend: {out}"
    );
    assert_eq!(hops[0]["to"]["confidence"], 0.95);

    drop(tl);
    drop(rca);
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn tiny_rca_reports_an_unreachable_http_backend() {
    // Point at a port with nothing on it: without --strict the server must still
    // start and explain, since an agent can still ask what is available.
    let rca_bin = PathBuf::from(env!("CARGO_BIN_EXE_tiny-rca"));
    let dead = free_port();
    let rca_port = free_port();

    let child = Command::new(&rca_bin)
        .args([
            "serve",
            "--tinylink-url",
            &format!("http://127.0.0.1:{dead}/mcp"),
            "--http",
            "--port",
            &rca_port.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tiny-rca");
    let mut server = HttpServer {
        child,
        port: rca_port,
    };
    wait_for_port_or_panic(&mut server, Duration::from_secs(40));

    let caps = call_tool(server.url().as_ref(), "capabilities", json!({}))
        .await
        .expect("capabilities");
    assert_eq!(caps["can_map_traces_to_code"], false, "{caps}");
    let unavailable = caps["unavailable"].as_array().unwrap();
    assert!(
        unavailable
            .iter()
            .any(|u| u.as_str().unwrap().contains("tinylink")),
        "{caps}"
    );

    drop(server);
}

#[tokio::test(flavor = "multi_thread")]
async fn tiny_rca_over_http_exposes_the_same_tools_as_stdio() {
    let rca_bin = PathBuf::from(env!("CARGO_BIN_EXE_tiny-rca"));
    let rca_port = free_port();
    let mut rca = HttpServer::spawn(&rca_bin, &["serve".to_string()], rca_port);
    wait_for_port_or_panic(&mut rca, Duration::from_secs(40));

    let url = rca.url().to_string();
    let uri: std::sync::Arc<str> = url.as_str().into();
    let service = tokio::time::timeout(
        Duration::from_secs(30),
        ().serve(StreamableHttpClientTransport::from_uri(uri)),
    )
    .await
    .expect("connect")
    .expect("connect to tiny-rca over HTTP");

    let tools = service.list_all_tools().await.expect("list tools");
    let mut names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "capabilities",
            "explain_method",
            "ingest_logs",
            "investigate_error",
            "method_dependencies",
            "search_logs",
            "telemetry_call"
        ],
        "HTTP must expose the same tools as stdio: {names:?}"
    );

    service.cancel().await.ok();
    drop(rca);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_backend_url_is_reported_not_panicked() {
    // A URL without a scheme is a configuration mistake; it must be reported in
    // the tool output, not crash the server.
    let rca_bin = PathBuf::from(env!("CARGO_BIN_EXE_tiny-rca"));
    let rca_port = free_port();

    let child = Command::new(&rca_bin)
        .args([
            "serve",
            "--tinylink-url",
            "127.0.0.1:9999/mcp",
            "--http",
            "--port",
            &rca_port.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tiny-rca");
    let server = HttpServer {
        child,
        port: rca_port,
    };
    assert!(wait_for_port(rca_port, Duration::from_secs(40)));

    let caps = call_tool(server.url().as_ref(), "capabilities", json!({}))
        .await
        .expect("capabilities");
    let unavailable = caps["unavailable"].as_array().unwrap();
    assert!(
        unavailable
            .iter()
            .any(|u| u.as_str().unwrap().contains("http")),
        "a bad URL must be explained: {caps}"
    );

    drop(server);
}
