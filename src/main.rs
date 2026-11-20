use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use ridgewarden_failover::config::{Config, EXAMPLE};
use ridgewarden_failover::{App, drill, spawn_loops, web};

#[derive(Parser)]
#[command(
    version,
    about = "MeshChat-compatible chat that survives site loss on AREDN meshes (a ridgewarden app)"
)]
struct Cli {
    #[arg(
        long,
        short,
        global = true,
        default_value = "/etc/ridgewarden/failover.toml"
    )]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write a starter config
    Init,
    /// Follow the core and serve the chat, the MeshChat API and the directory
    Run,
    /// Hide a message or shared file at every site, by its event id (the "id" shown
    /// under it, or longer)
    Hide { id: String },
    /// Take one site down on purpose and check the others keep serving and nothing is
    /// lost when it comes back. Run it at a site other than the one going down.
    Drill {
        /// node name of the site to take down
        #[arg(long)]
        kill: String,
        /// shell command that takes it down; without it, the drill asks you to
        #[arg(long)]
        down: Option<String>,
        /// shell command that brings it back
        #[arg(long)]
        up: Option<String>,
        /// messages posted at each site in each phase
        #[arg(long, default_value_t = 3)]
        messages: usize,
        /// give up on any step after this many seconds
        #[arg(long, default_value_t = 900)]
        timeout_s: u64,
        /// don't log the result through this site's core
        #[arg(long)]
        no_record: bool,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ridgewarden_failover=info,ridgewarden_client=info".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init => {
            if cli.config.exists() {
                bail!(
                    "{} already exists; not overwriting it",
                    cli.config.display()
                );
            }
            if let Some(d) = cli.config.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&cli.config, EXAMPLE)?;
            println!("Wrote {}", cli.config.display());
            Ok(())
        }
        Cmd::Run => {
            let cfg = Config::load(&cli.config)?;
            tokio::runtime::Runtime::new()?.block_on(run(cfg))
        }
        Cmd::Drill {
            kill,
            down,
            up,
            messages,
            timeout_s,
            no_record,
        } => {
            let app = App::new(Config::load(&cli.config)?)?;
            let plan = drill::Plan {
                target: kill,
                down,
                up,
                per_phase: messages.max(1),
                timeout: std::time::Duration::from_secs(timeout_s),
            };
            let rt = tokio::runtime::Runtime::new()?;
            let report = rt.block_on(drill::run(&app, plan))?;
            print!("{}", report.text());
            if !no_record {
                rt.block_on(drill::record(&app, &report))?;
                println!("Logged; every site's sites page counts it.");
            }
            if !report.passed() {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Hide { id } => {
            let app = App::new(Config::load(&cli.config)?)?;
            let full = tokio::runtime::Runtime::new()?.block_on(app.hide(&id))?;
            println!("Hid {full}; other sites hide it as they sync");
            Ok(())
        }
    }
}

async fn run(cfg: Config) -> Result<()> {
    let listen = cfg.listen;
    let app = Arc::new(App::new(cfg)?);
    tracing::info!(
        "ridgewarden-failover {} following {}, chat on http://{listen}",
        env!("CARGO_PKG_VERSION"),
        app.core.base()
    );
    spawn_loops(app.clone());
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    axum::serve(
        listener,
        web::router(app).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}
