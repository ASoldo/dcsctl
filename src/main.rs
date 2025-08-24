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

#[derive(Debug, Clone, Deserialize, Default)]
struct Telemetry {
    // t: Option<f64>,
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

#[derive(Debug, Clone, Default)]
struct UiState {
    last: Telemetry,
    ias_hist: VecDeque<f64>,
    alt_hist: VecDeque<f64>,
}

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

fn draw(f: &mut Frame, s: &UiState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Percentage(50),
            Constraint::Percentage(50),
        ])
        .split(f.area());

    // Header
    let h = header_line(&s.last);
    f.render_widget(h, chunks[0]);

    // Upper row: big numbers
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);

    let info_left = Paragraph::new(format_info_left(&s.last))
        .block(Block::default().borders(Borders::ALL).title("Flight"))
        .wrap(Wrap { trim: true });
    let info_right = Paragraph::new(format_info_right(&s.last))
        .block(Block::default().borders(Borders::ALL).title("Att/Accel"))
        .wrap(Wrap { trim: true });

    f.render_widget(info_left, row[0]);
    f.render_widget(info_right, row[1]);

    // Lower row: sparklines
    let row2 = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[2]);

    // IAS sparkline in knots -> u64
    let ias_kn_hist: Vec<u64> = s
        .ias_hist
        .iter()
        .map(|ms| (ms * 1.943_844).max(0.0) as u64)
        .collect();
    let ias_widget = Sparkline::default()
        .block(Block::default().borders(Borders::ALL).title("IAS (kt)"))
        .data(&ias_kn_hist);

    // Altitude sparkline (meters) -> u64
    let alt_hist: Vec<u64> = s.alt_hist.iter().map(|m| m.max(0.0) as u64).collect();
    let alt_widget = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Altitude MSL (m)"),
        )
        .data(&alt_hist);

    f.render_widget(ias_widget, row2[0]);
    f.render_widget(alt_widget, row2[1]);
}

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
