use clap::{Parser, Subcommand};
use tiny_rca::{RcaServer, ServeArgs};

#[derive(Parser)]
#[command(
    name = "tiny-rca",
    about = "Root-cause analysis for .NET microservices over MCP"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

// Clippy wants the data-carrying variant boxed. It is right about the size and wrong
// about the consequence: this enum is constructed once by clap, pattern-matched once,
// and lives for the process. Boxing it would add an allocation and an indirection to
// satisfy a heuristic that pays off only when a large enum is moved in a loop, which
// this never is.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Commands {
    /// Start the MCP server (stdio), bridging tinylink and tiny-pdg-cs
    Serve(ServeArgs),
    /// List the tools this server exposes, for wiring into an MCP client
    Tools,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        // Logs go to stderr: stdout carries the JSON-RPC stream.
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Serve(args) => serve(args),
        Commands::Tools => {
            for name in [
                "investigate_error",
                "explain_method",
                "method_dependencies",
                "search_logs",
                "ingest_logs",
                "telemetry_call",
                "capabilities",
            ] {
                println!("{name}");
            }
            Ok(())
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}

fn serve(args: ServeArgs) -> anyhow::Result<()> {
    use rmcp::transport::stdio;
    use rmcp::ServiceExt;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        let server = RcaServer::connect(args.clone()).await?;
        if args.http {
            return server.serve_http(args).await;
        }
        let service = server.serve(stdio()).await?;
        service.waiting().await?;
        Ok::<(), anyhow::Error>(())
    })
}
