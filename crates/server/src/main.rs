use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use sm_server::{auth, http, store::Store};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "sm-server", about = "sharedmem server", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the HTTP server.
    Serve {
        #[arg(long)]
        db: PathBuf,
        /// Keep it on loopback or a tailnet address; tokens must not cross a network in plaintext.
        #[arg(long, default_value = "127.0.0.1:8377")]
        bind: SocketAddr,
    },
    /// Manage tokens.
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
    },
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Create a token for a user's device. Printed once, stored hashed.
    Add {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        user: String,
        #[arg(long)]
        device: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // RUST_LOG overrides: sm_server=debug for every request and tool call, trace for recall queries
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("warn,sm_server=info")),
        )
        .with_writer(std::io::stderr)
        // colours only on a terminal, so log files stay plain text
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    match Cli::parse().cmd {
        Cmd::Serve { db, bind } => {
            let store = Store::open(&db)?;
            let listener = tokio::net::TcpListener::bind(bind).await?;
            info!(addr = %listener.local_addr()?, db = %db.display(), "listening");
            axum::serve(listener, http::router(store))
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
        Cmd::Token {
            cmd: TokenCmd::Add { db, user, device },
        } => {
            let store = Store::open(&db)?;
            let token = auth::new_token()?;
            store
                .add_token(&user, &device, &auth::hash_token(&token))
                .await?;
            println!("{token}");
            info!(%user, %device, "token created; it is not shown again");
        }
    }
    Ok(())
}
