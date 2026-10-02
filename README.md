# podmangr

A fast, **vim-keyed TUI for local Podman** — containers *and* pods — because function keys mean two hands. 🙂 Rust + [ratatui](https://ratatui.rs).

- **Containers** via [bollard](https://crates.io/crates/bollard) over the rootless Podman socket (Podman speaks the Docker API).
- **Pods** via the `podman` CLI (pods are Podman-specific — not in the Docker API).
- Full lifecycle: **start / stop / remove**, **logs**, and a **create** form that can write a Quadlet unit for autostart + dependencies.

## Why

Other container TUIs lean on function keys or can't create with resource limits/dependencies. podmangr is one-handed vim keys, and its create form maps straight onto Podman's real features (`--memory/--cpus`, `--requires`, and Quadlet for boot-autostart).

## Layout

```
 Containers | Pods                                        (Tab)
┌ podmangr — containers ──────────────────────────────────────┐
│   NAME                IMAGE                STATE    STATUS   │
│ › dev-postgres_1      postgres:17          running  Up 3h    │
│   penpot-frontend_1   penpotapp/frontend   running  Up 2h    │
│   caddy               caddy:2.10           running  Up 2h    │
└──────────────────────────────────────────────────────────────┘
 j/k move · s start · x stop · X rm · l logs · n new · r refresh · q quit
```

`Tab` switches between the **Containers** and **Pods** screens; state is colour-coded (running = green, stopped/exited = red, paused = yellow).

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
| `Tab` | switch Containers ⇄ Pods |
| `j` / `k` (or ↓/↑) | move selection |
| `g` / `G` | first / last |
| `s` | start (container, or pod on the Pods screen) |
| `x` | stop |
| `X` | remove — asks **y / N** to confirm |
| `l` | logs (container) |
| `n` | new container (form) |
| `r` | refresh |
| `q` | quit |

**Logs viewer:** `j`/`k` scroll, `PgUp`/`PgDn` page, `g`/`G` top/bottom, `q`/`Esc` back.
**Create form:** `Tab`/`↑↓` move fields, `space` toggles Autostart, `Enter` submits, `Esc` cancels.

## Screens

### Containers
Lifecycle via the bollard API: `s` start, `x` stop, `X` remove (with a y/N confirm modal). `l` opens a scrollable log view (last ~1000 lines).

### Pods
Driven by the `podman` CLI (`podman pod ps/start/stop/rm`), since pods aren't part of the Docker API. `s`/`x`/`X` act on the selected pod.

## Creating containers (`n`)

Fill the form; behaviour depends on what you set:

- **Plain `podman run -d`** — when *Autostart* is off and *Depends* is empty. Applies `--name`, `--network`, `-p` (ports), `--memory`, `--cpus`, `-v` (volume), and `--requires` if you name a container.
- **Quadlet unit** — when *Autostart* is on **or** *Depends* is set. Writes `~/.config/containers/systemd/<name>.container` (limits via `PodmanArgs=`, `Volume=`, `[Unit] After=/Requires=<dep>`, `[Install] WantedBy=default.target`), then `systemctl --user daemon-reload` and starts it. This gives **boot-autostart** and **start-order dependencies**. *Depends* expects a systemd unit (e.g. `dev-postgres.service`, or another Quadlet's `<name>.service`).
- **Disk** is handled with the Volume field (a named volume) rather than a hard size cap (not available on ext4).

## What it touches (safety)

- **Reads/controls** containers through the rootless Podman socket; **nothing is removed without a y/N confirm**.
- **Pods** are managed by shelling out to `podman`.
- **Create** either runs `podman run` or writes a Quadlet `.container` file under `~/.config/containers/systemd/` and starts the unit.
- No daemon, no root — it's all rootless Podman as your user.

## Roadmap

- [x] Containers: list + start/stop/remove (+ confirm) + logs
- [x] Pods screen (start/stop/rm)
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
