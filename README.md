<p align="center">
  <h1 align="center">tv</h1>
</p>

![tv](https://github.com/user-attachments/assets/454c5f57-d8fc-4ce1-b27b-59ea4897afd5)

<p align="center">
  tv enables human-agent collaboration through shared terminal sessions. Agents
  interact through a simple CLI, humans stay in control through built-in
  permissions. No assumptions about the environment — if it works in a terminal,
  it works with tv.
</p>

## Install

```
cargo install --path .
```

## Quick Start

```bash
# Start a new session
tv new @work

# From another terminal (or an agent), interact with it
tv exec @work "ls -la" {cr} {wait:5s}
tv peek @work -j
tv history @work -n 100
```

## How It Works

tv runs a daemon that manages terminal sessions (PTY + terminal emulator).
Each session is a real terminal — shells, TUIs, SSH, Docker, everything works.
Agents interact with sessions through CLI commands over Unix socket IPC.

```
Agent                     tv daemon             Terminal sessions
  |                           |                       |
  |--- tv exec @s "cmd" ----->|--- PTY write -------->|
  |--- tv peek @s -j -------->|--- read screen ------>|
  |<-- JSON response ---------|                       |
```

## Agent Reference

```
tv llms
```

Prints a comprehensive reference document for AI agents. Feed this to your
agent's context and it will know how to use tv.

## Commands

| Command | Alias | Description |
|---------|-------|-------------|
| `tv new [@tag]` | `tv n` | Start a new terminal session |
| `tv exec <sel> <keys>` | `tv x` | Send keystrokes to a session |
| `tv peek [sel]` | `tv p` | Read current screen content |
| `tv history [sel]` | `tv h` | Read scrollback history |
| `tv ls [sel]` | | List active sessions |
| `tv tag [@tag]` | `tv t` | Get/set session tags |
| `tv show/hide [sel]` | | Control session visibility |
| `tv clear [sel]` | | Clear scrollback |

## Key Features

- **Persistent sessions** — sessions survive agent disconnects and restarts
- **Real terminal emulation** — full PTY with alacritty's terminal emulator
- **Smart wait** — `{wait:30s}` returns early when output settles, no guessing
- **Piped input** — `echo "content" | tv exec @s {stdin} {cr}` for multiline/special chars
- **Permission prompts** — user approves agent actions in the session UI
- **JSON output** — `tv peek -j` returns structured data with cursor position, dimensions, alt-screen status
- **Session discovery** — agents find sessions by tag or ID, see CWD and activity

## AI Disclaimer

AI assisted with parts of development (docs, key mappings, VT100 parser). All core code — auth, IPC, client/server/session — was manually written and verified.

## License

Apache-2.0
