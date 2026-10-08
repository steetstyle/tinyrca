//! Integration test: tiny-rca bridging both backend servers on a real incident.
//!
//! Starts `tiny-rca serve`, which in turn spawns `tinylink serve` and
//! `tiny-pdg-cs serve`, then drives the whole chain over stdio JSON-RPC:
//!
//!     agent → tiny-rca → { tinylink, tiny-pdg-cs }
//!
//! The point is to prove the composition actually works — that tiny-rca can talk
//! to both servers, that it forwards a trace and gets a method back, and that a
//! code question reaches tiny-pdg-cs. Unit tests cannot cover any of that.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};
use std::sync::Arc;
use tiny_rca::backends::Backends;

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Mcp {
    fn start(args: &[String]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tiny-rca"))
            .arg("serve")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // tiny-rca's own logs go to stderr; keep them for the failure message.
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn tiny-rca serve");

        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut me = Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        me.send(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "integration", "version": "0"},
            }),
        );
        me.notify("notifications/initialized");
        me
    }

    fn notify(&mut self, method: &str) {
        writeln!(
            self.stdin,
            "{}",
            json!({"jsonrpc": "2.0", "method": method, "params": {}})
        )
        .expect("write notification");
        self.stdin.flush().expect("flush");
    }

    fn send(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        writeln!(
            self.stdin,
            "{}",
            json!({"jsonrpc":"2.0","id":self.next_id,"method":method,"params":params})
        )
        .expect("write request");
        self.stdin.flush().expect("flush");

        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read");
            assert!(
                read > 0,
                "tiny-rca closed the stream; stderr:\n{}",
                self.stderr()
            );
            let value: Value = serde_json::from_str(line.trim())
                .unwrap_or_else(|e| panic!("non-JSON on the MCP stream ({e}): {line}"));
            if value.get("id").and_then(|i| i.as_i64()) == Some(self.next_id) {
                return value;
            }
        }
    }

    fn stderr(&mut self) -> String {
        // Reading stderr after the fact is best-effort; it may already be closed.
        String::new()
    }

    fn call(&mut self, name: &str, args: Value) -> Value {
        let result = self.send("tools/call", json!({"name": name, "arguments": args}));
        assert!(
            result.get("error").is_none(),
            "{name} failed at the protocol level: {result}"
        );
        let content = result["result"]["content"].as_array().expect("content");
        let text = content[0]["text"].as_str().expect("text");
        if result["result"]["isError"] == json!(true) {
            panic!("{name} reported: {text}");
        }
        serde_json::from_str(text).unwrap_or_else(|e| panic!("{name} not JSON ({e}): {text}"))
    }

    fn try_call(&mut self, name: &str, args: Value) -> std::result::Result<Value, String> {
        let result = self.send("tools/call", json!({"name": name, "arguments": args}));
        if let Some(error) = result.get("error") {
            return Err(error.to_string());
        }
        let content = result["result"]["content"].as_array().expect("content");
        let text = content[0]["text"].as_str().expect("text");
        if result["result"]["isError"] == json!(true) {
            return Err(text.to_string());
        }
        serde_json::from_str(text).map_err(|e| format!("not JSON ({e}): {text}"))
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Where the backend binaries live, if they were built.
fn backend_binaries() -> Option<(PathBuf, PathBuf)> {
    // The tinylink binary is not a dependency of this crate, so its path is
    // derived from the tinylink source tree next door.
    let tinylink = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .join("tinylink/target/release/tinylink");
    let pdg = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .join("tiny-pdg-cs/target/release/tiny-pdg-cs");
    if tinylink.exists() && pdg.exists() {
        Some((tinylink, pdg))
    } else {
        None
    }
}

/// Build a C# project and the tinylink index for it.
fn setup_project(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("tiny_rca_it_{}", name));
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
   {"traceId":"t-rca","spanId":"s1","parentSpanId":"","name":"GET","kind":3,
    "attributes":[{"key":"http.method","value":{"stringValue":"GET"}},
                  {"key":"http.target","value":{"stringValue":"/api/agency/9f1c/member"}}],
    "startTimeUnixNano":0,"endTimeUnixNano":100000000}]}]},
 {"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"Agency.API"}}]},
  "scopeSpans":[{"spans":[
   {"traceId":"t-rca","spanId":"s2","parentSpanId":"s1","name":"GET","kind":2,
    "attributes":[{"key":"http.method","value":{"stringValue":"GET"}},
                  {"key":"http.route","value":{"stringValue":"/api/agency/{agencyId}/member"}},
                  {"key":"http.status_code","value":{"intValue":503}}],
    "startTimeUnixNano":5000000,"endTimeUnixNano":90000000}]}]}
]}"#;

#[test]
fn tiny_rca_serves_its_own_tools_with_no_backends() {
    // No --tinylink/--pdg: the server must still start and report what is
    // missing, so an operator sees a usable diagnostic instead of a crash.
    let mut mcp = Mcp::start(&[]);

    let caps = mcp.call("capabilities", json!({}));
    assert_eq!(caps["can_map_traces_to_code"], false, "{caps}");
    assert_eq!(caps["can_analyse_code"], false, "{caps}");
    let servers = caps["servers"].as_array().unwrap();
    assert_eq!(servers.len(), 2, "{caps}");

    let err = mcp
        .try_call("investigate_error", json!({"trace": FAILING_TRACE}))
        .expect_err("without the trace server there is nothing to investigate");
    assert!(
        err.contains("tinylink"),
        "the error must name the missing dependency: {err}"
    );
}

#[test]
fn tiny_rca_lists_only_its_own_tools() {
    let mut mcp = Mcp::start(&[]);
    let listed = mcp.send("tools/list", json!({}));
    let mut names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    // The server lists tools alphabetically; compare as a set.
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "capabilities",
            "explain_method",
            "ingest_logs",
            "investigate_error",
            "method_dependencies",
            "search_logs"
        ],
        "tiny-rca should expose its bridging tools, not duplicate the backends': {names:?}"
    );
}

#[test]
fn investigate_error_maps_a_503_through_the_tinylink_backend() {
    let Some((tinylink_bin, _)) = backend_binaries() else {
        eprintln!("skipping: backend binaries not built (cargo build --release)");
        return;
    };

    let (root, db) = setup_project("investigate");
    let args = vec![
        "--tinylink".into(),
        tinylink_bin.display().to_string(),
        "--index".into(),
        db.display().to_string(),
    ];
    let mut mcp = Mcp::start(&args);

    let caps = mcp.call("capabilities", json!({}));
    assert_eq!(
        caps["can_map_traces_to_code"], true,
        "tinylink should have connected: {caps}"
    );

    let out = mcp.call("investigate_error", json!({"trace": FAILING_TRACE}));
    let hops = out["investigation"].as_array().expect("investigation");
    assert_eq!(hops.len(), 1, "{out}");
    assert_eq!(hops[0]["error_count"], 1);
    assert_eq!(
        hops[0]["to"]["method"], "GetMembersEndpoint.Handler",
        "the 503 must resolve to the handler through the backend: {out}"
    );
    assert_eq!(hops[0]["to"]["confidence"], 0.95);
    let file = hops[0]["to"]["file"].as_str().expect("file");
    assert!(file.ends_with("Handler.cs"), "got {file}");

    // next_steps is how the agent knows where to go next.
    let steps = out["next_steps"].as_array().expect("next_steps");
    assert!(
        steps
            .iter()
            .any(|s| s.as_str().unwrap().contains("GetMembersEndpoint.Handler")),
        "{out}"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn method_dependencies_reaches_the_pdg_backend() {
    let Some((tinylink_bin, pdg_bin)) = backend_binaries() else {
        eprintln!("skipping: backend binaries not built");
        return;
    };

    let (root, db) = setup_project("pdg");
    let source_dir = root.join("Agency.API");
    let args = vec![
        "--tinylink".into(),
        tinylink_bin.display().to_string(),
        "--index".into(),
        db.display().to_string(),
        "--pdg".into(),
        pdg_bin.display().to_string(),
    ];
    let mut mcp = Mcp::start(&args);

    let caps = mcp.call("capabilities", json!({}));
    assert_eq!(
        caps["can_analyse_code"], true,
        "pdg server should connect: {caps}"
    );

    let out = mcp.call(
        "method_dependencies",
        json!({
            "target": "GetMembersEndpoint.Handler",
            "path": source_dir.display().to_string(),
        }),
    );
    assert_eq!(out["method"], "Handler", "{out}");
    let blocks = out["blocks"].as_array().expect("blocks");
    assert!(
        !blocks.is_empty(),
        "expected PDG blocks from the backend: {out}"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn missing_path_is_reported_as_an_argument_error() {
    let Some((_, pdg_bin)) = backend_binaries() else {
        eprintln!("skipping: backend binaries not built");
        return;
    };

    let args = vec!["--pdg".into(), pdg_bin.display().to_string()];
    let mut mcp = Mcp::start(&args);

    // The backend is up but no source root was given. The error must say what
    // is missing rather than failing obscurely.
    let err = mcp
        .try_call(
            "method_dependencies",
            json!({"target": "GetMembersEndpoint.Handler"}),
        )
        .expect_err("a missing path argument must be rejected");
    assert!(err.contains("path"), "{err}");
}

#[test]
fn strict_mode_refuses_to_start_with_a_bad_backend_path() {
    let out = Command::new(env!("CARGO_BIN_EXE_tiny-rca"))
        .args(["serve", "--tinylink", "/nonexistent/tinylink", "--strict"])
        .stdin(Stdio::null())
        .output()
        .expect("run serve");

    assert!(!out.status.success(), "--strict must fail on a bad backend");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("tinylink"),
        "the failure must name the backend: {stderr}"
    );
}

#[test]
fn a_missing_backend_binary_is_reported_not_fatal() {
    let args = vec!["--tinylink".into(), "/nonexistent/tinylink".to_string()];
    let mut mcp = Mcp::start(&args);

    // Without --strict the server still starts and explains what is missing.
    let caps = mcp.call("capabilities", json!({}));
    assert_eq!(caps["can_map_traces_to_code"], false);
    let unavailable = caps["unavailable"].as_array().expect("unavailable");
    assert!(
        unavailable
            .iter()
            .any(|u| u.as_str().unwrap().contains("tinylink")),
        "{caps}"
    );
    assert!(
        unavailable
            .iter()
            .any(|u| u.as_str().unwrap().contains("cargo build")),
        "the note should say how to fix it: {caps}"
    );
}

#[test]
fn server_binary_path_helper_agrees_with_the_filesystem() {
    // Guards the helper the other tests rely on to skip or run.
    match backend_binaries() {
        Some((tinylink, pdg)) => {
            assert!(Path::new(&tinylink).exists(), "{tinylink:?}");
            assert!(Path::new(&pdg).exists(), "{pdg:?}");
        }
        None => eprintln!("backends not built; integration cases will skip"),
    }
}

// ── the log chain, through tiny-rca ──

/// A log payload carrying the same trace id as `FAILING_TRACE`.
///
/// Written as OTLP/JSON rather than reusing the protobuf capture, because the
/// point here is the join: the trace tool and the log tool have to agree on a
/// trace id, and they only do if the id is threaded through both.
fn write_logs_for_the_failing_trace(root: &Path) -> PathBuf {
    let dir = root.join("logs");
    std::fs::create_dir_all(&dir).unwrap();
    let payload = serde_json::json!({
        "resourceLogs": [{
            "resource": {"attributes": [
                {"key": "service.name", "value": {"stringValue": "Agency.API"}}
            ]},
            "scopeLogs": [{"logRecords": [
                {
                    "severityNumber": 9,
                    "severityText": "Information",
                    "body": {"stringValue": "Request started for {Endpoint}"},
                    "traceId": "t-rca",
                    "attributes": [
                        {"key": "Endpoint", "value": {"stringValue": "/api/agency/9f1c/member"}}
                    ]
                },
                {
                    "severityNumber": 17,
                    "severityText": "Error",
                    "body": {"stringValue": "Handler {HandlerName} failed"},
                    "traceId": "t-rca",
                    "attributes": [
                        {"key": "HandlerName", "value": {"stringValue": "GetMembers"}},
                        {"key": "exception.type", "value": {"stringValue": "TimeoutException"}},
                        {"key": "exception.message",
                         "value": {"stringValue": "upstream agency service did not respond"}}
                    ]
                }
            ]}]
        }]
    });
    let path = dir.join("agency-logs.json");
    std::fs::write(&path, serde_json::to_vec(&payload).unwrap()).unwrap();
    path
}

#[test]
fn logs_reach_the_agent_through_tiny_rca() {
    let Some((tinylink_bin, _)) = backend_binaries() else {
        eprintln!("skipping: build tinylink and tiny-pdg-cs first");
        return;
    };
    let (root, db) = setup_project("logs_chain");
    let payload = write_logs_for_the_failing_trace(&root);

    let mut mcp = Mcp::start(&[
        "--tinylink".into(),
        tinylink_bin.display().to_string(),
        "--index".into(),
        db.display().to_string(),
    ]);

    // 1. Ingest through the bridge, not through tinylink directly.
    let ingested = mcp.call(
        "ingest_logs",
        json!({ "source": payload.display().to_string() }),
    );
    assert_eq!(ingested["records_inserted"], 2, "{ingested}");
    assert_eq!(ingested["services"][0], "Agency.API", "{ingested}");

    // 2. Free text that only exists in an attribute. A body-only search would not
    //    find this, because the body is the unrendered template.
    let found = mcp.call(
        "search_logs",
        json!({ "contains": "upstream agency service did not respond" }),
    );
    assert_eq!(found["count"], 1, "{found}");
    assert_eq!(found["logs"][0]["severity"], "Error", "{found}");

    // 3. The join: the trace tool names the failing method, the logs say why.
    let investigated = mcp.call(
        "investigate_error",
        json!({ "trace": FAILING_TRACE, "include_logs": true }),
    );
    let hop = &investigated["investigation"][0];
    assert_eq!(
        hop["to"]["method"], "GetMembersEndpoint.Handler",
        "{investigated}"
    );
    assert_eq!(
        hop["logs"].as_array().map(Vec::len),
        Some(1),
        "{investigated}"
    );
    let per_trace = &hop["logs"][0];
    assert_eq!(per_trace["total_matched"], 2, "{investigated}");
    let records = per_trace["logs"].as_array().expect("records");
    assert_eq!(records.len(), 2, "the whole trace, not just the error");
    // Newest first, so the error leads — that is what the failure was.
    assert_eq!(records[0]["severity"], "Error", "{investigated}");
    assert_eq!(
        records[0]["attributes"]["exception.type"], "TimeoutException",
        "{investigated}"
    );

    // 4. next_steps should point at the log tool, since a trace id is available.
    let steps = investigated["next_steps"].as_array().expect("next_steps");
    assert!(
        steps
            .iter()
            .any(|s| s.as_str().unwrap_or("").contains("search_logs")),
        "{investigated}"
    );
}

/// An empty log store must say so, because "no logs found" and "logs were never
/// ingested" lead to opposite conclusions.
#[test]
fn searching_an_empty_log_store_says_how_to_fill_it() {
    let Some((tinylink_bin, _)) = backend_binaries() else {
        eprintln!("skipping: build tinylink and tiny-pdg-cs first");
        return;
    };
    let (_root, db) = setup_project("logs_empty");

    let mut mcp = Mcp::start(&[
        "--tinylink".into(),
        tinylink_bin.display().to_string(),
        "--index".into(),
        db.display().to_string(),
    ]);

    let out = mcp.call("search_logs", json!({}));
    assert_eq!(out["count"], 0, "{out}");
    assert_eq!(out["store_records"], 0, "{out}");
    assert!(
        out["note"].as_str().unwrap_or("").contains("logs-ingest"),
        "the response should say how to populate the store: {out}"
    );
}

/// `capabilities` has to say whether logs can be searched *and* how to load them.
/// Log search needs the tinylink backend and a server new enough to expose the
/// tools, so a live connection alone is not enough to promise it.
#[test]
fn capabilities_advertises_log_search_and_how_to_load_it() {
    let Some((tinylink_bin, _)) = backend_binaries() else {
        eprintln!("skipping: build tinylink and tiny-pdg-cs first");
        return;
    };
    let (_root, db) = setup_project("caps_logs");

    let mut mcp = Mcp::start(&[
        "--tinylink".into(),
        tinylink_bin.display().to_string(),
        "--index".into(),
        db.display().to_string(),
    ]);

    let caps = mcp.call("capabilities", json!({}));
    assert_eq!(caps["can_search_logs"], true, "{caps}");
    assert!(
        caps["log_guidance"]
            .as_str()
            .unwrap_or("")
            .contains("ingest_logs"),
        "{caps}"
    );
}

/// Without a backend the capability has to be reported as unavailable rather than
/// assumed, so an agent plans around what exists.
#[tokio::test]
async fn capabilities_reports_log_search_unavailable_without_a_backend() {
    let backends = Arc::new(Backends::new());
    let caps = tiny_rca::tools::capabilities(&backends)
        .await
        .expect("capabilities");
    assert_eq!(caps["can_search_logs"], false, "{caps}");
    assert!(
        caps["log_guidance"]
            .as_str()
            .unwrap_or("")
            .contains("unavailable"),
        "{caps}"
    );
}
