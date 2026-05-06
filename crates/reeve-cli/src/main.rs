//! Reeve binary entry point.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use reeve_runtime::{AuditLog, IdentityRegistry};
use reeve_types::IdentityId;

mod adapter;
mod daemon;
mod envelope;
mod identity;
mod keychain;
mod output;
mod prompt;

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
    /// Debug subcommands for signed message envelopes.
    Envelope {
        #[command(subcommand)]
        command: EnvelopeCommands,
    },
    /// Manage model adapters: store API keys and test end-to-end connectivity.
    Adapter {
        #[command(subcommand)]
        command: adapter::AdapterSubcommand,
    },
    /// Manage the runtime daemon: start, stop, check status.
    Daemon {
        #[command(subcommand)]
        command: daemon::DaemonSubcommand,
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
    /// Remove the operator identity, its keychain entry, and append an audit
    /// record. Requires --confirm to prevent accidental invocation.
    Unenroll {
        /// Acknowledge the destructive operation. Without this flag the
        /// command exits with a reminder message and does nothing.
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Subcommand, Debug)]
enum EnvelopeCommands {
    /// Sign a new envelope addressed to a recipient and write JSON to stdout.
    Sign {
        /// Recipient identity ID (`UUIDv7` string). Use `reeve identity list`
        /// to find identity IDs.
        #[arg(long)]
        to: String,
        /// UTF-8 text to use as the envelope payload. Note: this value is
        /// visible in process listings on Unix.
        #[arg(long)]
        body: String,
    },
    /// Verify an envelope JSON file and print confirmation to stdout.
    Verify {
        /// Path to the envelope JSON file.
        file: PathBuf,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        // No subcommand: exit 0; clap's --help handles help requests.
        None => Ok(()),
        Some(Commands::Identity {
            command: IdentityCommands::Enroll,
        }) => cmd_enroll(),
        Some(Commands::Identity {
            command: IdentityCommands::List,
        }) => cmd_list(),
        Some(Commands::Identity {
            command: IdentityCommands::Unenroll { confirm },
        }) => cmd_unenroll(confirm),
        Some(Commands::Envelope {
            command: EnvelopeCommands::Sign { to, body },
        }) => cmd_envelope_sign(&to, &body),
        Some(Commands::Envelope {
            command: EnvelopeCommands::Verify { file },
        }) => cmd_envelope_verify(&file),
        Some(Commands::Adapter { command }) => adapter::dispatch(command),
        Some(Commands::Daemon { command }) => daemon::dispatch(&command),
    }
}

fn cmd_enroll() -> Result<(), Box<dyn std::error::Error>> {
    let keychain = keychain::open_platform_keystore()?;
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

fn cmd_unenroll(confirm: bool) -> Result<(), Box<dyn std::error::Error>> {
    let keychain = keychain::open_platform_keystore()?;
    run_unenroll(&keychain, confirm)
}

fn run_unenroll(
    keychain: &dyn reeve_runtime::OperatorKeyStore,
    confirm: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = IdentityRegistry::default_data_dir()?;
    let registry = IdentityRegistry::open(data_dir.clone())?;
    let audit = AuditLog::open(data_dir)?;

    match identity::unenroll(&registry, keychain, &audit, confirm) {
        Ok(identity_id) => {
            writeln!(
                io::stdout().lock(),
                "Unenrolled operator identity {identity_id}.",
            )?;
            Ok(())
        }
        Err(identity::UnenrollError::AuditFailed(err)) => {
            // Unenrollment succeeded; warn on stderr and exit 0.
            writeln!(
                io::stderr().lock(),
                "warning: audit append failed (unenrollment already complete): {err}",
            )?;
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

fn cmd_envelope_sign(to: &str, body: &str) -> Result<(), Box<dyn std::error::Error>> {
    let keychain = keychain::open_platform_keystore()?;
    run_envelope_sign(&keychain, to, body)
}

fn run_envelope_sign(
    keychain: &dyn reeve_runtime::OperatorKeyStore,
    to: &str,
    body: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let recipient_id = parse_identity_id(to)?;
    let registry = IdentityRegistry::open(IdentityRegistry::default_data_dir()?)?;
    envelope::sign(
        &registry,
        keychain,
        recipient_id,
        body.as_bytes(),
        &mut io::stdout().lock(),
    )?;
    Ok(())
}

fn cmd_envelope_verify(file: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let registry = IdentityRegistry::open(IdentityRegistry::default_data_dir()?)?;
    envelope::verify_from_path(&registry, file, &mut io::stdout().lock())?;
    Ok(())
}

/// Parse the `--to` argv value into a typed [`IdentityId`] at the CLI boundary.
fn parse_identity_id(s: &str) -> Result<IdentityId, Box<dyn std::error::Error>> {
    let uuid: uuid::Uuid = s.parse()?;
    Ok(IdentityId::try_from(uuid)?)
}

fn prompt_display_name() -> Result<String, Box<dyn std::error::Error>> {
    prompt::prompt_one_line("Display name: ", "display name must not be empty")
}
