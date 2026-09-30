use antigravity_responses::{
    auth,
    config::Config,
    server::{Gateway, router},
    setup,
};
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "Local Antigravity to Responses API gateway")]
struct Cli {
    #[command(flatten)]
    config: Config,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve,
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
async fn main() -> Result<()> {
    let cli = Cli::parse();
    cli.config.validate()?;
    match cli.command {
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
        Command::Login { callback_port } => auth::login(&cli.config, callback_port).await?,
        Command::Setup { target, codex_home } => {
            setup::setup(&cli.config, &target, &setup::codex_home(codex_home)?)?
        }
        Command::Doctor { codex_home } => {
            setup::doctor(&cli.config, &setup::codex_home(codex_home)?).await?
        }
    }
    Ok(())
}
