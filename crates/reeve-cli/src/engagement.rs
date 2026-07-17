//! `reeve engagement` subcommands.
//!
//! Mutations (`open`, `close`, `reopen`) are signed operator envelopes
//! deposited to the estate coordinator's inbox — the same transport path as
//! `reeve send`, pointed at the reserved `estate` recipient. There is no
//! reply channel: after depositing, the command polls the durable engagement
//! record for the expected effect and reports it, so success output means
//! the operation actually landed, not merely that a file was written.
//! `list` is a plain read of the durable store and works without a daemon.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Subcommand;
use reeve_runtime::engagement::{resolve_vcs_toplevel, EngagementRecord, EngagementState};
use reeve_runtime::{EngagementRegistry, EstateOp, RuntimeLayout, ESTATE_AGENT_NAME};

use crate::keychain;
use crate::send;

/// How long to wait for the daemon to apply a deposited operation before
/// telling the operator to check the audit log.
const APPLY_TIMEOUT: Duration = Duration::from_secs(5);
const APPLY_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Subcommand, Debug)]
pub(crate) enum EngagementSubcommand {
    /// Open a new engagement. The working root defaults to the VCS toplevel
    /// (jj/git) of the current directory.
    Open {
        /// Engagement name, unique per estate, never reused.
        #[arg(long)]
        name: String,
        /// What the work is, in prose.
        #[arg(long)]
        purpose: String,
        /// Explicit working root; overrides VCS-toplevel resolution.
        #[arg(long, conflicts_with = "no_root")]
        root: Option<PathBuf>,
        /// Open with no working root (rootless work — research, planning).
        #[arg(long)]
        no_root: bool,
    },
    /// Close an open engagement. The record persists; the name is never
    /// reused.
    Close {
        #[arg(long)]
        name: String,
    },
    /// Reopen a closed engagement with its recorded context intact.
    Reopen {
        #[arg(long)]
        name: String,
    },
    /// List all engagements.
    List,
}

pub(crate) fn dispatch(command: EngagementSubcommand) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        EngagementSubcommand::Open {
            name,
            purpose,
            root,
            no_root,
        } => {
            let resolved_root = if no_root {
                None
            } else {
                match root {
                    Some(explicit) => Some(std::path::absolute(explicit)?),
                    None => Some(resolve_vcs_toplevel(&std::env::current_dir()?)?),
                }
            };
            let op = EstateOp::OpenEngagement {
                name: name.clone(),
                purpose,
                root: resolved_root,
            };
            send_and_await(&op, &name, |record| record.state == EngagementState::Open)
        }
        EngagementSubcommand::Close { name } => send_and_await(
            &EstateOp::CloseEngagement { name: name.clone() },
            &name,
            |record| record.state == EngagementState::Closed,
        ),
        EngagementSubcommand::Reopen { name } => send_and_await(
            &EstateOp::ReopenEngagement { name: name.clone() },
            &name,
            |record| record.state == EngagementState::Open,
        ),
        EngagementSubcommand::List => cmd_list(),
    }
}

fn engagement_registry() -> Result<EngagementRegistry, Box<dyn std::error::Error>> {
    let root = reeve_runtime::default_data_root()?;
    Ok(EngagementRegistry::open(
        RuntimeLayout::new(root).engagements_root(),
    )?)
}

/// Deposit the signed operation envelope to the estate inbox, then poll the
/// durable record until `applied` observes the expected effect.
fn send_and_await(
    op: &EstateOp,
    name: &str,
    applied: impl Fn(&EngagementRecord) -> bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let state_dir = reeve_runtime::runtime_lock::default_state_dir()?;
    if !reeve_runtime::heartbeat_fresh(&state_dir) {
        return Err("no runtime found, run `reeve daemon start` first".into());
    }

    let id_registry = reeve_runtime::IdentityRegistry::open(
        reeve_runtime::IdentityRegistry::default_data_dir()?,
    )?;
    let agent_registry_path = reeve_runtime::AgentRegistry::default_registry_path()?;
    let system_registry_path =
        RuntimeLayout::new(reeve_runtime::default_data_root()?).system_registry_path();
    let keychain = keychain::open_platform_keystore()?;
    let body = serde_json::to_vec(op)?;
    let mut sent_line = Vec::new();
    send::send(
        &id_registry,
        &agent_registry_path,
        &system_registry_path,
        &keychain,
        ESTATE_AGENT_NAME,
        &body,
        &mut sent_line,
    )?;

    let registry = engagement_registry()?;
    let deadline = Instant::now() + APPLY_TIMEOUT;
    loop {
        if let Ok(record) = registry.get(name) {
            if applied(&record) {
                writeln!(
                    std::io::stdout().lock(),
                    "{}: {} {}",
                    record.name,
                    state_label(record.state),
                    record
                        .root
                        .as_deref()
                        .map(|p| format!("(root: {})", p.display()))
                        .unwrap_or_else(|| "(rootless)".to_owned()),
                )?;
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "operation deposited but not applied within {APPLY_TIMEOUT:?}; \
                 the daemon may have refused it — check `reeve engagement list` \
                 and the audit log for engagement.op_refused"
            )
            .into());
        }
        std::thread::sleep(APPLY_POLL_INTERVAL);
    }
}

fn state_label(state: EngagementState) -> &'static str {
    match state {
        EngagementState::Open => "open",
        EngagementState::Closed => "closed",
    }
}

fn cmd_list() -> Result<(), Box<dyn std::error::Error>> {
    let registry = engagement_registry()?;
    let records = registry.list()?;
    let mut out = std::io::stdout().lock();
    if records.is_empty() {
        writeln!(out, "no engagements")?;
        return Ok(());
    }
    for record in records {
        writeln!(
            out,
            "{:<24} {:<7} {}",
            record.name,
            state_label(record.state),
            record
                .root
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "-".to_owned()),
        )?;
    }
    Ok(())
}
