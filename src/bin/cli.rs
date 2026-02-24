use clap::{Parser, Subcommand};
use evo_gateway::auth::AuthStore;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "evo-gateway-cli", about = "CLI for managing evo-gateway")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage authentication keys
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
}

#[derive(Subcommand)]
enum AuthAction {
    /// Generate a new API key
    Generate {
        /// Name for the key
        #[arg(long)]
        name: String,
        /// Path to keys file
        #[arg(long, default_value = "auth.json")]
        keys_file: PathBuf,
    },
    /// List all API keys
    List {
        /// Path to keys file
        #[arg(long, default_value = "auth.json")]
        keys_file: PathBuf,
    },
    /// Revoke an API key by name
    Revoke {
        /// Name of the key to revoke
        #[arg(long)]
        name: String,
        /// Path to keys file
        #[arg(long, default_value = "auth.json")]
        keys_file: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Auth { action } => match action {
            AuthAction::Generate { name, keys_file } => {
                let mut store = AuthStore::load(&keys_file)?;
                let raw_key = store.generate(&name);
                store.save(&keys_file)?;
                println!("Generated API key for '{name}':");
                println!();
                println!("  {raw_key}");
                println!();
                println!("Save this key — it cannot be recovered.");
            }
            AuthAction::List { keys_file } => {
                let store = AuthStore::load(&keys_file)?;
                if store.keys.is_empty() {
                    println!("No keys found.");
                    return Ok(());
                }
                println!("{:<20} {:<20} CREATED", "NAME", "PREFIX");
                println!("{}", "-".repeat(60));
                for key in &store.keys {
                    println!(
                        "{:<20} {:<20} {}",
                        key.name,
                        key.prefix,
                        key.created_at.format("%Y-%m-%d %H:%M:%S UTC")
                    );
                }
            }
            AuthAction::Revoke { name, keys_file } => {
                let mut store = AuthStore::load(&keys_file)?;
                if store.revoke(&name) {
                    store.save(&keys_file)?;
                    println!("Revoked key '{name}'.");
                } else {
                    println!("No key found with name '{name}'.");
                }
            }
        },
    }

    Ok(())
}
