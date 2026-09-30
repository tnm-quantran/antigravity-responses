use antigravity_responses::{
    auth,
    config::Config,
    server::{Gateway, router},
    setup,
};
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
#[command(version, about = "Local Antigravity to Responses API gateway")]
struct Cli {
    #[command(flatten)]
    config: Config,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Serve,
    Usage,
    Login {
        #[arg(long, default_value_t = 0)]
        callback_port: u16,
    },
    Setup {
        #[arg(value_parser = ["codex", "rtk", "ponytail", "all"])]
        target: String,
        #[arg(long, env = "CODEX_HOME")]
        codex_home: Option<PathBuf>,
    },
    Doctor {
        #[arg(long, env = "CODEX_HOME")]
        codex_home: Option<PathBuf>,
    },
}

#[tokio::main]
async fn run() -> Result<()> {
    let cli = Cli::parse();
    cli.config.validate()?;
    let command = cli
        .command
        .ok_or_else(|| anyhow::anyhow!("a subcommand is required"))?;
    match command {
        Command::Serve => {
            let gateway = Gateway::new(cli.config)?;
            let listener = tokio::net::TcpListener::bind(gateway.config.listen).await?;
            println!("Listening on http://{}", listener.local_addr()?);
            axum::serve(listener, router(gateway))
                .with_graceful_shutdown(async {
                    if let Err(error) = tokio::signal::ctrl_c().await {
                        eprintln!("shutdown signal: {error}");
                    }
                })
                .await?;
        }
        Command::Usage => antigravity_responses::server::usage(cli.config).await?,
        Command::Login { callback_port } => auth::login(&cli.config, callback_port).await?,
        Command::Setup { target, codex_home } => {
            let started = Instant::now();
            let config = cli.config;
            setup::setup(&config, &target, &setup::codex_home(codex_home)?)?;
            println!("Done in {:.2}s", started.elapsed().as_secs_f64());
        }
        Command::Doctor { codex_home } => {
            setup::doctor(&cli.config, &setup::codex_home(codex_home)?).await?
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    antigravity_responses::config::load_dotenv()?;
    run()
}
