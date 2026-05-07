# Reeve

Reeve runs AI coding agents as named, addressable, supervised actors on a
developer's workstation. It handles message signing and verification, agent
supervision and crash recovery, and a filesystem-based TUI that attaches and
detaches without interrupting the runtime. See `specs/reeve-overview.md` for
the full design.

## Quick start

```bash
# Prerequisites: Rust stable, OS keychain (macOS Keychain / Linux Secret Service)

# Build
cargo build --release

# First run — enrollment, daemon start, and TUI attach in one command
./target/release/reeve
```

On a fresh machine `reeve` will:

1. Prompt for a display name and generate an ed25519 keypair (stored in the OS
   keychain).
2. Check for an Anthropic API key — if absent, print a hint and exit. Add one
   with `reeve adapter set-key`, then run `reeve` again.
3. Start the runtime daemon in the background.
4. Launch the TUI connected to the lead agent.

Closing the TUI does not stop the daemon. Run `reeve attach` to reconnect.

## CLI

```
reeve                            first-run setup + attach TUI
reeve attach                     attach TUI to running daemon
reeve daemon start|stop|status   daemon lifecycle
reeve identity enroll|list|unenroll
                                 operator identity management
reeve adapter set-key|test       model adapter configuration
```

## Architecture

See `architecture.md`.

## Development

Standard Rust workspace:

```bash
cargo build --workspace
cargo test --workspace
```
