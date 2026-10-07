# podmangr

A fast, **vim-keyed TUI for local Podman** — containers *and* pods — because function keys mean two hands. 🙂 Rust + [ratatui](https://ratatui.rs).

- **Containers** via [bollard](https://crates.io/crates/bollard) over the rootless Podman socket (Podman speaks the Docker API).
- **Pods** via the `podman` CLI (pods are Podman-specific — not in the Docker API).
- Full lifecycle: **start / stop / remove**, **logs**, and a **create** form that can write a Quadlet unit for autostart + dependencies.
- **Projects**: the systemd user units that run compose projects or quadlets, so a whole project starts and stops through its unit, and stays reachable even when its containers are stopped or gone.

## Why

Other container TUIs lean on function keys or can't create with resource limits/dependencies. podmangr is one-handed vim keys, and its create form maps straight onto Podman's real features (`--memory/--cpus`, `--requires`, and Quadlet for boot-autostart).

## Layout

```
 Containers | Pods | Projects                                                (Tab)
┌ podmangr — containers ─────────────────────────────────────────────────────────────┐
│   NAME                IMAGE                STATE    STATUS   OWNER                │
│ › dev-postgres_1      postgres:17          running  Up 3h    dev-postgres.service │
│   penpot-frontend_1   penpotapp/frontend   running  Up 2h    penpot.service       │
│   caddy               caddy:2.10           running  Up 2h    caddy.service        │
└────────────────────────────────────────────────────────────────────────────────────┘
 j/k move · s start · x stop · X rm · l logs · n new · r refresh · q quit
```

`Tab` cycles **Containers → Pods → Projects**; state is colour-coded (running = green, stopped/exited = red, paused = yellow).

## Build & run

Requires a stable Rust toolchain and the rootless Podman **API socket**:

```bash
systemctl --user enable --now podman.socket     # one-time
cargo run                                        # dev
cargo build --release                            # ./target/release/podmangr
```

It connects to `$XDG_RUNTIME_DIR/podman/podman.sock`.

## Keys

| Key | Action |
|-----|--------|
| `Tab` | switch Containers → Pods → Projects |
| `j` / `k` (or ↓/↑) | move selection |
| `g` / `G` | first / last |
| `s` | start (container; pod on Pods; whole project on Projects) |
| `x` | stop — a container a systemd unit runs asks first (see below) |
| `X` | remove — asks **y / N** to confirm |
| `l` | logs (container) |
| `n` | new container (form) |
| `r` | refresh |
| `t` | cycle colour theme (dark / light / solarized / gruvbox) — saved |
| `q` | quit |

**Logs viewer:** `j`/`k` scroll, `PgUp`/`PgDn` page, `g`/`G` top/bottom, `q`/`Esc` back.
**Create form:** `Tab`/`↑↓` move fields, `space` toggles Autostart, `Enter` submits, `Esc` cancels.

## Screens

### Containers
Lifecycle via the bollard API: `s` start, `x` stop, `X` remove (with a y/N confirm modal). `l` opens a scrollable log view (last ~1000 lines).

The **OWNER** column names the systemd unit that runs a container's project: the unit in its `PODMAN_SYSTEMD_UNIT` label when that unit exists (quadlets), else the compose unit whose `WorkingDirectory` is the container's compose project folder. (podman-compose always labels `podman-compose@<project>.service`, which usually isn't the unit that runs it.) A container from a compose project no unit runs shows `compose: <project>`.

**Stopping (`x`) or removing (`X`) a container a unit owns asks first**: `c` just this container, `u` the whole project through its unit, `n`/`Esc` cancel. If the unit runs `podman compose up` in the foreground, the box warns in red that stopping one container ends the unit and stops the rest, and if it stops with `compose down`, that they are all removed. That is how a single stopped Penpot container once took the whole stack with it.

### Pods
Driven by the `podman` CLI (`podman pod ps/start/stop/rm`), since pods aren't part of the Docker API. `s`/`x`/`X` act on the selected pod.

### Projects
The systemd user units that run a compose project or a quadlet (`podman run`, `kube play`, `pod start`), with state, containers up, and folder. `s`/`x` run `systemctl --user start`/`stop` on the unit, so the whole project starts and stops together. A stopped unit stays listed even when its containers are stopped or gone, so the project can always be started again from here. The NOTE column flags units whose design takes the project down when one container stops. Podman's own services and its healthcheck units aren't projects and aren't listed.

## Creating containers (`n`)

Fill the form; behaviour depends on what you set:

- **Plain `podman run -d`** — when *Autostart* is off and *Depends* is empty. Applies `--name`, `--network`, `-p` (ports), `--memory`, `--cpus`, `-v` (volume), and `--requires` if you name a container.
- **Quadlet unit** — when *Autostart* is on **or** *Depends* is set. Writes `~/.config/containers/systemd/<name>.container` (limits via `PodmanArgs=`, `Volume=`, `[Unit] After=/Requires=<dep>`, `[Install] WantedBy=default.target`), then `systemctl --user daemon-reload` and starts it. This gives **boot-autostart** and **start-order dependencies**. *Depends* expects a systemd unit (e.g. `dev-postgres.service`, or another Quadlet's `<name>.service`).
- **Disk** is handled with the Volume field (a named volume) rather than a hard size cap (not available on ext4).

## What it touches (safety)

- **Reads/controls** containers through the rootless Podman socket; **nothing is removed without a y/N confirm**.
- **Projects** are read with `systemctl --user list-unit-files`/`list-units`/`show`, and started or stopped with `systemctl --user start`/`stop`. It never edits a unit.
- **Pods** are managed by shelling out to `podman`.
- **Create** either runs `podman run` or writes a Quadlet `.container` file under `~/.config/containers/systemd/` and starts the unit.
- No daemon, no root — it's all rootless Podman as your user.

## Themes

Four colour schemes — `dark`, `light`, `solarized`, `gruvbox` — cycle with `t`. Your choice is saved to `~/.config/podmangr/theme` and restored on next launch.

## Roadmap

- [x] Containers: list + start/stop/remove (+ confirm) + logs
- [x] Pods screen (start/stop/rm)
- [x] Projects screen and container owners (systemd units)
- [x] Create form (limits, volume, autostart, dependencies via run/Quadlet)
- [ ] Exec shell into a container (`e`)
- [ ] Live stats (CPU/mem)
- [ ] Volumes / images / networks screens
- [ ] `/` filter & search

## Notes

- `ratatui::init()`/`restore()` install a panic hook so the terminal is always restored.
- crossterm is used via `ratatui::crossterm`, so its version always matches ratatui's.
- Pin deps / prefer `cargo install --locked` as this grows (see `~/Documents/TechDocs/rust-tricks.odt`).

## License

See [LICENSE](LICENSE).
