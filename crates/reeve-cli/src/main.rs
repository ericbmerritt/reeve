//! Reeve binary entry point.

use std::io::{self, BufRead, Write};

use clap::{Parser, Subcommand};
use reeve_runtime::IdentityRegistry;

mod identity;
mod output;

#[derive(Parser, Debug)]
#[command(
    name = "reeve",
    version,
    about = "Reeve — runtime that supervises AI coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Manage operator and agent identities.
    Identity {
        #[command(subcommand)]
        command: IdentityCommands,
    },
}

#[derive(Subcommand, Debug)]
enum IdentityCommands {
    /// Enroll the workstation operator: generate a keypair and register it.
    ///
    /// Fails if an operator identity already exists (one operator per machine).
    Enroll,
    /// Print all registered identities.
    List,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        None => Ok(()),
        Some(Commands::Identity {
            command: IdentityCommands::Enroll,
        }) => cmd_enroll(),
        Some(Commands::Identity {
            command: IdentityCommands::List,
        }) => cmd_list(),
    }
}

#[cfg(target_os = "macos")]
fn cmd_enroll() -> Result<(), Box<dyn std::error::Error>> {
    let keychain = reeve_runtime::keychain::macos::MacOsKeyStore::new();
    run_enroll(&keychain)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn cmd_enroll() -> Result<(), Box<dyn std::error::Error>> {
    let keychain = reeve_runtime::keychain::linux::SecretServiceKeyStore::connect()?;
    run_enroll(&keychain)
}

fn run_enroll(
    keychain: &dyn reeve_runtime::OperatorKeyStore,
) -> Result<(), Box<dyn std::error::Error>> {
    let display_name = prompt_display_name()?;
    let registry = IdentityRegistry::open(IdentityRegistry::default_data_dir()?)?;
    let stored = identity::enroll(&registry, keychain, &display_name)?;
    let id = stored.identity().identity_id;
    let fingerprint = stored
        .key_records()
        .first()
        .map(|kr| kr.public_key.fingerprint())
        .ok_or("enrolled identity has no key record")?;
    writeln!(
        io::stdout().lock(),
        "enrolled: {} ({})\nfingerprint: {}",
        stored.identity().display_name,
        id,
        fingerprint,
    )?;
    Ok(())
}

fn cmd_list() -> Result<(), Box<dyn std::error::Error>> {
    let registry = IdentityRegistry::open(IdentityRegistry::default_data_dir()?)?;
    identity::list(&registry, &mut io::stdout().lock())?;
    Ok(())
}

fn prompt_display_name() -> Result<String, Box<dyn std::error::Error>> {
    // Prompts go to stderr per UNIX convention; only results go to stdout.
    let mut stderr = io::stderr().lock();
    write!(stderr, "Display name: ")?;
    stderr.flush()?;
    // Drop stderr before acquiring stdin.lock to avoid lock ordering issues on Windows.
    drop(stderr);

    let stdin = io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    let name = line.trim().to_owned();
    if name.is_empty() {
        return Err("display name must not be empty".into());
    }
    Ok(name)
}
