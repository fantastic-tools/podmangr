//! podmangr — a vim-keyed TUI for local Podman containers & pods.
//! Containers via bollard (Docker-compatible API); pods via the `podman` CLI
//! (pods are Podman-specific and not in the Docker API). MVP lifecycle actions.

use anyhow::Result;
use bollard::Docker;
use bollard::container::{
    ListContainersOptions, LogOutput, LogsOptions, RemoveContainerOptions, StartContainerOptions,
    StopContainerOptions,
};
use futures_util::stream::StreamExt;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState};
use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

#[derive(PartialEq, Clone, Copy)]
enum Screen {
    Containers,
    Pods,
    Projects,
}

enum Pending {
    RemoveContainer(String, String, Option<Owner>), // id, label, owning unit
    RemovePod(String, String),                      // id, label
    StopOwned(String, String, Owner),               // id, label, owning unit
}

struct Ctr {
    id: String,
    name: String,
    image: String,
    state: String,
    status: String,
    /// The systemd unit that runs this container's project, if any.
    owner: Option<Owner>,
    /// Its compose project, with or without a unit.
    project: String,
}

/// A systemd user unit that runs Podman: a compose project or a quadlet.
#[derive(Debug, Clone, PartialEq)]
struct Unit {
    name: String,
    workdir: String,
    active: String,
    compose: bool,
    /// A foreground `podman compose up`: stopping any one of its containers
    /// ends the unit, which then stops the rest of the project.
    fragile: bool,
    /// Its stop command is `podman compose down`, which deletes the containers.
    removes: bool,
}

/// The unit that owns a container (ticket 003: stopping one container of a
/// unit-run project can take the whole project with it).
#[derive(Debug, Clone, PartialEq)]
struct Owner {
    unit: String,
    project: String,
    fragile: bool,
    removes: bool,
}

struct Pod {
    id: String,
    name: String,
    status: String,
    nctr: String,
}

struct LogView {
    title: String,
    lines: Vec<String>,
    offset: usize,
    max_off: usize,
}

#[derive(Default)]
struct NewForm {
    image: String,
    name: String,
    network: String,
    ports: String,
    memory: String,
    cpus: String,
    volume: String,
    depends: String,
    autostart: bool,
    focus: usize, // 0..=7 text fields, 8 = autostart
    err: String,
}

impl NewForm {
    fn new() -> Self {
        NewForm {
            network: "dev-net".into(),
            ..Default::default()
        }
    }
    fn fields(&mut self) -> [&mut String; 8] {
        [
            &mut self.image,
            &mut self.name,
            &mut self.network,
            &mut self.ports,
            &mut self.memory,
            &mut self.cpus,
            &mut self.volume,
            &mut self.depends,
        ]
    }
}

struct App {
    docker: Docker,
    containers: Vec<Ctr>,
    pods: Vec<Pod>,
    units: Vec<Unit>,
    cstate: TableState,
    pstate: TableState,
    ustate: TableState,
    screen: Screen,
    confirm: Option<Pending>,
    logs: Option<LogView>,
    new_form: Option<NewForm>,
    msg: String,
    theme: usize,
    quit: bool,
}

impl App {
    fn new(docker: Docker) -> Self {
        let mut cstate = TableState::default();
        cstate.select(Some(0));
        let mut pstate = TableState::default();
        pstate.select(Some(0));
        let mut ustate = TableState::default();
        ustate.select(Some(0));
        Self {
            docker,
            containers: Vec::new(),
            pods: Vec::new(),
            units: Vec::new(),
            cstate,
            pstate,
            ustate,
            screen: Screen::Containers,
            confirm: None,
            logs: None,
            new_form: None,
            msg: "loading…".into(),
            theme: load_theme(),
            quit: false,
        }
    }

    async fn refresh(&mut self) {
        // systemd user units that run podman (systemctl), first: containers
        // are matched to them
        self.units = fetch_units();
        // containers (bollard)
        let opts = ListContainersOptions::<String> {
            all: true,
            ..Default::default()
        };
        match self.docker.list_containers(Some(opts)).await {
            Ok(list) => {
                let units = &self.units;
                self.containers = list
                    .into_iter()
                    .map(|c| {
                        let labels = c.labels.unwrap_or_default();
                        Ctr {
                            id: c.id.unwrap_or_default(),
                            name: c
                                .names
                                .and_then(|n| n.into_iter().next())
                                .unwrap_or_default()
                                .trim_start_matches('/')
                                .to_string(),
                            image: c.image.unwrap_or_default(),
                            state: c.state.unwrap_or_default(),
                            status: c.status.unwrap_or_default(),
                            owner: owner_of(&labels, units),
                            project: compose_project(&labels),
                        }
                    })
                    .collect();
            }
            Err(e) => self.msg = format!("list error: {e}"),
        }
        // pods (podman CLI)
        self.pods = fetch_pods();

        clamp(&mut self.cstate, self.containers.len());
        clamp(&mut self.pstate, self.pods.len());
        clamp(&mut self.ustate, self.units.len());
        // Counts only when nothing else has been said: an action's result
        // (set before it refreshes) stays on the status line.
        if matches!(self.msg.as_str(), "loading…" | "refreshing…") {
            self.msg = format!(
                "{} containers · {} pods · {} projects",
                self.containers.len(),
                self.pods.len(),
                self.units.len()
            );
        }
    }

    fn active_len(&self) -> usize {
        match self.screen {
            Screen::Containers => self.containers.len(),
            Screen::Pods => self.pods.len(),
            Screen::Projects => self.units.len(),
        }
    }
    fn active_state(&mut self) -> &mut TableState {
        match self.screen {
            Screen::Containers => &mut self.cstate,
            Screen::Pods => &mut self.pstate,
            Screen::Projects => &mut self.ustate,
        }
    }
    fn mv(&mut self, delta: i64) {
        let len = self.active_len();
        if len == 0 {
            return;
        }
        let st = self.active_state();
        let cur = st.selected().unwrap_or(0) as i64;
        let next = (cur + delta).clamp(0, len as i64 - 1) as usize;
        st.select(Some(next));
    }
    fn go(&mut self, idx: usize) {
        let len = self.active_len();
        if len > 0 {
            self.active_state().select(Some(idx.min(len - 1)));
        }
    }
    fn sel_ctr(&self) -> Option<&Ctr> {
        self.cstate.selected().and_then(|i| self.containers.get(i))
    }
    fn sel_pod(&self) -> Option<&Pod> {
        self.pstate.selected().and_then(|i| self.pods.get(i))
    }
    fn sel_unit(&self) -> Option<&Unit> {
        self.ustate.selected().and_then(|i| self.units.get(i))
    }

    /// (running, total) containers that `unit` owns.
    fn unit_counts(&self, unit: &str) -> (usize, usize) {
        let owned = self
            .containers
            .iter()
            .filter(|c| c.owner.as_ref().is_some_and(|o| o.unit == unit));
        owned.fold((0, 0), |(up, all), c| {
            (up + usize::from(c.state == "running"), all + 1)
        })
    }

    async fn stop_container(&mut self, id: &str, label: &str) {
        let r = self
            .docker
            .stop_container(id, None::<StopContainerOptions>)
            .await;
        self.msg = match r {
            Ok(_) => format!("stopped {label}"),
            Err(e) => format!("stop failed: {e}"),
        };
    }

    async fn fetch_logs(&self, id: &str) -> Vec<String> {
        let opts = LogsOptions::<String> {
            stdout: true,
            stderr: true,
            tail: "1000".into(),
            ..Default::default()
        };
        let mut stream = self.docker.logs(id, Some(opts));
        let mut buf = String::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(LogOutput::StdOut { message })
                | Ok(LogOutput::StdErr { message })
                | Ok(LogOutput::Console { message }) => {
                    buf.push_str(&String::from_utf8_lossy(&message))
                }
                Ok(_) => {}
                Err(e) => {
                    buf.push_str(&format!("\n[log error: {e}]"));
                    break;
                }
            }
        }
        buf.lines().map(|l| l.to_string()).collect()
    }

    async fn submit_new(&mut self) {
        let fm = match self.new_form.as_ref() {
            Some(f) => f,
            None => return,
        };
        let image = fm.image.trim().to_string();
        let name = fm.name.trim().to_string();
        let network = fm.network.trim().to_string();
        let ports = fm.ports.trim().to_string();
        let memory = fm.memory.trim().to_string();
        let cpus = fm.cpus.trim().to_string();
        let volume = fm.volume.trim().to_string();
        let depends = fm.depends.trim().to_string();
        let autostart = fm.autostart;

        if image.is_empty() {
            if let Some(f) = self.new_form.as_mut() {
                f.err = "image is required".into();
            }
            return;
        }
        let use_quadlet = autostart || !depends.is_empty();
        if use_quadlet && name.is_empty() {
            if let Some(f) = self.new_form.as_mut() {
                f.err = "name required for autostart/dependency".into();
            }
            return;
        }

        let result = if use_quadlet {
            write_and_start_quadlet(
                &name, &image, &network, &ports, &memory, &cpus, &volume, &depends,
            )
        } else {
            run_container(
                &image, &name, &network, &ports, &memory, &cpus, &volume, &depends,
            )
        };
        match result {
            Ok(m) => {
                self.msg = m;
                self.new_form = None;
                self.refresh().await;
            }
            Err(e) => {
                if let Some(f) = self.new_form.as_mut() {
                    f.err = e;
                }
            }
        }
    }

    async fn perform(&mut self, act: Pending) {
        match act {
            Pending::RemoveContainer(id, label, _) => {
                let r = self
                    .docker
                    .remove_container(
                        &id,
                        Some(RemoveContainerOptions {
                            force: true,
                            ..Default::default()
                        }),
                    )
                    .await;
                self.msg = match r {
                    Ok(_) => format!("removed container {label}"),
                    Err(e) => format!("remove failed: {e}"),
                };
            }
            Pending::RemovePod(id, label) => {
                self.msg = match podman(&["pod", "rm", "-f", &id]) {
                    Ok(_) => format!("removed pod {label}"),
                    Err(e) => format!("pod rm failed: {e}"),
                };
            }
            Pending::StopOwned(id, label, _) => self.stop_container(&id, &label).await,
        }
        self.refresh().await;
    }
}

fn clamp(st: &mut TableState, len: usize) {
    if len == 0 {
        st.select(None);
    } else {
        let i = st.selected().unwrap_or(0).min(len - 1);
        st.select(Some(i));
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

/// Run a `podman` subcommand, returning stderr on failure.
fn podman(args: &[&str]) -> Result<String, String> {
    match Command::new("podman").args(args).output() {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}

fn systemctl_user(args: &[&str]) -> Result<(), String> {
    systemctl_user_out(args).map(|_| ())
}

/// Run `systemctl --user`, returning stdout (stderr on failure).
fn systemctl_user_out(args: &[&str]) -> Result<String, String> {
    let mut a = vec!["--user"];
    a.extend_from_slice(args);
    match Command::new("systemctl").args(&a).output() {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Start or stop a whole project through its unit.
fn unit_action(verb: &str, unit: &str) -> String {
    match systemctl_user(&[verb, unit]) {
        Ok(()) => format!("{verb}: {unit} (whole project)"),
        Err(e) => format!("{verb} {unit} failed: {e}"),
    }
}

/// The systemd user units that run podman. Every unit file plus every loaded
/// unit (transient and generated ones have no file of their own), read with
/// one `systemctl show`. A unit that is stopped, or whose containers are all
/// gone, is still listed, so its project can be started again from here.
fn fetch_units() -> Vec<Unit> {
    let mut names: Vec<String> = Vec::new();
    let lists: [&[&str]; 2] = [
        &[
            "list-unit-files",
            "--type=service",
            "--no-legend",
            "--plain",
        ],
        &[
            "list-units",
            "--type=service",
            "--all",
            "--no-legend",
            "--plain",
        ],
    ];
    for args in lists {
        let Ok(out) = systemctl_user_out(args) else {
            continue;
        };
        for line in out.lines() {
            // Templates (`name@.service`) can't be shown; instances can.
            let name = line
                .split_whitespace()
                .find(|w| w.ends_with(".service") && !w.ends_with("@.service"));
            if let Some(n) = name
                && !names.iter().any(|x| x == n)
            {
                names.push(n.to_string());
            }
        }
    }
    if names.is_empty() {
        return Vec::new();
    }
    let mut args = vec![
        "show",
        "--property=Id,WorkingDirectory,ExecStart,ExecStop,ActiveState,LoadState",
    ];
    args.extend(names.iter().map(String::as_str));
    let mut units = systemctl_user_out(&args)
        .map(|o| parse_units(&o))
        .unwrap_or_default();
    units.sort_by(|a, b| a.name.cmp(&b.name));
    units
}

/// `systemctl show` output (blank-line separated `Key=Value` blocks) to the
/// units among them that run a project: a compose project, or a quadlet
/// (`podman run`, `kube play`, `pod start`). Podman's own services
/// (`podman system service`, `auto-update`, `start --all`) and its
/// healthcheck timers' units run podman too, but no project.
fn parse_units(show: &str) -> Vec<Unit> {
    show.split("\n\n")
        .filter_map(|block| {
            let get = |key: &str| {
                block
                    .lines()
                    .find_map(|l| l.strip_prefix(key).and_then(|rest| rest.strip_prefix('=')))
                    .unwrap_or("")
            };
            let (name, start, stop) = (get("Id"), get("ExecStart"), get("ExecStop"));
            let compose = start.contains("podman compose") || start.contains("podman-compose");
            let quadlet = ["podman run ", "podman kube play ", "podman pod start "]
                .iter()
                .any(|p| start.contains(p));
            if name.is_empty() || get("LoadState") == "not-found" || !(compose || quadlet) {
                return None;
            }
            let detached =
                start.contains(" -d ") || start.contains(" -d;") || start.contains("--detach");
            Some(Unit {
                name: name.to_string(),
                workdir: get("WorkingDirectory").to_string(),
                active: get("ActiveState").to_string(),
                compose,
                fragile: compose && start.contains("compose up") && !detached,
                removes: compose && stop.contains("compose down"),
            })
        })
        .collect()
}

fn compose_project(labels: &HashMap<String, String>) -> String {
    labels
        .get("io.podman.compose.project")
        .or_else(|| labels.get("com.docker.compose.project"))
        .cloned()
        .unwrap_or_default()
}

/// The unit that runs a container. Its `PODMAN_SYSTEMD_UNIT` label when that
/// names a real unit (quadlets set it right); else the compose unit whose
/// working directory is the container's compose project directory, since
/// podman-compose always labels `podman-compose@<project>.service`, which
/// needn't exist.
fn owner_of(labels: &HashMap<String, String>, units: &[Unit]) -> Option<Owner> {
    let named = labels
        .get("PODMAN_SYSTEMD_UNIT")
        .and_then(|n| units.iter().find(|u| &u.name == n));
    let unit = named.or_else(|| {
        let dir = labels.get("com.docker.compose.project.working_dir")?;
        units.iter().find(|u| u.compose && &u.workdir == dir)
    })?;
    let project = match compose_project(labels) {
        p if p.is_empty() => unit.name.trim_end_matches(".service").to_string(),
        p => p,
    };
    Some(Owner {
        unit: unit.name.clone(),
        project,
        fragile: unit.fragile,
        removes: unit.removes,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_container(
    image: &str,
    name: &str,
    network: &str,
    ports: &str,
    memory: &str,
    cpus: &str,
    volume: &str,
    depends: &str,
) -> Result<String, String> {
    let mut a: Vec<String> = vec!["run".into(), "-d".into()];
    let mut push = |flag: &str, val: &str| {
        if !val.is_empty() {
            a.push(flag.into());
            a.push(val.into());
        }
    };
    push("--name", name);
    push("--network", network);
    push("-p", ports);
    push("--memory", memory);
    push("--cpus", cpus);
    push("-v", volume);
    push("--requires", depends);
    a.push(image.into());
    let refs: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
    let out = podman(&refs)?;
    let label = if name.is_empty() {
        out.trim().chars().take(12).collect::<String>()
    } else {
        name.to_string()
    };
    Ok(format!("created {label}"))
}

#[allow(clippy::too_many_arguments)]
fn write_and_start_quadlet(
    name: &str,
    image: &str,
    network: &str,
    ports: &str,
    memory: &str,
    cpus: &str,
    volume: &str,
    depends: &str,
) -> Result<String, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    let dir = format!("{home}/.config/containers/systemd");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = format!("{dir}/{name}.container");

    let mut s = String::new();
    s.push_str("[Unit]\n");
    s.push_str(&format!("Description={name} (podmangr)\n"));
    if !depends.is_empty() {
        s.push_str(&format!("After={depends}\nRequires={depends}\n"));
    }
    s.push_str("\n[Container]\n");
    s.push_str(&format!("Image={image}\n"));
    s.push_str(&format!("ContainerName={name}\n"));
    if !network.is_empty() {
        s.push_str(&format!("Network={network}\n"));
    }
    if !ports.is_empty() {
        s.push_str(&format!("PublishPort={ports}\n"));
    }
    if !volume.is_empty() {
        s.push_str(&format!("Volume={volume}\n"));
    }
    let mut pa: Vec<String> = Vec::new();
    if !memory.is_empty() {
        pa.push(format!("--memory={memory}"));
    }
    if !cpus.is_empty() {
        pa.push(format!("--cpus={cpus}"));
    }
    if !pa.is_empty() {
        s.push_str(&format!("PodmanArgs={}\n", pa.join(" ")));
    }
    s.push_str("\n[Service]\nRestart=always\n");
    s.push_str("\n[Install]\nWantedBy=default.target\n");

    std::fs::write(&path, s).map_err(|e| e.to_string())?;
    systemctl_user(&["daemon-reload"])?;
    systemctl_user(&["start", &format!("{name}.service")])?;
    Ok(format!(
        "quadlet {name} created + started (autostart on boot)"
    ))
}

fn fetch_pods() -> Vec<Pod> {
    let fmt = "{{.Name}}\t{{.Id}}\t{{.Status}}\t{{.NumberOfContainers}}";
    match podman(&["pod", "ps", "--format", fmt]) {
        Ok(out) => out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                let g = |i: usize| f.get(i).copied().unwrap_or("").to_string();
                Pod {
                    name: g(0),
                    id: g(1),
                    status: g(2),
                    nctr: g(3),
                }
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn sock_path() -> String {
    let rt = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".into());
    format!("{rt}/podman/podman.sock")
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let docker = Docker::connect_with_unix(&sock_path(), 120, bollard::API_DEFAULT_VERSION)?;
    let mut app = App::new(docker);
    app.refresh().await;

    let mut terminal = ratatui::init();
    let res = run(&mut terminal, &mut app).await;
    ratatui::restore();
    res
}

async fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.quit {
        terminal.draw(|f| ui(f, app))?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(k) = event::read()? else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }

        // Logs viewer intercepts keys (scroll) until closed.
        if let Some(lv) = app.logs.as_mut() {
            lv.offset = lv.offset.min(lv.max_off);
            let page = 20usize;
            match k.code {
                KeyCode::Char('j') | KeyCode::Down => lv.offset = (lv.offset + 1).min(lv.max_off),
                KeyCode::Char('k') | KeyCode::Up => lv.offset = lv.offset.saturating_sub(1),
                KeyCode::PageDown => lv.offset = (lv.offset + page).min(lv.max_off),
                KeyCode::PageUp => lv.offset = lv.offset.saturating_sub(page),
                KeyCode::Char('g') => lv.offset = 0,
                KeyCode::Char('G') => lv.offset = lv.max_off,
                KeyCode::Char('q') | KeyCode::Esc => app.logs = None,
                _ => {}
            }
            continue;
        }

        // Confirm modal intercepts all keys until answered.
        if let Some(pending) = &app.confirm {
            // The unit behind a container, when the modal offers to act on it.
            let unit = match pending {
                Pending::StopOwned(_, _, o) | Pending::RemoveContainer(_, _, Some(o)) => {
                    Some(o.unit.clone())
                }
                _ => None,
            };
            let this_one = match pending {
                Pending::StopOwned(..) => KeyCode::Char('c'),
                _ => KeyCode::Char('y'),
            };
            match k.code {
                c if c == this_one
                    || (this_one == KeyCode::Char('y') && c == KeyCode::Char('Y')) =>
                {
                    if let Some(act) = app.confirm.take() {
                        app.perform(act).await;
                    }
                }
                KeyCode::Char('u') if unit.is_some() => {
                    app.confirm = None;
                    if let Some(unit) = unit {
                        app.msg = unit_action("stop", &unit);
                        app.refresh().await;
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    app.confirm = None;
                    app.msg = "cancelled".into();
                }
                _ => {}
            }
            continue;
        }

        // New-container form intercepts keys (text entry) until submitted/cancelled.
        if app.new_form.is_some() {
            match k.code {
                KeyCode::Esc => {
                    app.new_form = None;
                    app.msg = "cancelled".into();
                }
                KeyCode::Enter => app.submit_new().await,
                _ => {
                    if let Some(fm) = app.new_form.as_mut() {
                        match k.code {
                            KeyCode::Tab | KeyCode::Down => fm.focus = (fm.focus + 1) % 9,
                            KeyCode::BackTab | KeyCode::Up => fm.focus = (fm.focus + 8) % 9,
                            KeyCode::Backspace => {
                                if fm.focus < 8 {
                                    let i = fm.focus;
                                    fm.fields()[i].pop();
                                }
                            }
                            KeyCode::Char(c) => {
                                if fm.focus == 8 {
                                    if c == ' ' {
                                        fm.autostart = !fm.autostart;
                                    }
                                } else {
                                    let i = fm.focus;
                                    fm.fields()[i].push(c);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            continue;
        }

        match k.code {
            KeyCode::Char('q') => app.quit = true,
            KeyCode::Char('t') => {
                app.theme = (app.theme + 1) % themes().len();
                save_theme(themes()[app.theme].name);
                app.msg = format!("theme: {}", themes()[app.theme].name);
            }
            KeyCode::Tab => {
                app.screen = match app.screen {
                    Screen::Containers => Screen::Pods,
                    Screen::Pods => Screen::Projects,
                    Screen::Projects => Screen::Containers,
                }
            }
            KeyCode::Char('j') | KeyCode::Down => app.mv(1),
            KeyCode::Char('k') | KeyCode::Up => app.mv(-1),
            KeyCode::Char('g') => app.go(0),
            KeyCode::Char('G') => app.go(usize::MAX),
            KeyCode::Char('r') => {
                app.msg = "refreshing…".into();
                app.refresh().await;
            }
            KeyCode::Char('l') => {
                if app.screen == Screen::Containers {
                    if let Some(c) = app.sel_ctr() {
                        let (id, title) = (c.id.clone(), c.name.clone());
                        app.msg = format!("logs: {title}");
                        let lines = app.fetch_logs(&id).await;
                        app.logs = Some(LogView {
                            title,
                            lines,
                            offset: usize::MAX,
                            max_off: 0,
                        });
                    }
                } else {
                    app.msg = "logs: select a container (Tab)".into();
                }
            }
            KeyCode::Char('n') => app.new_form = Some(NewForm::new()),
            KeyCode::Char('s') => match app.screen {
                Screen::Containers => {
                    if let Some(c) = app.sel_ctr() {
                        let (id, label) = (c.id.clone(), c.name.clone());
                        let r = app
                            .docker
                            .start_container(&id, None::<StartContainerOptions<String>>)
                            .await;
                        app.msg = match r {
                            Ok(_) => format!("started {label}"),
                            Err(e) => format!("start failed: {e}"),
                        };
                        app.refresh().await;
                    }
                }
                Screen::Pods => {
                    if let Some(p) = app.sel_pod() {
                        let (id, label) = (p.id.clone(), p.name.clone());
                        app.msg = match podman(&["pod", "start", &id]) {
                            Ok(_) => format!("started pod {label}"),
                            Err(e) => format!("pod start failed: {e}"),
                        };
                        app.refresh().await;
                    }
                }
                Screen::Projects => {
                    if let Some(u) = app.sel_unit() {
                        let unit = u.name.clone();
                        app.msg = unit_action("start", &unit);
                        app.refresh().await;
                    }
                }
            },
            KeyCode::Char('x') => match app.screen {
                Screen::Containers => {
                    if let Some(c) = app.sel_ctr() {
                        let (id, label, owner) = (c.id.clone(), c.name.clone(), c.owner.clone());
                        // A unit runs this one: ask whether to stop just it,
                        // or the project through its unit.
                        if let Some(owner) = owner {
                            app.confirm = Some(Pending::StopOwned(id, label, owner));
                        } else {
                            app.stop_container(&id, &label).await;
                            app.refresh().await;
                        }
                    }
                }
                Screen::Pods => {
                    if let Some(p) = app.sel_pod() {
                        let (id, label) = (p.id.clone(), p.name.clone());
                        app.msg = match podman(&["pod", "stop", &id]) {
                            Ok(_) => format!("stopped pod {label}"),
                            Err(e) => format!("pod stop failed: {e}"),
                        };
                        app.refresh().await;
                    }
                }
                Screen::Projects => {
                    if let Some(u) = app.sel_unit() {
                        let unit = u.name.clone();
                        app.msg = unit_action("stop", &unit);
                        app.refresh().await;
                    }
                }
            },
            KeyCode::Char('X') => match app.screen {
                Screen::Containers => {
                    if let Some(c) = app.sel_ctr() {
                        app.confirm = Some(Pending::RemoveContainer(
                            c.id.clone(),
                            c.name.clone(),
                            c.owner.clone(),
                        ));
                    }
                }
                Screen::Pods => {
                    if let Some(p) = app.sel_pod() {
                        app.confirm = Some(Pending::RemovePod(p.id.clone(), p.name.clone()));
                    }
                }
                Screen::Projects => {
                    app.msg = "a project's containers are removed with `podman compose down` in its folder".into();
                }
            },
            _ => {}
        }
    }
    Ok(())
}

struct Theme {
    name: &'static str,
    fg: Color,
    bg: Color,
    accent: Color,
    on: Color,
    warn: Color,
    danger: Color,
    off: Color,
}

fn themes() -> Vec<Theme> {
    vec![
        Theme {
            name: "dark",
            fg: Color::Gray,
            bg: Color::Reset,
            accent: Color::Cyan,
            on: Color::Green,
            warn: Color::Yellow,
            danger: Color::Red,
            off: Color::DarkGray,
        },
        Theme {
            name: "light",
            fg: Color::Black,
            bg: Color::White,
            accent: Color::Blue,
            on: Color::Rgb(0, 135, 0),
            warn: Color::Rgb(181, 137, 0),
            danger: Color::Rgb(197, 15, 31),
            off: Color::Gray,
        },
        Theme {
            name: "solarized",
            fg: Color::Rgb(131, 148, 150),
            bg: Color::Rgb(0, 43, 54),
            accent: Color::Rgb(38, 139, 210),
            on: Color::Rgb(133, 153, 0),
            warn: Color::Rgb(181, 137, 0),
            danger: Color::Rgb(220, 50, 47),
            off: Color::Rgb(88, 110, 117),
        },
        Theme {
            name: "gruvbox",
            fg: Color::Rgb(235, 219, 178),
            bg: Color::Rgb(40, 40, 40),
            accent: Color::Rgb(250, 189, 47),
            on: Color::Rgb(184, 187, 38),
            warn: Color::Rgb(254, 128, 25),
            danger: Color::Rgb(251, 73, 52),
            off: Color::Rgb(146, 131, 116),
        },
    ]
}

fn config_theme_path() -> String {
    format!(
        "{}/.config/podmangr/theme",
        std::env::var("HOME").unwrap_or_default()
    )
}
fn load_theme() -> usize {
    std::fs::read_to_string(config_theme_path())
        .ok()
        .and_then(|s| {
            let n = s.trim().to_string();
            themes().iter().position(|t| t.name == n)
        })
        .unwrap_or(0)
}
fn save_theme(name: &str) {
    let p = config_theme_path();
    if let Some(dir) = std::path::Path::new(&p).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&p, name);
}

fn ui(f: &mut Frame, app: &mut App) {
    let th = &themes()[app.theme];
    let base = Style::new().fg(th.fg).bg(th.bg);
    f.render_widget(Block::default().style(base), f.area());

    // Full-screen logs viewer
    if let Some(lv) = app.logs.as_mut() {
        let areas = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(f.area());
        let inner_h = areas[0].height.saturating_sub(2) as usize;
        lv.max_off = lv.lines.len().saturating_sub(inner_h);
        let off = lv.offset.min(lv.max_off);
        let visible: Vec<Line> = lv.lines[off..]
            .iter()
            .take(inner_h.max(1))
            .map(|l| Line::raw(l.clone()))
            .collect();
        let title = format!(
            " logs: {}  ({}/{}) ",
            lv.title,
            off + visible.len(),
            lv.lines.len()
        );
        f.render_widget(
            Paragraph::new(visible).block(Block::default().borders(Borders::ALL).title(title)),
            areas[0],
        );
        let help = Line::from(vec![
            Span::styled(" j/k", Style::new().fg(th.accent)),
            Span::raw(" scroll  "),
            Span::styled("PgUp/PgDn", Style::new().fg(th.accent)),
            Span::raw(" page  "),
            Span::styled("g/G", Style::new().fg(th.accent)),
            Span::raw(" top/bottom  "),
            Span::styled("q/Esc", Style::new().fg(th.accent)),
            Span::raw(" back"),
        ]);
        f.render_widget(Paragraph::new(help), areas[1]);
        return;
    }

    let chunks = Layout::vertical([
        Constraint::Length(1), // tabs
        Constraint::Min(1),    // table
        Constraint::Length(1), // help
    ])
    .split(f.area());

    // tab bar
    let tab = |s: Screen| {
        if app.screen == s {
            Style::new().bg(th.accent).fg(th.bg).bold()
        } else {
            Style::new().fg(th.off)
        }
    };
    let tabs = Line::from(vec![
        Span::styled(" Containers ", tab(Screen::Containers)),
        Span::raw("  "),
        Span::styled(" Pods ", tab(Screen::Pods)),
        Span::raw("  "),
        Span::styled(" Projects ", tab(Screen::Projects)),
        Span::raw("   (Tab to switch)"),
    ]);
    f.render_widget(Paragraph::new(tabs), chunks[0]);

    match app.screen {
        Screen::Containers => {
            let header = Row::new(["NAME", "IMAGE", "STATE", "STATUS", "OWNER"])
                .style(Style::new().fg(th.accent).bold());
            let rows = app.containers.iter().map(|c| {
                let owner = match (&c.owner, c.project.as_str()) {
                    (Some(o), _) => Cell::from(o.unit.clone()).style(Style::new().fg(
                        if o.fragile || o.removes {
                            th.warn
                        } else {
                            th.fg
                        },
                    )),
                    (None, "") => Cell::from(""),
                    (None, p) => Cell::from(format!("compose: {p}")).style(Style::new().fg(th.off)),
                };
                Row::new(vec![
                    Cell::from(c.name.clone()),
                    Cell::from(c.image.clone()),
                    Cell::from(c.state.clone()).style(state_style(th, &c.state)),
                    Cell::from(c.status.clone()),
                    owner,
                ])
            });
            let widths = [
                Constraint::Percentage(26),
                Constraint::Percentage(28),
                Constraint::Length(10),
                Constraint::Percentage(20),
                Constraint::Percentage(20),
            ];
            let t = Table::new(rows, widths)
                .header(header)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" podmangr — containers "),
                )
                .row_highlight_style(Style::new().bg(th.accent).fg(th.bg))
                .highlight_symbol("› ");
            f.render_stateful_widget(t, chunks[1], &mut app.cstate);
        }
        Screen::Pods => {
            let header = Row::new(["NAME", "ID", "STATUS", "#CTRS"])
                .style(Style::new().fg(th.accent).bold());
            let rows = app.pods.iter().map(|p| {
                Row::new(vec![
                    Cell::from(p.name.clone()),
                    Cell::from(short(&p.id).to_string()),
                    Cell::from(p.status.clone()).style(state_style(th, &p.status.to_lowercase())),
                    Cell::from(p.nctr.clone()),
                ])
            });
            let widths = [
                Constraint::Percentage(40),
                Constraint::Length(14),
                Constraint::Percentage(30),
                Constraint::Length(7),
            ];
            let t = Table::new(rows, widths)
                .header(header)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" podmangr — pods "),
                )
                .row_highlight_style(Style::new().bg(th.accent).fg(th.bg))
                .highlight_symbol("› ");
            f.render_stateful_widget(t, chunks[1], &mut app.pstate);
        }
        Screen::Projects => {
            let header = Row::new(["UNIT", "STATE", "CONTAINERS", "FOLDER", "NOTE"])
                .style(Style::new().fg(th.accent).bold());
            let rows = app.units.iter().map(|u| {
                let (up, all) = app.unit_counts(&u.name);
                let note = if u.fragile {
                    "stopping one container stops all"
                } else if u.removes {
                    "stop removes containers"
                } else {
                    ""
                };
                Row::new(vec![
                    Cell::from(u.name.clone()),
                    Cell::from(u.active.clone()).style(state_style(
                        th,
                        &u.active
                            .replace("inactive", "stopped")
                            .replace("active", "running"),
                    )),
                    Cell::from(format!("{up}/{all} up")),
                    Cell::from(u.workdir.clone()),
                    Cell::from(note).style(Style::new().fg(th.warn)),
                ])
            });
            let widths = [
                Constraint::Percentage(24),
                Constraint::Length(10),
                Constraint::Length(11),
                Constraint::Percentage(34),
                Constraint::Percentage(26),
            ];
            let t = Table::new(rows, widths)
                .header(header)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" podmangr — projects (systemd units that run podman) "),
                )
                .row_highlight_style(Style::new().bg(th.accent).fg(th.bg))
                .highlight_symbol("› ");
            f.render_stateful_widget(t, chunks[1], &mut app.ustate);
        }
    }

    let help = Line::from(vec![
        Span::styled(" Tab", Style::new().fg(th.accent)),
        Span::raw(" screen  "),
        Span::styled("j/k", Style::new().fg(th.accent)),
        Span::raw(" move  "),
        Span::styled("s", Style::new().fg(th.accent)),
        Span::raw(" start  "),
        Span::styled("x", Style::new().fg(th.accent)),
        Span::raw(" stop  "),
        Span::styled("X", Style::new().fg(th.accent)),
        Span::raw(" rm  "),
        Span::styled("l", Style::new().fg(th.accent)),
        Span::raw(" logs  "),
        Span::styled("n", Style::new().fg(th.accent)),
        Span::raw(" new  "),
        Span::styled("r", Style::new().fg(th.accent)),
        Span::raw(" refresh  "),
        Span::styled("t", Style::new().fg(th.accent)),
        Span::raw(" theme  "),
        Span::styled("q", Style::new().fg(th.accent)),
        Span::raw(" quit   "),
        Span::styled(format!("[{}]", app.msg), Style::new().fg(th.off)),
    ]);
    f.render_widget(Paragraph::new(help), chunks[2]);

    // confirm modal overlay
    if let Some(p) = &app.confirm {
        let (title, lines) = confirm_lines(th, p);
        let area = centered(f.area(), 84, lines.len() as u16 + 2);
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(Style::new().fg(th.danger)),
            ),
            area,
        );
    }

    // new-container form overlay
    if let Some(fm) = &app.new_form {
        let area = centered(f.area(), 66, 20);
        f.render_widget(Clear, area);
        let labels = [
            "Image *",
            "Name",
            "Network",
            "Ports (host:ctr)",
            "Memory (e.g. 512m)",
            "CPUs (e.g. 1.5)",
            "Volume (vol:/path)",
            "Depends (unit/ctr)",
        ];
        let vals = [
            &fm.image,
            &fm.name,
            &fm.network,
            &fm.ports,
            &fm.memory,
            &fm.cpus,
            &fm.volume,
            &fm.depends,
        ];
        let mut lines: Vec<Line> = vec![Line::raw("")];
        for i in 0..8 {
            let marker = if fm.focus == i { "› " } else { "  " };
            let vstyle = if fm.focus == i {
                Style::new().bg(th.accent).fg(th.bg)
            } else {
                Style::new()
            };
            lines.push(Line::from(vec![
                Span::raw(marker),
                Span::styled(format!("{:<20}", labels[i]), Style::new().fg(th.accent)),
                Span::styled(vals[i].clone(), vstyle),
            ]));
        }
        let m8 = if fm.focus == 8 { "› " } else { "  " };
        lines.push(Line::from(vec![
            Span::raw(m8),
            Span::styled(
                format!("{:<20}", "Autostart (space)"),
                Style::new().fg(th.accent),
            ),
            Span::raw(if fm.autostart {
                "[x]  → Quadlet systemd unit"
            } else {
                "[ ]"
            }),
        ]));
        lines.push(Line::raw(""));
        if !fm.err.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("  {}", fm.err),
                Style::new().fg(th.danger),
            )));
        }
        lines.push(Line::from(Span::styled(
            "  Enter submit · Tab/↑↓ move · space toggles autostart · Esc cancel",
            Style::new().fg(th.off),
        )));
        f.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" new container ")
                    .border_style(Style::new().fg(th.accent)),
            ),
            area,
        );
    }
}

/// The confirm modal's title and lines. A container a systemd unit owns gets a
/// third choice, its whole project through the unit, and a warning when the
/// unit would take the project down with it (ticket 003).
fn confirm_lines(th: &Theme, p: &Pending) -> (&'static str, Vec<Line<'static>>) {
    let key = |k: &'static str, colour: Color| Span::styled(k, Style::new().fg(colour).bold());
    let name = |l: &str| Span::styled(l.to_string(), Style::new().bold().fg(th.warn));
    let owned = |o: &Owner| {
        let mut v = vec![Line::from(vec![
            Span::raw("  Run by systemd unit "),
            Span::styled(o.unit.clone(), Style::new().bold()),
            Span::raw(format!(" (project {}).", o.project)),
        ])];
        if o.fragile {
            v.push(Line::from(Span::styled(
                "  Stopping any one of its containers ends the unit, which stops the rest",
                Style::new().fg(th.danger),
            )));
            v.push(Line::from(Span::styled(
                if o.removes {
                    "  and then REMOVES them all (compose down)."
                } else {
                    "  of the project."
                },
                Style::new().fg(th.danger),
            )));
        } else if o.removes {
            v.push(Line::from(Span::styled(
                "  Stopping the unit removes its containers (compose down).",
                Style::new().fg(th.danger),
            )));
        }
        v
    };
    match p {
        Pending::StopOwned(_, label, o) => {
            let mut lines = vec![
                Line::raw(""),
                Line::from(vec![Span::raw("  Stop "), name(label), Span::raw(" ?")]),
            ];
            lines.extend(owned(o));
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::raw("  "),
                key("c", th.on),
                Span::raw(" = just this container    "),
                key("u", th.warn),
                Span::raw(format!(" = whole project ({})", o.unit)),
            ]));
            lines.push(Line::from(vec![
                Span::raw("  "),
                key("n", th.danger),
                Span::raw("/Esc = cancel"),
            ]));
            (" confirm stop ", lines)
        }
        Pending::RemoveContainer(_, label, owner) => {
            let mut lines = vec![
                Line::raw(""),
                Line::from(vec![
                    Span::raw("  Remove container "),
                    name(label),
                    Span::raw(" ?"),
                ]),
            ];
            if let Some(o) = owner {
                lines.extend(owned(o));
            }
            lines.push(Line::raw(""));
            let mut choices = vec![Span::raw("  "), key("y", th.on), Span::raw(" = yes    ")];
            if let Some(o) = owner {
                choices.extend([
                    key("u", th.warn),
                    Span::raw(format!(" = stop project ({}) instead    ", o.unit)),
                ]);
            }
            choices.extend([key("n", th.danger), Span::raw("/Esc = no")]);
            lines.push(Line::from(choices));
            (" confirm remove ", lines)
        }
        Pending::RemovePod(_, label) => (
            " confirm remove ",
            vec![
                Line::raw(""),
                Line::from(vec![
                    Span::raw("  Remove pod "),
                    name(label),
                    Span::raw(" ?"),
                ]),
                Line::raw(""),
                Line::from(vec![
                    Span::raw("  "),
                    key("y", th.on),
                    Span::raw(" = yes    "),
                    key("n", th.danger),
                    Span::raw("/Esc = no"),
                ]),
            ],
        ),
    }
}

fn state_style(th: &Theme, state: &str) -> Style {
    match state {
        s if s.contains("running") || s.contains("up") => Style::new().fg(th.on),
        s if s.contains("paused") || s.contains("degraded") => Style::new().fg(th.warn),
        s if s.contains("exited")
            || s.contains("stopped")
            || s.contains("created")
            || s.contains("dead")
            || s.contains("down") =>
        {
            Style::new().fg(th.danger)
        }
        _ => Style::new().fg(th.fg),
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `systemctl --user show` as this machine prints it: penpot.service as
    /// rewritten 2026-10-07, the same unit before that (foreground `compose
    /// up`, `compose down` to stop), a quadlet, and a unit that isn't podman's.
    const SHOW: &str = "\
Id=penpot.service
LoadState=loaded
ActiveState=active
ExecStart={ path=/usr/bin/podman ; argv[]=/usr/bin/podman compose up -d ; ignore_errors=no ; pid=623641 ; code=exited ; status=0 }
ExecStop={ path=/usr/bin/podman ; argv[]=/usr/bin/podman compose stop ; ignore_errors=no ; pid=0 ; code=(null) ; status=0/0 }
WorkingDirectory=/home/u/develop/penpot

Id=old-penpot.service
LoadState=loaded
ActiveState=inactive
ExecStart={ path=/usr/bin/podman ; argv[]=/usr/bin/podman compose up ; ignore_errors=no ; pid=0 ; code=(null) ; status=0/0 }
ExecStop={ path=/usr/bin/podman ; argv[]=/usr/bin/podman compose down ; ignore_errors=no ; pid=0 ; code=(null) ; status=0/0 }
WorkingDirectory=/home/u/develop/old-penpot

Id=web.service
LoadState=loaded
ActiveState=active
ExecStart={ path=/usr/bin/podman ; argv[]=/usr/bin/podman run --name web --replace --rm -d docker.io/library/caddy ; ignore_errors=no ; pid=1 ; code=exited ; status=0 }
ExecStop={ path=/usr/bin/podman ; argv[]=/usr/bin/podman rm -v -f -i web ; ignore_errors=no ; pid=0 ; code=(null) ; status=0/0 }
WorkingDirectory=

Id=pipewire.service
LoadState=loaded
ActiveState=active
ExecStart={ path=/usr/bin/pipewire ; argv[]=/usr/bin/pipewire ; ignore_errors=no ; pid=2 ; code=(null) ; status=0/0 }
ExecStop=
WorkingDirectory=

Id=podman-restart.service
LoadState=loaded
ActiveState=inactive
ExecStart={ path=/usr/bin/podman ; argv[]=/usr/bin/podman $LOGGING start --all --filter restart-policy=always ; ignore_errors=no ; pid=0 ; code=(null) ; status=0/0 }
ExecStop={ path=/bin/sh ; argv[]=/bin/sh -c /usr/bin/podman $LOGGING stop $(/usr/bin/podman container ls --filter restart-policy=always -q) ; ignore_errors=no ; pid=0 ; code=(null) ; status=0/0 }
WorkingDirectory=!/home/u

Id=b3faa854977aaf4cbd7d61b1b0ef0115bfa7ae987eb473e98f396908c6a24c86-6f43c144bbcd955a.service
LoadState=loaded
ActiveState=inactive
ExecStart={ path=/usr/bin/podman ; argv[]=/usr/bin/podman healthcheck run b3faa854977aaf4cbd7d61b1b0ef0115bfa7ae987eb473e98f396908c6a24c86 ; ignore_errors=no ; pid=0 ; code=(null) ; status=0/0 }
ExecStop=
WorkingDirectory=!/home/u

Id=gone.service
LoadState=not-found
ActiveState=inactive
ExecStart=
ExecStop=
WorkingDirectory=
";

    fn labels(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// What a Penpot container carries: podman-compose always names
    /// `podman-compose@<project>.service`, which isn't the real unit.
    fn penpot_labels(dir: &str) -> HashMap<String, String> {
        labels(&[
            ("PODMAN_SYSTEMD_UNIT", "podman-compose@penpot.service"),
            ("io.podman.compose.project", "penpot"),
            ("com.docker.compose.project.working_dir", dir),
        ])
    }

    fn text(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn only_podman_units_are_projects() {
        let units = parse_units(SHOW);
        let names: Vec<&str> = units.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(
            names,
            ["penpot.service", "old-penpot.service", "web.service"]
        );
    }

    #[test]
    fn a_foreground_compose_up_is_fragile_and_compose_down_removes() {
        let units = parse_units(SHOW);
        let (new, old, quadlet) = (&units[0], &units[1], &units[2]);
        assert!(new.compose && !new.fragile && !new.removes, "{new:?}");
        assert!(old.compose && old.fragile && old.removes, "{old:?}");
        assert!(
            !quadlet.compose && !quadlet.fragile && !quadlet.removes,
            "{quadlet:?}"
        );
        assert_eq!(new.workdir, "/home/u/develop/penpot");
        assert_eq!(old.active, "inactive");
    }

    #[test]
    fn a_compose_container_belongs_to_the_unit_running_its_folder() {
        let units = parse_units(SHOW);
        let owner = owner_of(&penpot_labels("/home/u/develop/penpot"), &units).expect("owned");
        assert_eq!(owner.unit, "penpot.service");
        assert_eq!(owner.project, "penpot");
        assert!(!owner.fragile && !owner.removes);

        let old = owner_of(&penpot_labels("/home/u/develop/old-penpot"), &units).expect("owned");
        assert_eq!(old.unit, "old-penpot.service");
        assert!(old.fragile && old.removes);
    }

    #[test]
    fn a_quadlet_container_names_its_own_unit() {
        let units = parse_units(SHOW);
        let owner =
            owner_of(&labels(&[("PODMAN_SYSTEMD_UNIT", "web.service")]), &units).expect("owned");
        assert_eq!(owner.unit, "web.service");
        assert_eq!(owner.project, "web");
    }

    #[test]
    fn a_container_no_unit_runs_has_no_owner() {
        let units = parse_units(SHOW);
        assert_eq!(owner_of(&HashMap::new(), &units), None);
        let loose = labels(&[
            ("io.podman.compose.project", "scratch"),
            ("com.docker.compose.project.working_dir", "/tmp/scratch"),
        ]);
        assert_eq!(owner_of(&loose, &units), None);
        assert_eq!(compose_project(&loose), "scratch");
    }

    #[test]
    fn stopping_an_owned_container_offers_its_unit_and_warns_when_fragile() {
        let th = &themes()[0];
        let units = parse_units(SHOW);
        let safe = owner_of(&penpot_labels("/home/u/develop/penpot"), &units).expect("owned");
        let (title, lines) = confirm_lines(
            th,
            &Pending::StopOwned("id".into(), "penpot-mcp".into(), safe),
        );
        let body = text(&lines);
        assert_eq!(title, " confirm stop ");
        assert!(
            body.contains("Run by systemd unit penpot.service (project penpot)"),
            "{body}"
        );
        assert!(
            body.contains("c = just this container")
                && body.contains("u = whole project (penpot.service)"),
            "{body}"
        );
        assert!(!body.contains("REMOVES"), "{body}");

        let fragile =
            owner_of(&penpot_labels("/home/u/develop/old-penpot"), &units).expect("owned");
        let (_, lines) = confirm_lines(
            th,
            &Pending::StopOwned("id".into(), "penpot-mcp".into(), fragile),
        );
        let body = text(&lines);
        assert!(
            body.contains("ends the unit, which stops the rest"),
            "{body}"
        );
        assert!(body.contains("REMOVES them all (compose down)"), "{body}");
    }

    #[test]
    fn removing_offers_the_unit_only_when_there_is_one() {
        let th = &themes()[0];
        let units = parse_units(SHOW);
        let owner = owner_of(&penpot_labels("/home/u/develop/penpot"), &units);
        let (_, lines) = confirm_lines(
            th,
            &Pending::RemoveContainer("id".into(), "penpot-mcp".into(), owner),
        );
        assert!(text(&lines).contains("u = stop project (penpot.service) instead"));
        let (_, lines) = confirm_lines(
            th,
            &Pending::RemoveContainer("id".into(), "loose".into(), None),
        );
        let body = text(&lines);
        assert!(!body.contains("u = ") && body.contains("y = yes"), "{body}");
    }
}
