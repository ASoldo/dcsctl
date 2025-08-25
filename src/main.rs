use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph, Sparkline, Wrap},
};
use serde::Deserialize;
use std::{
    collections::VecDeque,
    io::{self, Stdout},
    time::{Duration, Instant},
};
use tokio::{net::UdpSocket, sync::watch, task, time::sleep};

const BUF: usize = 8192;
const TICK_MS: u64 = 100;
const HISTORY: usize = 300; // ~30s at 10 Hz

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

    // systems from Lua
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
    thrtl_est: Option<bool>, // true if Lua synthesized throttle from RPM
    #[serde(default)]
    noz: Option<Pair>,
    #[serde(default)]
    noz_present: Option<bool>, // present flag from Lua
    #[serde(default)]
    temp: Option<Pair>,
    #[serde(default)]
    fuelf: Option<Pair>,
    #[serde(default)]
    map: Option<Pair>,
    #[serde(default)]
    map_present: Option<bool>, // present flag from Lua
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
    wow_guess: Option<bool>, // true if Lua guessed WoW from AGL/TAS/VV
}

#[derive(Debug, Clone, Default)]
struct UiState {
    last: Telemetry,
    ias_hist: VecDeque<f64>,
    alt_hist: VecDeque<f64>,
}

// ---------------- Runtime wiring ----------------

#[tokio::main]
async fn main() -> Result<()> {
    let (tx, rx) = watch::channel(UiState::default());
    let port = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5010);
    task::spawn(udp_listener(format!("127.0.0.1:{port}"), tx));
    run_tui(rx).await
}

async fn udp_listener(bind: String, tx: watch::Sender<UiState>) {
    let sock = match UdpSocket::bind(&bind).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Bind failed on {bind}: {e}");
            return;
        }
    };
    let mut buf = vec![0u8; BUF];
    let mut state = UiState::default();

    loop {
        match sock.recv_from(&mut buf).await {
            Ok((n, _peer)) => {
                for line in std::str::from_utf8(&buf[..n]).unwrap_or("").split('\n') {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(t) = serde_json::from_str::<Telemetry>(line) {
                        let ias_ms = t.ias_ms.unwrap_or(0.0);
                        let alt = t.alt_msl.unwrap_or(0.0);
                        push_hist(&mut state.ias_hist, ias_ms, HISTORY);
                        push_hist(&mut state.alt_hist, alt, HISTORY);
                        state.last = t;
                        let _ = tx.send(state.clone());
                    }
                }
            }
            Err(e) => {
                eprintln!("recv error: {e}");
                sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

fn push_hist(q: &mut VecDeque<f64>, v: f64, cap: usize) {
    q.push_back(v);
    while q.len() > cap {
        q.pop_front();
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
                    (KeyCode::Char('c'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('q'), KeyModifiers::NONE)
                    | (KeyCode::Esc, _) => break 'ui,
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

fn last_n_scaled(src: &VecDeque<f64>, n: usize, scale: f64) -> Vec<u64> {
    if n == 0 {
        return Vec::new();
    }
    let len = src.len();
    let start = len.saturating_sub(n);
    src.iter()
        .skip(start)
        .take(n)
        .map(|v| (*v * scale).max(0.0) as u64)
        .collect()
}

fn last_n(src: &VecDeque<f64>, n: usize) -> Vec<u64> {
    if n == 0 {
        return Vec::new();
    }
    let len = src.len();
    let start = len.saturating_sub(n);
    src.iter()
        .skip(start)
        .take(n)
        .map(|v| v.max(0.0) as u64)
        .collect()
}

fn draw(f: &mut Frame, s: &UiState) {
    // Header, stats row (3 columns), then two full-width charts stacked
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // header
            Constraint::Length(12), // stats row: Flight | Att/Accel | Systems
            Constraint::Min(6),     // IAS chart
            Constraint::Min(6),     // Alt chart
        ])
        .split(f.area());
    // Header
    f.render_widget(header_line(&s.last), layout[0]);

    // Stats row: Flight | Att/Accel | Systems
    let stats_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(33),
            Constraint::Percentage(33),
            Constraint::Percentage(34),
        ])
        .split(layout[1]);

    let info_left = Paragraph::new(format_info_left(&s.last))
        .block(Block::default().borders(Borders::ALL).title("Flight"))
        .wrap(Wrap { trim: true });
    let info_right = Paragraph::new(format_info_right(&s.last))
        .block(Block::default().borders(Borders::ALL).title("Att/Accel"))
        .wrap(Wrap { trim: true });
    let systems = Paragraph::new(format_systems(&s.last))
        .block(Block::default().borders(Borders::ALL).title("Systems"))
        .wrap(Wrap { trim: true });

    f.render_widget(info_left, stats_row[0]);
    f.render_widget(info_right, stats_row[1]);
    f.render_widget(systems, stats_row[2]);

    // Chart 1: IAS (kt) — compute inner width so we pass exactly that many points
    let ias_rect = layout[2];
    let ias_inner_width = ias_rect.width.saturating_sub(2) as usize; // borders = 2
    let ias_kn_hist = last_n_scaled(&s.ias_hist, ias_inner_width, 1.943_844);

    let ias_widget = Sparkline::default()
        .block(Block::default().borders(Borders::ALL).title("IAS (kt)"))
        .data(&ias_kn_hist);
    f.render_widget(ias_widget, ias_rect);

    // Chart 2: Altitude MSL (m)
    let alt_rect = layout[3];
    let alt_inner_width = alt_rect.width.saturating_sub(2) as usize;
    let alt_hist = last_n(&s.alt_hist, alt_inner_width);

    let alt_widget = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Altitude MSL (m)"),
        )
        .data(&alt_hist);
    f.render_widget(alt_widget, alt_rect);
}

// ---------------- Formatting helpers ----------------

fn header_line(t: &Telemetry) -> Paragraph<'static> {
    let name = t.name.as_deref().unwrap_or("?");
    let lat = t.lat.map(|v| format!("{v:.5}")).unwrap_or("-".into());
    let lon = t.lon.map(|v| format!("{v:.5}")).unwrap_or("-".into());
    let txt =
        format!(" DCS Dash — Airframe: {name}   POS: {lat}, {lon}   Ctrl+C / q / Esc to exit ");
    Paragraph::new(txt).block(Block::default().borders(Borders::ALL).title("Status"))
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

    // ----- Engine block (show what we have, hide unknown engine-only signals) -----
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

    // ----- Mech block (ALWAYS visible) -----
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
        // in case mech block is entirely missing
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
