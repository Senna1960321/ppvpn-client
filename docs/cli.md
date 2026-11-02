# `ppvpn` command-line client

`crates/cli` builds the `ppvpn` binary: a terminal client that logs in through the browser and runs
ppvpn-core's standard instance in its own process, exposing the authenticated local HTTP/SOCKS5 proxy. It
does not create a TUN device or change system network settings.

Status: every command is wired to `ppvpn-account` and to ppvpn-core's `Engine`. What a command returns
follows the engine: while an engine method is not implemented yet, the command reports core's error
(`CORE_OPERATION_FAILED`, exit 5).

## Account

`ppvpn login` starts a device authorization, prints the authorization URL and the confirmation code to
stderr, opens the URL in the default browser unless `--no-browser` is given, and waits until the
authorization is confirmed, denied or expired. A CLI login is its own device session, separate from the
desktop app's: the CLI sends no product header and only accepts access tokens whose audience is `cli`.

The device credential is kept in the platform secret store and nowhere else:

- macOS: a generic password in the login Keychain (service `com.peakpassvpn.ppvpn.cli`);
- Linux: an item in the Secret Service default collection (GNOME Keyring, KWallet, …), written over an
  encrypted session.

There is no plain-file fallback. Where no secret store is available (for example a server without a Secret
Service) or the keyring stays locked, the CLI reports `CREDENTIAL_STORE_UNAVAILABLE` or
`CREDENTIAL_STORE_LOCKED` (exit 8) and does not log in. A locked keyring is asked to unlock (its own prompt);
when that is not answered within 60 seconds, the keyring counts as locked. Before it starts a device authorization,
`login` reads the store and writes and removes an item of its own (`com.peakpassvpn.ppvpn.cli.probe`), so a store
that cannot be used fails at once rather than after the code was confirmed: the macOS login Keychain, for one, answers
reads but refuses writes in an SSH session. Access tokens are never written to disk.

`ppvpn account` prints the authorized account and its current team (`--json`: `team`, or `null` when the
team list cannot be read, as from a disabled team). `ppvpn logout` revokes the device session and removes the
local credential. Without a saved login, commands that need one report `NOT_LOGGED_IN` (exit 3) before any
request is made.

`ppvpn start` downloads the account's proxy profile before it touches the daemon; a rejected access token is
refreshed once. The profile is passed to core as received and is never written to disk.

The backend serves the profile of the account's current team, and every access token is issued for that
team. It is the same team as in the dashboard and the desktop app, so switching it there switches the CLI
at its next command. `ppvpn team` lists the teams with the current one marked `*` and personal teams marked
`personal` (`--json`: `teams`, each with `id`, `name`, `personal`, `current` and `status`).
`ppvpn team use <id|name>` switches: an exact team ID, or a name only one team has (`TEAM_NOT_FOUND` and
`TEAM_AMBIGUOUS` are exit 2; a disabled team is `TEAM_DISABLED`, exit 3). The switch changes the user's
default team on the backend, not just the CLI's: the dashboard and the desktop app follow it at their next
refresh. A running proxy keeps the old team's profile until `ppvpn restart`; `--json` reports that as
`"restart_required": true`.

When the profile download is refused, the message adds the backend's own explanation (its problem `detail`,
when it says more than the CLI's message) and the current team, for example
`no active subscription (backend: <detail>; team: Personal (t-1)); ppvpn team lists your teams`.

## Daemon

`ppvpn start` runs a per-user daemon in the background (`ppvpn daemon`, in its own session) that hosts
ppvpn-core's standard instance: the local proxy only, no TUN and no system proxy. `start --foreground` runs
the same daemon in the CLI's own process until Ctrl+C or SIGTERM. Other commands talk to it over a private
control channel:

- a Unix socket (`0600`, in the `0700` runtime directory) carrying one JSON request and one JSON response per
  line; every request carries the session secret from `session.secret` (`0600`), created fresh by each daemon;
- the protocol is private to one CLI version (client and daemon are the same binary);
- `daemon.lock` (flock) allows one daemon per user; a second one exits with `DAEMON_ALREADY_RUNNING`;
- `daemon.json` records PID, executable and start time; `status` and `stop` only trust a record whose process
  still matches all three, so a record left by a crash or power loss is ignored and cleaned up;
- `stop` asks the daemon to shut core down (at most 10 seconds) and waits for it to exit; if the control
  channel is gone, it sends SIGTERM to the verified process;
- if `start` launched a daemon and then failed to apply or start, it stops that daemon again.

Errors from core keep their code; `--json` also includes core's `field` when there is one. Core's runtime codes
map to exits 2 (`NODE_NOT_FOUND`, `INGRESS_NOT_FOUND`, `PINS_INVALID`, `ROUTING_MODE_INVALID`), 5
(`PROFILE_NOT_APPLIED`, `CORE_*`, `ENGINE_*`, `NO_DEFAULT_INTERFACE`), 6 (proxy or TUN features unavailable)
and 8 (`STATE_DIR_IN_USE`, `PERMISSION_DENIED`, `TUN_NAME_TAKEN`); every other core code is a profile or
request validation failure (exit 7).

A `dev` build reads the profile from the absolute path in `PPVPN_PROFILE_FILE` when that variable is set,
instead of downloading it; release builds ignore the variable.

`core.log` (in the runtime directory, `0600`) has core's and sail's log lines at the `info` level; nothing in
it is a credential. Core only appends to it, so the daemon rotates it when it starts: a log over 8 MiB becomes
`core.log.1`, replacing the previous one. `daemon.log` is the background daemon's own stdout and stderr.

`status` (and `start`) show the local proxy's address. When core had to replace the local proxy credential as
this instance started (a damaged state file, or one that other users could read), they also say so for as long
as the instance runs, because applications that were given the old credential need the new one; `--json` has
core's `local_proxy.credentials_reset` as it is.

## The running instance

`nodes`, `use`, `probe`, `traffic`, `connections`, `proxy`, `ingress` and its subcommands talk to the daemon.
Without one they report `CORE_NOT_RUNNING` (exit 5, retryable: it works after `ppvpn start`); with a daemon that has no profile, selections and pins
report core's `PROFILE_NOT_APPLIED`.

- `nodes` lists the nodes with the selected one marked `*`. A node name that starts with a flag emoji (the backend's
  "🇺🇸洛杉矶家宽 203.0.113.10 ①") shows the flag's two letters instead (`US 洛杉矶家宽 203.0.113.10 ①`),
  because terminals draw flags at unsteady widths; `--json` has the name unchanged. Names are never cut, so a
  node with an IPv6 address has a long one; the columns are separated by tabs. Commands take the node ID, not
  the name.
- `use <node-id>` selects the node of new connections; open connections stay where they are. `ingress pin`
  takes effect at once and turns failover off for that node; `ingress auto` turns it back on. Core does not
  persist these choices: the CLI saves them to `settings.json` only after core accepted them, and passes them
  with the next apply.
- `mode <rules|global>` applies the profile the daemon holds again with the new mode, then saves it; `--json`
  reports `"applied": true`. With nothing running, the mode is saved for the next start (`"applied": false`).
  When the held profile has expired, the CLI downloads a new one and applies that with the new mode. When
  core refuses the change, the saved mode is not changed. The daemon holds the profile in memory only.
- `probe` measures TCP entrances (`--all`, or one node) or fetches `--target` through one node
  (`--type availability`). A probe that ran and failed is a result (`"success": false` with an `error_code`),
  not a command error; the exit code is 0.
- `proxy` lists the local proxy endpoints without secrets. `proxy credential` prints the routed credential
  (the routing mode and rules decide the node), `proxy credential <node-id>` a credential that always uses
  that node. Both print the username, the password and ready-made `http://` and `socks5h://` URLs
  (`http_url` and `socks5_url` with `--json`); these are the only commands whose output contains a secret.
- `ingress [node-id]` shows each node's pin and its ingresses' health; `*` marks the ingress in use.

Core reads ingress health, traffic and connections from the running proxy only while someone asks, about once
a second. A command asks once, so for `status`, `ingress`, `traffic` and `connections` the daemon waits for
core's next reading (up to 1.5 seconds after a quiet period) instead of answering with the values from before
it; `traffic` has `measured_at`, the time of the reading.

With `--json`, the fields are core's (`docs/host-integration.md`, sections 4 and 5) under `"ok": true`:
`nodes` gives `selected_node_id` and `nodes`; `traffic` gives `upload_bytes`, `download_bytes` and
`measured_at`; `connections`, `proxy` and `ingress` give `connections`, `endpoints` and `nodes`; `probe`
gives `type` and `results` (entrance) or `result` (availability).

## Commands

```text
ppvpn [--json] [--no-color] <command>
```

| Command | Purpose |
| --- | --- |
| `login [--no-browser]` | authorize this device in a browser |
| `logout` | revoke and remove this device's credential |
| `account` | show the authorized account and its current team |
| `team` / `team use <id\|name>` | list the account's teams; switch to one |
| `start [--foreground]` / `restart [--foreground]` / `stop` | run or stop the local proxy |
| `status` | state, routing mode, selected ingress, rule sets |
| `nodes` / `use <node-id>` | list nodes; select the node for new connections |
| `probe [node-id] [--all] [--type entrance\|availability] [--timeout 5s] [--concurrency 4] [--target URL]` | entrance or end-to-end probes |
| `traffic` / `connections` | cumulative traffic; active connections |
| `proxy` / `proxy credential [node-id]` | local proxy endpoints; the routed credential, or a node's |
| `mode [rules\|global]` | show or set the routing mode |
| `ingress [node-id]` / `ingress pin <node-id> <endpoint-key>` / `ingress auto <node-id>` | ingress health; pin or unpin |
| `doctor [--network]` | privacy-safe diagnostics; `--network` also checks reaching the API host: the system resolver's and AliDNS's and DNSPod's DoH answers, and TCP and TLS to each address (#521) |
| `version` / `completion <bash\|zsh\|fish>` | version; shell completion script |

Argument values are checked before any file, keychain, network or core access. For example, `probe` needs
either one node ID or `--all` for entrance probes and exactly one node ID for availability probes, a timeout
between 1 ms and 2 minutes, and a concurrency between 1 and 32.

## Output

- Without `--json`, results go to stdout and errors to stderr as `Error: <message>`.
- With `--json`, stdout carries exactly one JSON value per invocation, for errors too:
  `{"ok": false, "code": "<CODE>", "message": "<text>", "retryable": <bool>}`. Progress and warnings go to
  stderr.
- `code` is stable and upper-case; `message` is for people and may change.
- Byte counts in text output use decimal units, as the backend counts traffic: 1 KB = 1000 bytes, 1 MB = 10⁶,
  1 GB = 10⁹, 1 TB = 10¹² (`traffic` prints `400.0 GB (400000000000 bytes)`). JSON always has the exact byte count.

## Exit codes

| Exit | Meaning |
| --- | --- |
| 0 | success |
| 1 | other or internal error |
| 2 | invalid argument or build configuration |
| 3 | login missing, expired or not permitted |
| 4 | backend unavailable, untrusted, or nothing to serve for the account |
| 5 | core not running or a core operation failed |
| 6 | incompatible core or feature unavailable |
| 7 | the backend's profile could not be applied |
| 8 | local environment: files, directories, keychain |

Backend errors keep the backend's code. When the CLI cannot download a profile because of the
subscription, the exit is 4 and the code tells why: `SUBSCRIPTION_REQUIRED` (no active subscription),
`SUBSCRIPTION_EXPIRED`, or `PROXY_NODES_NOT_READY` (the subscription's nodes are not published yet;
`retryable` is true, try again in a few minutes). A plain `NOT_FOUND` covers the cases the backend does not
classify.

## Files

| | macOS | Linux |
| --- | --- | --- |
| settings | `~/Library/Application Support/ppvpn-cli/settings.json` | `$XDG_CONFIG_HOME/ppvpn-cli/settings.json` |
| runtime: control socket, session secret, process record, daemon lock, `daemon.log` and `core.log` | `~/Library/Application Support/ppvpn-cli/runtime/` | `$XDG_STATE_HOME/ppvpn-cli/runtime/` |
| core `state_dir` | `~/Library/Application Support/ppvpn-cli/state/` | `$XDG_STATE_HOME/ppvpn-cli/state/` |

Unset or relative XDG variables fall back to `~/.config` and `~/.local/state`. Directories are `0700` and
files `0600`. Linux deliberately avoids `XDG_RUNTIME_DIR`: it is cleared on reboot and may be removed at
logout, and core's state directory holds the local proxy username prefix, password and port that users copy
into other applications, so they must not change.

When the runtime directory is too long for a Unix socket address (`sun_path`: 104 bytes on macOS, 108 on
Linux), the control socket moves to a private, owner-checked `0700` directory `ppvpn-cli-<uid>/` under the
system temporary directory, named after a hash of the runtime directory.

The process record survives reboots, so a recorded process counts as the CLI's daemon only when its PID is
alive, runs the recorded executable, and is the process that was recorded: on Linux the same boot (the
kernel's boot ID) and the same start tick, which do not depend on the wall clock; on macOS a start within 10
seconds of the recorded time.

## Settings

`settings.json` holds the per-device choices that core does not persist and that the CLI passes with every
apply ([host integration](host-integration.md), section 4.1):

```json
{
  "routing_mode": "rules",
  "selected_node_id": "hk-001",
  "ingress_pins": { "hk-001": "9002" }
}
```

Unknown fields are ignored and an unknown `routing_mode` falls back to `rules`.

## Build

The backend is compiled in from `PPVPN_API_BASE_DEFAULT`; `tools/cli/build-release.sh` requires it, and
`release.yml` passes the backend (#517, see [Build and release](release.md#后端地址)).
No backend address is written in the source. Release builds connect to it and ignore overrides. A build made
with `PPVPN_BUILD_PROFILE=dev`, or one without a compiled-in backend (a local build), reads `PPVPN_API_BASE`
at run time (HTTPS only; plain HTTP only for loopback). `PPVPN_VERSION` sets the reported version.
Static Linux (musl) builds use mimalloc as the global allocator.

Releases carry the CLI as tar.gz archives beside the desktop installers, in the same GitHub Release (tag
`vX.Y.Z`, the crate's version): Linux x86_64 and aarch64 (static, musl) and macOS universal, built by
`tools/cli/build-release.sh` in `release.yml`. `crates/cli/README.md`, shipped in each archive, covers
installing and verifying them.

On macOS, the script defaults to building and packaging in one invocation. The release workflow sets
`PPVPN_CLI_STAGE=build` to compile unsigned slices before stable approval, then `PPVPN_CLI_STAGE=package`
to sign, notarize and archive those same slices without recompiling (see [Build and release](release.md#ci)).

The shipped binary has no symbols, so a backtrace shows addresses only. Each platform's symbols are a separate
file in the same release, `ppvpn-cli-<version>-<platform>.symbols.tar.gz`, under the same SHA256SUMS and build
provenance: `ppvpn.debug` (Linux) and `ppvpn.dSYM` (macOS). To turn a frame's address into a function name,
subtract the binary's load address (Linux: the first mapping of `ppvpn` in `/proc/<pid>/maps`) and run
`addr2line -f -e ppvpn.debug <offset>`; on macOS, `atos -arch <arm64|x86_64> -o
ppvpn.dSYM/Contents/Resources/DWARF/ppvpn -l <load address> <address>`. The names are Rust-mangled;
`rustfilt` demangles them.
