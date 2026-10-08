# paper-claude-switch

**Multi-account switcher for Claude Code (CLI and VS Code extension), with automatic switching before you hit a usage limit.**

Save several Claude Code logins, see each account's 5-hour, 7-day and per-model weekly usage, switch with one command, and let `auto` move you to a fresh account before the active one runs dry.

> Inspired by [claude-swap](https://github.com/realiti4/claude-swap) and [paper-codex-switch](https://github.com/Cazorlas/Paper.Codex-switcher).

## Features

- Save, rename and recoverably delete Claude Code accounts; switch by name, by number, or to the best account automatically.
- Usage dashboard (`list` and interactive `tui`) for 5-hour, 7-day and per-model weekly usage.
- **`auto`**: loop that switches accounts when the active one nears its limit (leave the window open, minimized).
- **`launch`**: start `claude` on the best account.
- Switch the account used by both Claude Code CLI and its VS Code extension without restarting.
- Windows and Linux. macOS Keychain credentials are not supported yet.

## Requirements

- Claude Code installed and logged in, with `claude` on `PATH`.
- Windows or Linux, with credentials in `~/.claude/.credentials.json` (or `.credentials.json` inside `$CLAUDE_CONFIG_DIR`).
- macOS (Keychain) is not supported yet.

## Install

```bash
npm i -g paper-claude-switch
```

This downloads the prebuilt binary for your platform from [GitHub Releases](https://github.com/Cazorlas/Paper.Claude-switcher/releases). Node 18+ is required for the npm launcher. Credential switching currently supports Windows and Linux.

Or build from source (Rust 1.88+):

```bash
cargo install --git https://github.com/Cazorlas/Paper.Claude-switcher
```

Data lives in `~/.paper-claude-switch` (override with `PAPER_CLAUDE_SWITCH_HOME`). Update with `paper-claude-switch self-update`.

## Setup (first time)

1. Install Claude Code and log in with your first account.
2. Install paper-claude-switch (see Install) and check everything with:
   ```bash
   paper-claude-switch doctor
   ```
3. Save the first login:
   ```bash
   paper-claude-switch login personal
   ```
4. In Claude Code, run `/login` and sign in with your second account. **Do not run `/logout` first:** Claude Code may revoke the refresh token stored for the first account.
5. Save the second login:
   ```bash
   paper-claude-switch login work
   ```
6. Check them: `paper-claude-switch list`. If the live Claude Code account has not been saved, `list` offers to save it.

## Everyday use

```bash
paper-claude-switch list           # numbered usage dashboard
paper-claude-switch use            # switch to the best account
paper-claude-switch use 2          # switch to account number 2 in `list`
paper-claude-switch use work       # ...or by alias
paper-claude-switch launch         # start claude on the best account
paper-claude-switch tui            # interactive dashboard
paper-claude-switch auto           # switch automatically near the limit (see below)
```

No restart needed: Claude Code re-reads its credential file when it changes, so a running CLI or VS Code session uses the new account on its next message.

`use <n>` uses the numbers shown by `list` (alphabetical by alias); a profile literally named `2` wins over position 2.

## Manage accounts

| Task | Command |
|---|---|
| Save the current Claude Code login | `paper-claude-switch login [alias]` |
| Save a renewed login after running `/login` in Claude Code | `paper-claude-switch login <existing alias>` |
| Rename | `paper-claude-switch rename <old> <new>` |
| Delete (archived, recoverable; the active account can't be deleted) | `paper-claude-switch delete <alias> [--yes]` |
| List deleted accounts / bring one back | `paper-claude-switch restore` / `paper-claude-switch restore <alias> [--as <new>]` |
| Import the accounts Orca manages | `paper-claude-switch import-orca [--dry-run]` |
| Refresh usage now, ignoring the cache | `paper-claude-switch list --force` |
| Open the data folder | `paper-claude-switch open` |
| Check Claude Code version and setup | `paper-claude-switch doctor` |

Global flags: `--json` / `--json-pretty` (machine-readable output), `--proxy <url>`, `--color always|never`, `--debug`. Settings live in `~/.paper-claude-switch/config.toml` (also editable in the `tui` Settings tab).

To remove accounts you no longer use: `list`, then `delete <alias>`. Deleting is not final: `restore` lists what was deleted and `restore <alias>` brings the newest archive back (use `--as` if the name is taken). Archives live in `~/.paper-claude-switch/deleted-profiles`. `auto` only switches between saved accounts, so deleting an account also takes it out of rotation.

### Using with Orca

1. In Orca, set the Claude account to **System default**, so Orca runs Claude Code on `~/.claude`, the login paper-claude-switch switches.
2. Run `paper-claude-switch import-orca` once (add `--dry-run` to preview). It copies each Orca account into a profile: new accounts are created, a saved account is updated only when Orca holds the fresher login, and the rest are left as they are. Orca's own files are never changed. Use `--from <dir>` if Orca keeps its accounts somewhere else (default: `orca/claude-accounts` in your config folder).
3. Remove those accounts in Orca (Settings > Accounts), so only one app refreshes each login.

### Update / uninstall

```bash
paper-claude-switch self-update --check   # is there a newer version?
paper-claude-switch self-update           # update (npm installs)
```

Once a day the command looks for a newer version in the background and prints a one-line hint when there is one; it never installs by itself. `self-update` closes a running `auto` (Windows locks the running `.exe`) and runs `npm i -g paper-claude-switch@latest`; start `auto` again afterwards. Installed with cargo? Re-run `cargo install --git https://github.com/Cazorlas/Paper.Claude-switcher`.

### Uninstall

```bash
paper-claude-switch uninstall              # closes auto, removes the npm package,
                                           # then asks whether to delete your saved accounts [y/N]
paper-claude-switch uninstall --purge      # ...and delete the accounts and settings without asking
paper-claude-switch uninstall --keep-data  # ...keep them without asking
```

This works even if Windows Security has blocked the program, because it runs from the npm launcher. Installed with cargo instead? `cargo uninstall paper-claude-switch`, then delete `~/.paper-claude-switch` if you want the data gone too.

### Windows Security says "Trojan:Win32/...!ml"?

The `.exe` is not code-signed yet, and Microsoft Defender's machine-learning detection sometimes flags new unsigned programs. You can check the file against the SHA256 shown on the release page. If Defender quarantines it: Windows Security → Protection history → *Allow on device*, then re-run the command (or `npm i -g paper-claude-switch@latest`).

## Automatic switching

```bash
paper-claude-switch auto                    # foreground loop, checks every 60s
paper-claude-switch auto --threshold 80     # switch earlier (default 90)
paper-claude-switch auto --dry-run          # log what it would do, never switch
paper-claude-switch --json auto --once      # one check, for cron / Task Scheduler
```

| Option | Default | Meaning |
|---|---|---|
| `--threshold` | 90 | switch when the 5h or 7d window reaches this used % |
| `--margin` | 10 | target must be at least this many points below the active account |
| `--interval` | 60 | seconds between checks |
| `--cooldown` | 300 | minimum seconds between two switches |
| `--once` | off | single check, then exit |
| `--dry-run` | off | report only |

How it decides:

1. Every `--interval` it checks only the active account. Below the threshold: nothing else happens (no traffic for the other accounts).
2. At or above the threshold it checks saved accounts, ranks them with the same scoring as `use`, and picks the best eligible one under the threshold and `--margin` points lower.
3. It switches the credentials used by Claude Code; the next message uses the new account.
4. If every account is exhausted it backs off to a 10-minute cadence. Usage-check errors keep the current account and retry.

`--once` exit codes: `0` switched, `1` error, `2` nothing to do, `3` blocked (no viable target). With `--json` each event is one JSON line.

### Keeping it running

`auto` runs in the terminal window where you start it. Leave that window open and minimize it; it checks quietly and prints a line only when something happens or fails. Closing the window stops it, and starting it again is just `paper-claude-switch auto`.

There is no hidden background service: nothing registers itself to start with Windows. To run it unattended, use a single check from your scheduler (cron, Windows Task Scheduler):

```bash
paper-claude-switch auto --once --json
```

## Status line

Claude Code can hand the active account's 5h and 7d usage to a status line command. `paper-claude-switch statusline` stores those numbers in its usage cache, so `list`, the `tui` and `auto` read the active account from the cache instead of asking the usage endpoint while Claude Code is running. It never uses the network.

In `~/.claude/settings.json`, put it in front of your existing status line command (everything after `--` is run with the same input, and its output is shown unchanged):

```json
"statusLine": {
  "type": "command",
  "command": "paper-claude-switch statusline -- <your existing status line command>"
}
```

If your status line is a shell script (Orca writes one), put this in front of it in a pipe instead; `--tee` passes the input on unchanged, and `|| cat` keeps your status line working if paper-claude-switch is missing:

```json
"command": "{ paper-claude-switch statusline --tee 2>/dev/null || cat; } | { <your existing shell command>; }"
```

Orca may rewrite its status line when it updates its hooks; re-apply this line then.

Without a command after `--` it prints one line, for example `personal 5h 38% · 7d 12%`. If anything goes wrong, the status line still shows the next command's output (or nothing).

## Resets column

Claude Max gives a number of session-limit resets each week (claimed in Claude Code with `/rate-limit-options` when you hit the 5-hour wall). The Resets column shows how many are left: `1`, `1 ready` at the wall, `0 → MM-DD HH:MM` when used until that time, `n/a` when the plan has none, and `?` when Anthropic did not say.

Anthropic only reveals this to Claude Code itself, so the usage request identifies as Claude Code (`User-Agent: claude-cli/<version>`, `x-app: cli`). If Anthropic starts expecting a newer Claude Code version, set `CS_CLAUDE_CODE_VERSION` (for example to the output of `claude --version`) until paper-claude-switch is updated.

## How it works

A switch replaces only `claudeAiOauth` in the credentials file and `oauthAccount` in `~/.claude.json`, keeping MCP logins and every other setting. It holds Claude Code's own lock directories while writing.

The switcher writes back the rotated token of the account it leaves. It never refreshes the active account's token, because that would log out the running Claude Code. It refreshes saved inactive accounts when needed and stores each rotated token immediately.

## Security

This tool manages local credential files. Never publish profiles, `.credentials.json`, tokens or unredacted debug output. Treat the saved accounts in `~/.paper-claude-switch` as credentials too.

## Development

```bash
cargo build && cargo test
```

Releases: push a `v<version>` tag; `.github/workflows/release.yml` builds the binaries, attaches them to the release and publishes `npm/`.

## License

MIT, © Cazorlas — see `LICENSE`. Parts are adapted from [paper-codex-switch](https://github.com/Cazorlas/Paper.Codex-switcher), [codex-switch](https://github.com/xjoker/codex-switch) and [claude-swap](https://github.com/realiti4/claude-swap) (MIT); see `THIRD_PARTY_NOTICES.md`.
