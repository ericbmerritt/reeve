# Operations

## Prerequisites

- macOS or Linux
- [Nix](https://nixos.org/download) with flakes enabled
- An Anthropic API key

## Install

```bash
git clone https://github.com/ericbmerritt/reeve
cd reeve
nix develop          # enter the dev shell
cargo build --release
```

## Configure

Enroll the workstation operator identity:

```bash
reeve identity enroll
```

Store the Anthropic API key:

```bash
reeve adapter set-key
```

## Start the daemon

```bash
reeve daemon start
```

The daemon writes its PID and a heartbeat file to
`~/.local/state/reeve/`. Check status with:

```bash
reeve daemon status
```

## Attach the TUI

```bash
reeve              # attach or start from session memory
reeve attach       # same, explicit subcommand
reeve attach lead  # open lead's chat directly
```

### Screens

| Key                   | Action                   |
| --------------------- | ------------------------ |
| `Tab`                 | Toggle chat ↔ panopticon |
| `Enter` (panopticon)  | Open per-agent inspect   |
| `Q` (panopticon)      | Open quarantine review   |
| `h` / `Esc` (inspect) | Back to panopticon       |
| `q` / `Esc` (chat)    | Quit                     |

## Send a message without the TUI

```bash
reeve send --to lead --body "start the deeds refactor"
```

## Stop the daemon

```bash
reeve daemon stop
```

## Logs

The daemon log is at `~/.local/state/reeve/daemon.log`.
