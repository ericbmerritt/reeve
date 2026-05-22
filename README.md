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
reeve daemon logs [-f] [-n N]    print or follow the daemon log
reeve identity enroll|list|unenroll
                                 operator identity management
reeve adapter set-key|test       model adapter configuration
```

## Logging

The daemon process writes structured (tracing) output to
`$XDG_STATE_HOME/reeve/daemon.log` (default `~/.local/state/reeve/daemon.log`).
The same file is also where pre-subscriber stderr from daemon startup lands,
so it is the single place to look when a daemon refuses to start or a
running daemon misbehaves. `reeve daemon status` and `reeve daemon start`
print the path on every invocation; `reeve daemon logs -f` tails it.

The default filter is `reeve=debug,warn` — `debug` for first-party crates,
`warn` for everything else. Override with `REEVE_LOG`, which accepts the
standard [`tracing-subscriber` EnvFilter] syntax:

```bash
# turn everything in the runtime up to trace
REEVE_LOG=trace ./target/release/reeve

# debug just the dispatcher and watcher, warn elsewhere
REEVE_LOG="reeve_runtime::dispatcher=debug,reeve_runtime::watcher=debug,warn" \
  ./target/release/reeve
```

`REEVE_LOG` is read at daemon start, so changing it requires
`reeve daemon stop && reeve daemon start` to take effect on the running
process.

[`tracing-subscriber` EnvFilter]: https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html

## Architecture

See `architecture.md`.

## Development

Standard Rust workspace:

```bash
cargo build --workspace
cargo test --workspace
```
