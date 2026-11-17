use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod client;
mod commands;
mod config;

#[derive(Parser)]
#[command(name = "cloudreve-cli")]
#[command(about = "CLI tool for Cloudreve file upload", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Login {
        #[arg(short, long)]
        server: String,
        #[arg(short, long)]
        email: String,
        #[arg(short = 'P', long)]
        password: String,
    },
    Upload {
        local: PathBuf,
        remote: String,
        #[arg(short, long)]
        recursive: bool,
        #[arg(long)]
        overwrite: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Login { server, email, password } => {
            commands::login::run(&server, &email, &password).await?;
        }
        Commands::Upload {
            local,
            remote,
            recursive,
            overwrite,
        } => {
            commands::upload::run(&local, &remote, recursive, overwrite).await?;
        }
    }

    Ok(())
}
