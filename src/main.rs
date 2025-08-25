use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode as TermKeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
#[cfg(feature = "wacom")]
use evdev::{Device, EventType, KeyCode};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph, Sparkline, Wrap},
};
use serde::Deserialize;
use std::{
    collections::VecDeque,
    fs,
    io::{self, Stdout},
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{net::UdpSocket, sync::watch, task, time::sleep};

const BUF: usize = 8192;
const TICK_MS: u64 = 100;
const HISTORY: usize = 300;
const INPUT_LOG_CAP: usize = 200;

// We still track ABS for logging context, but mapping no longer depends on it.
const SIDE_TIMEOUT_MS: u128 = 250;

// ---------------- Telemetry model ----------------

#[derive(Debug, Clone, Deserialize, Default)]
struct Telemetry {
    name: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    alt_msl: Option<f64>,
    alt_agl: Option<f64>,
    ias_ms: Option<f64>,
    tas_ms: Option<f64>,
    mach: Option<f64>,
    aoa_rad: Option<f64>,
    vv_ms: Option<f64>,
    #[serde(default)]
    att: Option<Att>,
    #[serde(default)]
    accel: Option<Accel>,
    #[serde(default)]
    engine: Option<Engine>,
    #[serde(default)]
    mech: Option<Mech>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct Att {
    pitch: Option<f64>,
    bank: Option<f64>,
    yaw: Option<f64>,
}
#[derive(Debug, Clone, Deserialize, Default)]
struct Accel {
    x: Option<f64>,
    y: Option<f64>,
    z: Option<f64>,
}
#[derive(Debug, Clone, Deserialize, Default)]
struct Pair {
    L: Option<f64>,
    R: Option<f64>,
}
#[derive(Debug, Clone, Deserialize, Default)]
struct Engine {
    #[serde(default)]
    rpm: Option<Pair>,
    #[serde(default)]
    thrtl: Option<Pair>,
    #[serde(default)]
    thrtl_est: Option<bool>,
    #[serde(default)]
    noz: Option<Pair>,
    #[serde(default)]
    noz_present: Option<bool>,
    #[serde(default)]
    temp: Option<Pair>,
    #[serde(default)]
    fuelf: Option<Pair>,
    #[serde(default)]
    map: Option<Pair>,
    #[serde(default)]
    map_present: Option<bool>,
}
#[derive(Debug, Clone, Deserialize, Default)]
struct Mech {
    gear: Option<f64>,
    flaps: Option<f64>,
    airbrake: Option<f64>,
    hook: Option<f64>,
    wing: Option<f64>,
    wow: Option<f64>,
    #[serde(default)]
    wow_guess: Option<bool>,
}

// ---------------- UI state ----------------

impl Default for Pane {
    fn default() -> Self {
        Pane::Flight
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Flight = 0,
    Att = 1,
    Systems = 2,
    Inputs = 3,
    IasChart = 4,
    AltChart = 5,
}
const PANE_COUNT: usize = 6;

impl Pane {
    fn from_index(i: usize) -> Pane {
        match i {
            0 => Pane::Flight,
            1 => Pane::Att,
            2 => Pane::Systems,
            3 => Pane::Inputs,
            4 => Pane::IasChart,
            _ => Pane::AltChart,
        }
    }
    fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, Default)]
struct UiState {
    last: Telemetry,
    ias_hist: VecDeque<f64>,
    alt_hist: VecDeque<f64>,
    input_log: VecDeque<String>,
    focused: Pane,
    fullscreen: Option<Pane>,
}

// ---------------- Small helpers ----------------

/// Try to open a Wacom pad **once**. If not found, return None (don’t block).
#[cfg(feature = "wacom")]
fn try_open_wacom_pad_now() -> Option<(String, Device)> {
    if let Some(t) = open_wacom_from_env() {
        return Some(t);
    }
    // Prefer stable by-id paths
    if let Ok(entries) = fs::read_dir("/dev/input/by-id") {
        for ent in entries.flatten() {
            let p: PathBuf = ent.path();
            if let Ok(tgt) = fs::canonicalize(&p) {
                let name = p
                    .file_name()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or_default();
                if name.contains("Wacom")
                    && name.to_ascii_lowercase().contains("pad")
                    && name.contains("event")
                {
                    if let Ok(d) = Device::open(&tgt) {
                        return Some((tgt.display().to_string(), d));
                    }
                }
            }
        }
    }
    // Fallback scan of /dev/input
    if let Ok(entries) = fs::read_dir("/dev/input") {
        for ent in entries.flatten() {
            let p = ent.path();
            let fname = p
                .file_name()
                .map(|s| s.to_string_lossy())
                .unwrap_or_default();
            if !fname.starts_with("event") {
                continue;
            }
            if let Ok(d) = Device::open(&p) {
                let n = d.name().unwrap_or("");
                if n.contains("Wacom") && n.contains("Pad") {
                    return Some((p.display().to_string(), d));
                }
            }
        }
    }
    None
}

fn push_hist(q: &mut VecDeque<f64>, v: f64, cap: usize) {
    q.push_back(v);
    while q.len() > cap {
        q.pop_front();
    }
}
fn push_log(q: &mut VecDeque<String>, line: String) {
    q.push_back(line);
    while q.len() > INPUT_LOG_CAP {
        q.pop_front();
    }
}
fn last_n_scaled(src: &VecDeque<f64>, n: usize, scale: f64) -> Vec<u64> {
    let len = src.len();
    let start = len.saturating_sub(n);
    src.iter()
        .skip(start)
        .take(n)
        .map(|v| (*v * scale).max(0.0) as u64)
        .collect()
}
fn last_n(src: &VecDeque<f64>, n: usize) -> Vec<u64> {
    let len = src.len();
    let start = len.saturating_sub(n);
    src.iter()
        .skip(start)
        .take(n)
        .map(|v| v.max(0.0) as u64)
        .collect()
}
fn fmt_ts(ts: SystemTime) -> (u64, u32) {
    match ts.duration_since(UNIX_EPOCH) {
        Ok(d) => ((d.as_secs() % 1000) as u64, d.subsec_micros()),
        Err(_) => (0, 0),
    }
}

// ---------------- Runtime wiring ----------------

#[tokio::main]
async fn main() -> Result<()> {
    let (tx, rx) = watch::channel(UiState::default());
    let port = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5010);

    task::spawn(udp_listener(
        format!("127.0.0.1:{port}"),
        tx.clone(),
        rx.clone(),
    ));
    // Optional Wacom: start movement logic only if a device is available right now.
    #[cfg(feature = "wacom")]
    {
        if let Some((path, dev)) = try_open_wacom_pad_now() {
            eprintln!("Using Wacom pad at {}", path);
            task::spawn(wacom_listener_with_device(
                tx.clone(),
                rx.clone(),
                path,
                dev,
            ));
        } else {
            eprintln!(
                "No Wacom pad found (or no permission). Running dashboard without pad controls."
            );
        }
    }

    run_tui(rx).await
}

#[cfg(feature = "wacom")]
async fn wacom_listener_with_device(
    tx: watch::Sender<UiState>,
    rx: watch::Receiver<UiState>,
    path: String,
    mut dev: Device,
) {
    // For logging context
    let mut last_side_hint = Side::Left;
    let mut last_abs_misc: i32 = 0;
    let mut last_abs_at = Instant::now() - Duration::from_millis(SIDE_TIMEOUT_MS as u64 + 1);

    loop {
        match dev.fetch_events() {
            Ok(iter) => {
                let mut saw = false;
                for ev in iter {
                    saw = true;

                    if ev.event_type() == EventType::ABSOLUTE {
                        if let Some(s) = side_from_abs(ev.code(), ev.value()) {
                            last_side_hint = s;
                            last_abs_at = Instant::now();
                        }
                        if ev.code() == 40 {
                            last_abs_misc = ev.value();
                        }
                    }

                    if ev.event_type() == EventType::KEY && ev.value() == 1 {
                        let code_u16 = ev.code();
                        let act = map_btn_code(code_u16);

                        let mut state = rx.borrow().clone();
                        match act {
                            PadAction::Select => {
                                if state.fullscreen == Some(state.focused) {
                                    state.fullscreen = None;
                                } else {
                                    state.fullscreen = Some(state.focused);
                                }
                            }
                            PadAction::Up
                            | PadAction::Down
                            | PadAction::Left
                            | PadAction::Right => {
                                state.focused = move_focus(state.focused, act);
                            }
                            PadAction::Unknown => {}
                        }

                        let side_for_log = side_from_code(code_u16)
                            .or_else(|| {
                                if last_abs_at.elapsed().as_millis() <= SIDE_TIMEOUT_MS {
                                    Some(last_side_hint)
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(last_side_hint);

                        let (s, us) = fmt_ts(ev.timestamp());
                        push_log(
                            &mut state.input_log,
                            format!(
                                "[{:>3}.{:06}] {:?} (code={}, ABS_MISC={}) -> {:?} ({:?} side)",
                                s,
                                us,
                                KeyCode::new(code_u16),
                                code_u16,
                                last_abs_misc,
                                act,
                                side_for_log
                            ),
                        );
                        let _ = tx.send(state);
                    }
                }
                if !saw {
                    sleep(Duration::from_millis(10)).await;
                }
            }
            Err(e) => {
                eprintln!("Wacom read error ({}): {}", path, e);
                sleep(Duration::from_millis(300)).await;
            }
        }
    }
}

async fn udp_listener(bind: String, tx: watch::Sender<UiState>, rx: watch::Receiver<UiState>) {
    let sock = match UdpSocket::bind(&bind).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Bind failed on {bind}: {e}");
            return;
        }
    };
    let mut buf = vec![0u8; BUF];
    loop {
        match sock.recv_from(&mut buf).await {
            Ok((n, _)) => {
                for line in std::str::from_utf8(&buf[..n]).unwrap_or("").split('\n') {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(t) = serde_json::from_str::<Telemetry>(line) {
                        let mut state = rx.borrow().clone();
                        push_hist(&mut state.ias_hist, t.ias_ms.unwrap_or(0.0), HISTORY);
                        push_hist(&mut state.alt_hist, t.alt_msl.unwrap_or(0.0), HISTORY);
                        state.last = t;
                        let _ = tx.send(state);
                    }
                }
            }
            Err(e) => {
                eprintln!("UDP recv error: {e}");
                sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

// ---------------- Wacom / evdev ----------------

/// Optional override: set WACOM_EVENT=/dev/input/eventXX
#[cfg(feature = "wacom")]
fn open_wacom_from_env() -> Option<(String, Device)> {
    if let Ok(p) = std::env::var("WACOM_EVENT") {
        match Device::open(&p) {
            Ok(d) => {
                eprintln!("Using WACOM_EVENT {}", p);
                return Some((p, d));
            }
            Err(e) => eprintln!("WACOM_EVENT={} open failed: {}", p, e),
        }
    }
    None
}

/// Try /dev/input/by-id first (stable symlinks), then /dev/input
#[cfg(feature = "wacom")]
fn find_wacom_pad() -> Option<(String, Device)> {
    if let Some(t) = open_wacom_from_env() {
        return Some(t);
    }

    if let Ok(entries) = fs::read_dir("/dev/input/by-id") {
        for ent in entries.flatten() {
            let p: PathBuf = ent.path();
            if let Ok(tgt) = fs::canonicalize(&p) {
                let name = p
                    .file_name()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or_default();
                if name.contains("Wacom")
                    && name.to_ascii_lowercase().contains("pad")
                    && name.contains("event")
                {
                    match Device::open(&tgt) {
                        Ok(d) => {
                            eprintln!("Wacom pad (by-id): {} -> {}", name, tgt.display());
                            return Some((tgt.display().to_string(), d));
                        }
                        Err(e) => eprintln!("Found {} but open failed: {}", tgt.display(), e),
                    }
                }
            }
        }
    }

    if let Ok(entries) = fs::read_dir("/dev/input") {
        for ent in entries.flatten() {
            let p = ent.path();
            let fname = p
                .file_name()
                .map(|s| s.to_string_lossy())
                .unwrap_or_default();
            if !fname.starts_with("event") {
                continue;
            }
            match Device::open(&p) {
                Ok(d) => {
                    let n = d.name().unwrap_or("");
                    if n.contains("Wacom") && n.contains("Pad") {
                        eprintln!("Wacom pad: {} ({})", n, p.display());
                        return Some((p.display().to_string(), d));
                    }
                }
                Err(e) => eprintln!("Skip {} (open failed): {}", p.display(), e),
            }
        }
    }
    None
}

#[derive(Debug, Clone, Copy)]
enum PadAction {
    Up,
    Down,
    Left,
    Right,
    Select, // toggle fullscreen
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

/// (For logging only) ABS_MISC (code 40) often flips between 0 and >0 when you touch/use a side.
#[cfg(feature = "wacom")]
fn side_from_abs(code_u16: u16, val: i32) -> Option<Side> {
    match code_u16 {
        40 /* ABS_MISC */ => {
            if val > 0 { Some(Side::Right) } else { Some(Side::Left) }
        }
        _ => None,
    }
}

/// Map by raw button code (works for both sides).
#[cfg(feature = "wacom")]
fn map_btn_code(code_u16: u16) -> PadAction {
    match code_u16 {
        // LEFT PAD
        264 => PadAction::Up,     // Top
        259 => PadAction::Down,   // Bottom
        258 => PadAction::Select, // Tall -> fullscreen toggle
        256 => PadAction::Left,   // Mid-UR
        257 => PadAction::Right,  // Mid-LR

        // RIGHT PAD
        265 => PadAction::Up,     // Top acts as Left
        263 => PadAction::Down,   // Bottom acts as Right
        262 => PadAction::Select, // Tall -> fullscreen toggle
        260 => PadAction::Left,   // Mid-UR acts as Left
        261 => PadAction::Right,  // Mid-LR acts as Right

        _ => PadAction::Unknown,
    }
}

/// Infer side from the raw code (for nicer logs).
#[cfg(feature = "wacom")]
fn side_from_code(code_u16: u16) -> Option<Side> {
    match code_u16 {
        256 | 257 | 258 | 259 | 264 => Some(Side::Left),
        260 | 261 | 262 | 263 | 265 => Some(Side::Right),
        _ => None,
    }
}

fn move_focus(focused: Pane, dir: PadAction) -> Pane {
    use Pane::*;
    match dir {
        PadAction::Left => match focused {
            Flight => Systems, // wrap within the top row of 3
            Att => Flight,
            Systems => Att,
            IasChart | AltChart => focused, // left/right do nothing on charts
            Inputs => Flight,               // defensive: if ever focused, bounce to visible
        },
        PadAction::Right => match focused {
            Flight => Att,
            Att => Systems,
            Systems => Flight, // wrap
            IasChart | AltChart => focused,
            Inputs => Flight, // defensive
        },
        PadAction::Up => match focused {
            IasChart => Flight,
            AltChart => IasChart,
            other => other,
        },
        PadAction::Down => match focused {
            Flight | Att | Systems => IasChart,
            IasChart => AltChart,
            AltChart => AltChart,
            Inputs => IasChart, // defensive
        },
        _ => focused,
    }
}

#[cfg(feature = "wacom")]
async fn wacom_listener(tx: watch::Sender<UiState>, rx: watch::Receiver<UiState>) {
    let (path, mut dev) = loop {
        match find_wacom_pad() {
            Some((p, d)) => break (p, d),
            None => {
                eprintln!("No readable Wacom pad yet; retrying…");
                sleep(Duration::from_millis(1500)).await;
            }
        }
    };

    // For logging context
    let mut last_side_hint = Side::Left;
    let mut last_abs_misc: i32 = 0;
    let mut last_abs_at = Instant::now() - Duration::from_millis(SIDE_TIMEOUT_MS as u64 + 1);

    loop {
        match dev.fetch_events() {
            Ok(iter) => {
                let mut saw = false;
                for ev in iter {
                    saw = true;

                    if ev.event_type() == EventType::ABSOLUTE {
                        if let Some(s) = side_from_abs(ev.code(), ev.value()) {
                            last_side_hint = s;
                            last_abs_at = Instant::now();
                        }
                        if ev.code() == 40 {
                            last_abs_misc = ev.value();
                        }
                    }

                    // Only react on key DOWN
                    if ev.event_type() == EventType::KEY && ev.value() == 1 {
                        let code_u16 = ev.code();
                        let act = map_btn_code(code_u16);

                        let mut state = rx.borrow().clone();
                        match act {
                            PadAction::Select => {
                                if state.fullscreen == Some(state.focused) {
                                    state.fullscreen = None;
                                } else {
                                    state.fullscreen = Some(state.focused);
                                }
                            }
                            PadAction::Up
                            | PadAction::Down
                            | PadAction::Left
                            | PadAction::Right => {
                                state.focused = move_focus(state.focused, act);
                            }
                            PadAction::Unknown => {}
                        }

                        // Prefer inferring side from code; fall back to recent ABS hint
                        let side_for_log = side_from_code(code_u16)
                            .or_else(|| {
                                if last_abs_at.elapsed().as_millis() <= SIDE_TIMEOUT_MS {
                                    Some(last_side_hint)
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(last_side_hint);

                        let (s, us) = fmt_ts(ev.timestamp());
                        push_log(
                            &mut state.input_log,
                            format!(
                                "[{:>3}.{:06}] {:?} (code={}, ABS_MISC={}) -> {:?} ({:?} side)",
                                s,
                                us,
                                KeyCode::new(code_u16),
                                code_u16,
                                last_abs_misc,
                                act,
                                side_for_log
                            ),
                        );
                        let _ = tx.send(state);
                    }
                }
                if !saw {
                    sleep(Duration::from_millis(10)).await;
                }
            }
            Err(e) => {
                eprintln!("Wacom read error ({}): {}", path, e);
                sleep(Duration::from_millis(300)).await;
            }
        }
    }
}

// ---------------- TUI ----------------

async fn run_tui(rx: watch::Receiver<UiState>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut last_redraw = Instant::now();

    'ui: loop {
        while event::poll(Duration::from_millis(0))? {
            if let Event::Key(KeyEvent {
                code, modifiers, ..
            }) = event::read()?
            {
                match (code, modifiers) {
                    (TermKeyCode::Char('c'), KeyModifiers::CONTROL)
                    | (TermKeyCode::Char('q'), KeyModifiers::NONE)
                    | (TermKeyCode::Esc, _) => break 'ui,
                    _ => {}
                }
            }
        }

        if last_redraw.elapsed() >= Duration::from_millis(TICK_MS) {
            let state = rx.borrow().clone();
            terminal.draw(|f| draw(f, &state))?;
            last_redraw = Instant::now();
        }

        sleep(Duration::from_millis(10)).await;
    }

    disable_raw_mode()?;
    let mut out: Stdout = io::stdout();
    execute!(out, LeaveAlternateScreen)?;
    Ok(())
}

fn draw(f: &mut Frame, s: &UiState) {
    // header area
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(12),
            Constraint::Min(6),
            Constraint::Min(6),
        ])
        .split(f.area());

    // Fullscreen: only draw header + focused pane stretched
    if let Some(fs) = s.fullscreen {
        f.render_widget(header_line(&s.last), layout[0]);
        let fs_area = Rect {
            x: layout[1].x,
            y: layout[1].y,
            width: layout[1].width,
            height: layout[1].height + layout[2].height + layout[3].height,
        };
        draw_one_pane(f, s, fs, fs_area, true);
        return;
    }

    // normal layout
    f.render_widget(header_line(&s.last), layout[0]);

    // top row 4 columns
    let stats_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(33),
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            // Constraint::Percentage(25),
        ])
        .split(layout[1]);

    draw_one_pane(f, s, Pane::Flight, stats_row[0], false);
    draw_one_pane(f, s, Pane::Att, stats_row[1], false);
    draw_one_pane(f, s, Pane::Systems, stats_row[2], false);
    // draw_one_pane(f, s, Pane::Inputs, stats_row[3], false);

    // charts (full width blocks)
    draw_one_pane(f, s, Pane::IasChart, layout[2], false);
    draw_one_pane(f, s, Pane::AltChart, layout[3], false);
}

fn draw_one_pane(f: &mut Frame, s: &UiState, which: Pane, area: Rect, fullscreen: bool) {
    let is_focused = s.focused == which && !fullscreen;

    match which {
        Pane::Flight => {
            let block = Block::default()
                .borders(Borders::ALL)
                .title("Flight")
                .border_style(if is_focused {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                });
            let w = Paragraph::new(format_info_left(&s.last))
                .block(block)
                .wrap(Wrap { trim: true });
            f.render_widget(w, area);
        }
        Pane::Att => {
            let block = Block::default()
                .borders(Borders::ALL)
                .title("Att/Accel")
                .border_style(if is_focused {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                });
            let w = Paragraph::new(format_info_right(&s.last))
                .block(block)
                .wrap(Wrap { trim: true });
            f.render_widget(w, area);
        }
        Pane::Systems => {
            let block = Block::default()
                .borders(Borders::ALL)
                .title("Systems")
                .border_style(if is_focused {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                });
            let w = Paragraph::new(format_systems(&s.last))
                .block(block)
                .wrap(Wrap { trim: true });
            f.render_widget(w, area);
        }
        Pane::Inputs => {
            let max_lines = 16usize;
            let len = s.input_log.len();
            let start = len.saturating_sub(max_lines);
            let inputs_text = s
                .input_log
                .iter()
                .skip(start)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            let block = Block::default()
                .borders(Borders::ALL)
                .title("Inputs")
                .border_style(if is_focused {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                });
            let w = Paragraph::new(inputs_text)
                .block(block)
                .wrap(Wrap { trim: false });
            f.render_widget(w, area);
        }
        Pane::IasChart => {
            let inner = area.width.saturating_sub(2) as usize;
            let data = last_n_scaled(&s.ias_hist, inner, 1.943_844);
            let block = Block::default()
                .borders(Borders::ALL)
                .title("IAS (kt)")
                .border_style(if is_focused {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                });
            let w = Sparkline::default().block(block).data(&data);
            f.render_widget(w, area);
        }
        Pane::AltChart => {
            let inner = area.width.saturating_sub(2) as usize;
            let data = last_n(&s.alt_hist, inner);
            let block = Block::default()
                .borders(Borders::ALL)
                .title("Altitude MSL (m)")
                .border_style(if is_focused {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                });
            let w = Sparkline::default().block(block).data(&data);
            f.render_widget(w, area);
        }
    }
}

// ---------------- Formatting helpers ----------------

fn header_line(t: &Telemetry) -> Paragraph<'static> {
    let name = t.name.as_deref().unwrap_or("?");
    let lat = t.lat.map(|v| format!("{v:.5}")).unwrap_or("-".into());
    let lon = t.lon.map(|v| format!("{v:.5}")).unwrap_or("-".into());
    Paragraph::new(format!(
        " DCS Dash — Airframe: {name}   POS: {lat}, {lon}   Ctrl+C / q / Esc to exit "
    ))
    .block(Block::default().borders(Borders::ALL).title("Status"))
}

fn format_info_left(t: &Telemetry) -> String {
    let ias_ms = t.ias_ms.unwrap_or(0.0);
    let ias_kt = ias_ms * 1.943_844;
    let ias_kmh = ias_ms * 3.6;
    let tas_ms = t.tas_ms.unwrap_or(0.0);
    let tas_kt = tas_ms * 1.943_844;
    let alt = t.alt_msl.unwrap_or(0.0);
    let agl = t.alt_agl.unwrap_or(0.0);
    let mach = t.mach.unwrap_or(0.0);
    let vv = t.vv_ms.unwrap_or(0.0);
    format!(
        "IAS: {:>6.1} kt ({:>6.1} km/h)\nTAS: {:>6.1} kt\nALT MSL: {:>8.0} m   AGL: {:>7.0} m\nMach: {:>4.2}   VV: {:>6.1} m/s",
        ias_kt, ias_kmh, tas_kt, alt, agl, mach, vv
    )
}

fn format_info_right(t: &Telemetry) -> String {
    let aoa_deg = t.aoa_rad.unwrap_or(0.0) * 57.295_779_5;
    let (p, b, y) = match &t.att {
        Some(a) => (
            a.pitch.unwrap_or(0.0) * 57.295_779_5,
            a.bank.unwrap_or(0.0) * 57.295_779_5,
            a.yaw.unwrap_or(0.0) * 57.295_779_5,
        ),
        None => (0.0, 0.0, 0.0),
    };
    let (ax, ay, az) = match &t.accel {
        Some(g) => (g.x.unwrap_or(0.0), g.y.unwrap_or(0.0), g.z.unwrap_or(0.0)),
        None => (0.0, 0.0, 0.0),
    };
    format!(
        "AoA: {:>5.2}°\nPitch: {:>6.2}°  Bank: {:>6.2}°  Yaw: {:>6.2}°\nAccel G: X {:>5.2}  Y {:>5.2}  Z {:>5.2}",
        aoa_deg, p, b, y, ax, ay, az
    )
}

fn fmt_pair_opt(label: &str, p: &Option<Pair>, scale_pct: bool) -> Option<String> {
    let to = p.as_ref()?;
    if to.L.is_none() && to.R.is_none() {
        return None;
    }
    let mut l = to.L;
    let mut r = to.R;
    if scale_pct {
        l = l.map(|x| x * 100.0);
        r = r.map(|x| x * 100.0);
    }
    let fmtv = |v: Option<f64>| v.map(|x| format!("{:>6.1}", x)).unwrap_or("   ---".into());
    Some(format!("{label}: L {}  R {}", fmtv(l), fmtv(r)))
}

fn format_systems(t: &Telemetry) -> String {
    let mut lines = Vec::new();

    if let Some(e) = &t.engine {
        if let Some(s) = fmt_pair_opt("RPM %", &e.rpm, false) {
            lines.push(s);
        }
        let thr_label = if e.thrtl_est.unwrap_or(false) {
            "THR % (est)"
        } else {
            "THR %"
        };
        lines.push(
            fmt_pair_opt(thr_label, &e.thrtl, true)
                .unwrap_or_else(|| format!("{thr_label}: L   ---  R   ---")),
        );
        if e.noz_present.unwrap_or(false) {
            lines.push(
                fmt_pair_opt("NOZ %", &e.noz, true)
                    .unwrap_or_else(|| "NOZ %: L   ---  R   ---".into()),
            );
        }
        if let Some(s) = fmt_pair_opt("TEMP", &e.temp, false) {
            lines.push(s);
        }
        if let Some(s) = fmt_pair_opt("FF", &e.fuelf, false) {
            lines.push(s);
        }
        if e.map_present.unwrap_or(false) {
            lines.push(
                fmt_pair_opt("MAP", &e.map, false)
                    .unwrap_or_else(|| "MAP: L   ---  R   ---".into()),
            );
        }
    }

    lines.push(String::new());

    let show = |label: &str, v: Option<f64>, guessed: bool| -> String {
        let lab = if guessed {
            format!("{label} (guess)")
        } else {
            label.to_string()
        };
        match v {
            Some(x) => format!("{lab}: {:>5.2}", x),
            None => format!("{lab}:   ---"),
        }
    };

    if let Some(m) = &t.mech {
        lines.push(show("Gear", m.gear, false));
        lines.push(show("Flaps", m.flaps, false));
        lines.push(show("Airbrk", m.airbrake, false));
        lines.push(show("Hook", m.hook, false));
        lines.push(show("Wing", m.wing, false));
        lines.push(show("WoW", m.wow, m.wow_guess.unwrap_or(false)));
    } else {
        lines.extend(
            [
                "Gear:   ---",
                "Flaps:  ---",
                "Airbrk: ---",
                "Hook:   ---",
                "Wing:   ---",
                "WoW:    ---",
            ]
            .map(str::to_string),
        );
    }

    lines.join("\n")
}
