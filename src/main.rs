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
use std::process::Command;
use std::time::Duration;

#[derive(PartialEq, Clone, Copy)]
enum Screen {
    Containers,
    Pods,
}

enum Pending {
    RemoveContainer(String, String), // id, label
    RemovePod(String, String),       // id, label
}

struct Ctr {
    id: String,
    name: String,
    image: String,
    state: String,
    status: String,
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
        NewForm { network: "dev-net".into(), ..Default::default() }
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
    cstate: TableState,
    pstate: TableState,
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
        Self {
            docker,
            containers: Vec::new(),
            pods: Vec::new(),
            cstate,
            pstate,
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
        // containers (bollard)
        let opts = ListContainersOptions::<String> { all: true, ..Default::default() };
        match self.docker.list_containers(Some(opts)).await {
            Ok(list) => {
                self.containers = list
                    .into_iter()
                    .map(|c| Ctr {
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
                    })
                    .collect();
            }
            Err(e) => self.msg = format!("list error: {e}"),
        }
        // pods (podman CLI)
        self.pods = fetch_pods();

        clamp(&mut self.cstate, self.containers.len());
        clamp(&mut self.pstate, self.pods.len());
        self.msg = format!("{} containers · {} pods", self.containers.len(), self.pods.len());
    }

    fn active_len(&self) -> usize {
        match self.screen {
            Screen::Containers => self.containers.len(),
            Screen::Pods => self.pods.len(),
        }
    }
    fn active_state(&mut self) -> &mut TableState {
        match self.screen {
            Screen::Containers => &mut self.cstate,
            Screen::Pods => &mut self.pstate,
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
            write_and_start_quadlet(&name, &image, &network, &ports, &memory, &cpus, &volume, &depends)
        } else {
            run_container(&image, &name, &network, &ports, &memory, &cpus, &volume, &depends)
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
            Pending::RemoveContainer(id, label) => {
                let r = self
                    .docker
                    .remove_container(&id, Some(RemoveContainerOptions { force: true, ..Default::default() }))
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
    let mut a = vec!["--user"];
    a.extend_from_slice(args);
    match Command::new("systemctl").args(&a).output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
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
    Ok(format!("quadlet {name} created + started (autostart on boot)"))
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
                Pod { name: g(0), id: g(1), status: g(2), nctr: g(3) }
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
        let Event::Key(k) = event::read()? else { continue };
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
        if app.confirm.is_some() {
            match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if let Some(act) = app.confirm.take() {
                        app.perform(act).await;
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
                    Screen::Pods => Screen::Containers,
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
                        app.logs = Some(LogView { title, lines, offset: usize::MAX, max_off: 0 });
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
                        let r = app.docker.start_container(&id, None::<StartContainerOptions<String>>).await;
                        app.msg = match r { Ok(_) => format!("started {label}"), Err(e) => format!("start failed: {e}") };
                        app.refresh().await;
                    }
                }
                Screen::Pods => {
                    if let Some(p) = app.sel_pod() {
                        let (id, label) = (p.id.clone(), p.name.clone());
                        app.msg = match podman(&["pod", "start", &id]) { Ok(_) => format!("started pod {label}"), Err(e) => format!("pod start failed: {e}") };
                        app.refresh().await;
                    }
                }
            },
            KeyCode::Char('x') => match app.screen {
                Screen::Containers => {
                    if let Some(c) = app.sel_ctr() {
                        let (id, label) = (c.id.clone(), c.name.clone());
                        let r = app.docker.stop_container(&id, None::<StopContainerOptions>).await;
                        app.msg = match r { Ok(_) => format!("stopped {label}"), Err(e) => format!("stop failed: {e}") };
                        app.refresh().await;
                    }
                }
                Screen::Pods => {
                    if let Some(p) = app.sel_pod() {
                        let (id, label) = (p.id.clone(), p.name.clone());
                        app.msg = match podman(&["pod", "stop", &id]) { Ok(_) => format!("stopped pod {label}"), Err(e) => format!("pod stop failed: {e}") };
                        app.refresh().await;
                    }
                }
            },
            KeyCode::Char('X') => match app.screen {
                Screen::Containers => {
                    if let Some(c) = app.sel_ctr() {
                        app.confirm = Some(Pending::RemoveContainer(c.id.clone(), c.name.clone()));
                    }
                }
                Screen::Pods => {
                    if let Some(p) = app.sel_pod() {
                        app.confirm = Some(Pending::RemovePod(p.id.clone(), p.name.clone()));
                    }
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
        Theme { name: "dark",      fg: Color::Gray,  bg: Color::Reset,                   accent: Color::Cyan,            on: Color::Green,          warn: Color::Yellow,        danger: Color::Red,            off: Color::DarkGray },
        Theme { name: "light",     fg: Color::Black, bg: Color::White,                   accent: Color::Blue,            on: Color::Rgb(0,135,0),   warn: Color::Rgb(181,137,0), danger: Color::Rgb(197,15,31), off: Color::Gray },
        Theme { name: "solarized", fg: Color::Rgb(131,148,150), bg: Color::Rgb(0,43,54), accent: Color::Rgb(38,139,210), on: Color::Rgb(133,153,0), warn: Color::Rgb(181,137,0), danger: Color::Rgb(220,50,47), off: Color::Rgb(88,110,117) },
        Theme { name: "gruvbox",   fg: Color::Rgb(235,219,178), bg: Color::Rgb(40,40,40), accent: Color::Rgb(250,189,47), on: Color::Rgb(184,187,38), warn: Color::Rgb(254,128,25), danger: Color::Rgb(251,73,52), off: Color::Rgb(146,131,116) },
    ]
}

fn config_theme_path() -> String {
    format!("{}/.config/podmangr/theme", std::env::var("HOME").unwrap_or_default())
}
fn load_theme() -> usize {
    std::fs::read_to_string(config_theme_path())
        .ok()
        .and_then(|s| { let n = s.trim().to_string(); themes().iter().position(|t| t.name == n) })
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
    if app.logs.is_some() {
        let areas = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(f.area());
        let lv = app.logs.as_mut().unwrap();
        let inner_h = areas[0].height.saturating_sub(2) as usize;
        lv.max_off = lv.lines.len().saturating_sub(inner_h);
        let off = lv.offset.min(lv.max_off);
        let visible: Vec<Line> = lv.lines[off..]
            .iter()
            .take(inner_h.max(1))
            .map(|l| Line::raw(l.clone()))
            .collect();
        let title = format!(" logs: {}  ({}/{}) ", lv.title, off + visible.len(), lv.lines.len());
        f.render_widget(
            Paragraph::new(visible).block(Block::default().borders(Borders::ALL).title(title)),
            areas[0],
        );
        let help = Line::from(vec![
            Span::styled(" j/k", Style::new().fg(th.accent)), Span::raw(" scroll  "),
            Span::styled("PgUp/PgDn", Style::new().fg(th.accent)), Span::raw(" page  "),
            Span::styled("g/G", Style::new().fg(th.accent)), Span::raw(" top/bottom  "),
            Span::styled("q/Esc", Style::new().fg(th.accent)), Span::raw(" back"),
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
    let (ctab, ptab) = match app.screen {
        Screen::Containers => (Style::new().bg(th.accent).fg(th.bg).bold(), Style::new().fg(th.off)),
        Screen::Pods => (Style::new().fg(th.off), Style::new().bg(th.accent).fg(th.bg).bold()),
    };
    let tabs = Line::from(vec![
        Span::styled(" Containers ", ctab),
        Span::raw("  "),
        Span::styled(" Pods ", ptab),
        Span::raw("   (Tab to switch)"),
    ]);
    f.render_widget(Paragraph::new(tabs), chunks[0]);

    match app.screen {
        Screen::Containers => {
            let header = Row::new(["NAME", "IMAGE", "STATE", "STATUS"]).style(Style::new().fg(th.accent).bold());
            let rows = app.containers.iter().map(|c| {
                Row::new(vec![
                    Cell::from(c.name.clone()),
                    Cell::from(c.image.clone()),
                    Cell::from(c.state.clone()).style(state_style(th, &c.state)),
                    Cell::from(c.status.clone()),
                ])
            });
            let widths = [
                Constraint::Percentage(28),
                Constraint::Percentage(34),
                Constraint::Length(10),
                Constraint::Percentage(30),
            ];
            let t = Table::new(rows, widths)
                .header(header)
                .block(Block::default().borders(Borders::ALL).title(" podmangr — containers "))
                .row_highlight_style(Style::new().bg(th.accent).fg(th.bg))
                .highlight_symbol("› ");
            f.render_stateful_widget(t, chunks[1], &mut app.cstate);
        }
        Screen::Pods => {
            let header = Row::new(["NAME", "ID", "STATUS", "#CTRS"]).style(Style::new().fg(th.accent).bold());
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
                .block(Block::default().borders(Borders::ALL).title(" podmangr — pods "))
                .row_highlight_style(Style::new().bg(th.accent).fg(th.bg))
                .highlight_symbol("› ");
            f.render_stateful_widget(t, chunks[1], &mut app.pstate);
        }
    }

    let help = Line::from(vec![
        Span::styled(" Tab", Style::new().fg(th.accent)), Span::raw(" screen  "),
        Span::styled("j/k", Style::new().fg(th.accent)), Span::raw(" move  "),
        Span::styled("s", Style::new().fg(th.accent)), Span::raw(" start  "),
        Span::styled("x", Style::new().fg(th.accent)), Span::raw(" stop  "),
        Span::styled("X", Style::new().fg(th.accent)), Span::raw(" rm  "),
        Span::styled("l", Style::new().fg(th.accent)), Span::raw(" logs  "),
        Span::styled("n", Style::new().fg(th.accent)), Span::raw(" new  "),
        Span::styled("r", Style::new().fg(th.accent)), Span::raw(" refresh  "),
        Span::styled("t", Style::new().fg(th.accent)), Span::raw(" theme  "),
        Span::styled("q", Style::new().fg(th.accent)), Span::raw(" quit   "),
        Span::styled(format!("[{}]", app.msg), Style::new().fg(th.off)),
    ]);
    f.render_widget(Paragraph::new(help), chunks[2]);

    // confirm modal overlay
    if let Some(p) = &app.confirm {
        let (what, label) = match p {
            Pending::RemoveContainer(_, l) => ("container", l.clone()),
            Pending::RemovePod(_, l) => ("pod", l.clone()),
        };
        let area = centered(f.area(), 54, 7);
        f.render_widget(Clear, area);
        let body = Paragraph::new(vec![
            Line::raw(""),
            Line::from(vec![
                Span::raw(format!("  Remove {what} ")),
                Span::styled(label, Style::new().bold().fg(th.warn)),
                Span::raw(" ?"),
            ]),
            Line::raw(""),
            Line::from(vec![
                Span::raw("      "),
                Span::styled("y", Style::new().fg(th.on).bold()),
                Span::raw(" = yes    "),
                Span::styled("n", Style::new().fg(th.danger).bold()),
                Span::raw("/Esc = no"),
            ]),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" confirm remove ")
                .border_style(Style::new().fg(th.danger)),
        );
        f.render_widget(body, area);
    }

    // new-container form overlay
    if let Some(fm) = &app.new_form {
        let area = centered(f.area(), 66, 20);
        f.render_widget(Clear, area);
        let labels = [
            "Image *", "Name", "Network", "Ports (host:ctr)", "Memory (e.g. 512m)",
            "CPUs (e.g. 1.5)", "Volume (vol:/path)", "Depends (unit/ctr)",
        ];
        let vals = [
            &fm.image, &fm.name, &fm.network, &fm.ports, &fm.memory, &fm.cpus, &fm.volume, &fm.depends,
        ];
        let mut lines: Vec<Line> = vec![Line::raw("")];
        for i in 0..8 {
            let marker = if fm.focus == i { "› " } else { "  " };
            let vstyle = if fm.focus == i { Style::new().bg(th.accent).fg(th.bg) } else { Style::new() };
            lines.push(Line::from(vec![
                Span::raw(marker),
                Span::styled(format!("{:<20}", labels[i]), Style::new().fg(th.accent)),
                Span::styled(vals[i].clone(), vstyle),
            ]));
        }
        let m8 = if fm.focus == 8 { "› " } else { "  " };
        lines.push(Line::from(vec![
            Span::raw(m8),
            Span::styled(format!("{:<20}", "Autostart (space)"), Style::new().fg(th.accent)),
            Span::raw(if fm.autostart { "[x]  → Quadlet systemd unit" } else { "[ ]" }),
        ]));
        lines.push(Line::raw(""));
        if !fm.err.is_empty() {
            lines.push(Line::from(Span::styled(format!("  {}", fm.err), Style::new().fg(th.danger))));
        }
        lines.push(Line::from(Span::styled(
            "  Enter submit · Tab/↑↓ move · space toggles autostart · Esc cancel",
            Style::new().fg(th.off),
        )));
        f.render_widget(
            Paragraph::new(lines)
                .block(Block::default().borders(Borders::ALL).title(" new container ").border_style(Style::new().fg(th.accent))),
            area,
        );
    }
}

fn state_style(th: &Theme, state: &str) -> Style {
    match state {
        s if s.contains("running") || s.contains("up") => Style::new().fg(th.on),
        s if s.contains("paused") || s.contains("degraded") => Style::new().fg(th.warn),
        s if s.contains("exited") || s.contains("stopped") || s.contains("created") || s.contains("dead") || s.contains("down") => {
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
