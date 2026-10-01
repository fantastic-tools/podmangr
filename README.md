# podmangr

A vim-keyed **TUI manager for local Podman containers** — because function keys mean two hands. 🙂

Rust + [ratatui](https://ratatui.rs) talking to the rootless Podman socket
(`$XDG_RUNTIME_DIR/podman/podman.sock`) via [bollard](https://crates.io/crates/bollard)
(Podman's Docker-compatible API).

## Build & run
```bash
cargo run              # dev
cargo build --release  # ./target/release/podmangr
```
Requires the rootless Podman API socket (already enabled here):
```bash
systemctl --user enable --now podman.socket
```
Override the socket with `XDG_RUNTIME_DIR`, or later a `PODMAN_SOCK` env (see roadmap).

## Keys (vim-style, one-handed)
| Key | Action |
|-----|--------|
| `j` / `k` (or ↓/↑) | move selection |
| `g` / `G` | first / last |
| `s` | start container |
| `x` | stop container |
| `X` | remove (force) — asks **y/N** to confirm |
| `Tab` | switch Containers ⇄ Pods |
| `n` | new container (form) |
| `l` | logs (container) — scroll j/k, PgUp/PgDn, g/G; q/Esc back |
| `r` | refresh |
| `q` | quit |

### Creating containers (`n`)
Fill the form (Tab/↑↓ to move, space toggles Autostart, Enter submits):
- **Plain run** when Autostart is off and Depends is empty → `podman run -d` with
  `--memory/--cpus/-v/-p/--network` (and `--requires` if you name a container).
- **Quadlet unit** when Autostart is on or Depends is set → writes
  `~/.config/containers/systemd/<name>.container` (Memory/CPUs via PodmanArgs,
  `Volume=`, `[Unit] After=/Requires=<dep>`, `[Install] WantedBy=default.target`),
  then `systemctl --user daemon-reload` + start. This gives boot-autostart and
  start-order dependencies. Depends expects a **systemd unit** (e.g. `dev-postgres.service`
  or another quadlet's `name.service`).
- **Disk**: use the Volume field (named volume) rather than a size cap (ext4 here).

## Status (MVP)
- [x] List all containers (name / image / state / status), colour-coded state
- [x] Start / stop / remove, refresh
- [x] Logs view (`l`) — scrollable, last 1000 lines
- [ ] Exec shell (`e`)
- [ ] Live stats (cpu/mem)
- [x] Create dialog (`n`): image, name, network, ports, **cpu/mem limits**, **volume**, **autostart**, **depends-on**
- [x] Pods screen (Tab) — start/stop/rm pods (via `podman` CLI)
- [ ] Volumes / images / networks screens
- [ ] Search/filter (`/`)
- [x] Confirm dialog before destroy (y/N)

## Notes
- Pods are Podman-specific (not in the Docker API), so the Pods screen uses the `podman` CLI; containers use the bollard API.
- `ratatui::init()`/`restore()` install a panic hook that restores the terminal.
- bollard is reached through `ratatui::crossterm` for terminal + events, so crossterm
  versions always match ratatui's.
- Pin dependencies / use `cargo install --locked` style discipline as this grows
  (see ~/Documents/TechDocs/rust-tricks.odt).
