//! Real-time EEG chart viewer for MW75 Neuro headphones.
//!
//! Usage:
//!   cargo run --bin mw75-tui                # scan + connect to hardware
//!   cargo run --bin mw75-tui -- --simulate  # synthetic data (no hardware)
//!
//! Keys
//! ────
//!   +  / =   zoom out  (increase µV scale)
//!   -        zoom in   (decrease µV scale)
//!   a        auto-scale: fit Y axis to current peak amplitude
//!   v        toggle smooth overlay (dim raw + bright moving-average)
//!   p        pause streaming
//!   r        resume streaming
//!   c        clear waveform buffers
//!   q / Esc  quit

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph},
    Frame, Terminal,
};

use mw75::mw75_client::{Mw75Client, Mw75ClientConfig};
use mw75::protocol::{EEG_CHANNEL_NAMES, NUM_EEG_CHANNELS};
use mw75::simulate::spawn_simulator;
use mw75::types::Mw75Event;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Width of the scrolling waveform window in seconds.
const WINDOW_SECS: f64 = 2.0;

/// EEG sample rate (Hz). MW75 streams at 500 Hz.
const EEG_HZ: f64 = 500.0;

/// Number of samples retained per channel — enough to fill `WINDOW_SECS`.
const BUF_SIZE: usize = (WINDOW_SECS * EEG_HZ) as usize; // 1000

/// Number of EEG channels to display (show first 4 in stacked charts,
/// remaining 8 in a combined bottom chart).
const TOP_CHANNELS: usize = 4;

/// Y-axis scale steps in µV (half the symmetric range ±scale).
const Y_SCALES: &[f64] = &[10.0, 25.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0];

/// Default scale index. ±200 µV covers typical simulation peaks.
const DEFAULT_SCALE: usize = 4;

/// Per-channel line colours for the first 4 channels.
const COLORS: [Color; 4] = [Color::Cyan, Color::Yellow, Color::Green, Color::Magenta];

/// Dimmed versions for smooth mode background trace.
const DIM_COLORS: [Color; 4] = [
    Color::Rgb(0, 90, 110),
    Color::Rgb(110, 90, 0),
    Color::Rgb(0, 110, 0),
    Color::Rgb(110, 0, 110),
];

/// Moving-average window in samples. 9 samples ≈ 18 ms at 500 Hz.
const SMOOTH_WINDOW: usize = 9;

/// Braille spinner frames.
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// ── App mode ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
enum AppMode {
    Scanning,
    Connected { name: String },
    Simulated,
    Disconnected,
}

// ── App state ─────────────────────────────────────────────────────────────────

struct App {
    bufs: Vec<VecDeque<f64>>,
    mode: AppMode,
    battery: Option<u8>,
    total_samples: u64,
    pkt_times: VecDeque<Instant>,
    scale_idx: usize,
    paused: bool,
    smooth: bool,
    dropped_packets: u64,
    last_counter: Option<u8>,
}

impl App {
    fn new() -> Self {
        Self {
            bufs: (0..NUM_EEG_CHANNELS)
                .map(|_| VecDeque::with_capacity(BUF_SIZE + 16))
                .collect(),
            mode: AppMode::Scanning,
            battery: None,
            total_samples: 0,
            pkt_times: VecDeque::with_capacity(512),
            scale_idx: DEFAULT_SCALE,
            paused: false,
            smooth: true,
            dropped_packets: 0,
            last_counter: None,
        }
    }

    fn push(&mut self, channels: &[f32]) {
        if self.paused {
            return;
        }
        for (ch, &v) in channels.iter().enumerate().take(NUM_EEG_CHANNELS) {
            let buf = &mut self.bufs[ch];
            buf.push_back(v as f64);
            while buf.len() > BUF_SIZE {
                buf.pop_front();
            }
        }
        self.total_samples += 1;
        let now = Instant::now();
        self.pkt_times.push_back(now);
        while self
            .pkt_times
            .front()
            .map(|t| now.duration_since(*t) > Duration::from_secs(2))
            .unwrap_or(false)
        {
            self.pkt_times.pop_front();
        }
    }

    fn clear(&mut self) {
        for b in &mut self.bufs {
            b.clear();
        }
        self.total_samples = 0;
        self.pkt_times.clear();
        self.battery = None;
        self.dropped_packets = 0;
        self.last_counter = None;
    }

    fn pkt_rate(&self) -> f64 {
        let n = self.pkt_times.len();
        if n < 2 {
            return 0.0;
        }
        let span = self
            .pkt_times
            .back()
            .unwrap()
            .duration_since(self.pkt_times[0])
            .as_secs_f64();
        if span < 1e-9 { 0.0 } else { (n as f64 - 1.0) / span }
    }

    fn y_range(&self) -> f64 {
        Y_SCALES[self.scale_idx]
    }

    fn scale_up(&mut self) {
        if self.scale_idx + 1 < Y_SCALES.len() {
            self.scale_idx += 1;
        }
    }

    fn scale_down(&mut self) {
        if self.scale_idx > 0 {
            self.scale_idx -= 1;
        }
    }

    fn auto_scale(&mut self) {
        let peak = self
            .bufs
            .iter()
            .flat_map(|b| b.iter())
            .fold(0.0_f64, |acc, &v| acc.max(v.abs()));
        let needed = peak * 1.1;
        self.scale_idx = Y_SCALES
            .iter()
            .position(|&s| s >= needed)
            .unwrap_or(Y_SCALES.len() - 1);
    }

    fn track_counter(&mut self, counter: u8) {
        if let Some(last) = self.last_counter {
            let expected = last.wrapping_add(1);
            if counter != expected {
                let dropped = counter.wrapping_sub(expected) as u64;
                self.dropped_packets += dropped;
            }
        }
        self.last_counter = Some(counter);
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn spinner_str() -> &'static str {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    SPINNER[(ms / 100) as usize % SPINNER.len()]
}

fn smooth_signal(data: &[(f64, f64)], window: usize) -> Vec<(f64, f64)> {
    if data.len() < 3 || window < 2 {
        return data.to_vec();
    }
    let half = window / 2;
    data.iter()
        .enumerate()
        .map(|(i, &(x, _))| {
            let start = i.saturating_sub(half);
            let end = (i + half + 1).min(data.len());
            let sum: f64 = data[start..end].iter().map(|&(_, y)| y).sum();
            (x, sum / (end - start) as f64)
        })
        .collect()
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let root = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(3),
    ])
    .split(area);

    draw_header(frame, root[0], app);
    draw_charts(frame, root[1], app);
    draw_footer(frame, root[2], app);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let (label, color) = match &app.mode {
        AppMode::Scanning => (format!("{} Scanning…", spinner_str()), Color::Yellow),
        AppMode::Connected { name } => (format!("● {name}"), Color::Green),
        AppMode::Simulated => ("◆ Simulated (500 Hz)".to_owned(), Color::Cyan),
        AppMode::Disconnected => (
            format!("{} Disconnected", spinner_str()),
            Color::Red,
        ),
    };

    let bat = app
        .battery
        .map(|b| format!("Bat {b}%"))
        .unwrap_or_else(|| "Bat N/A".into());

    let rate = format!("{:.0} Hz", app.pkt_rate());
    let scale = format!("±{:.0} µV", app.y_range());
    let total = format!("{}K smp", app.total_samples / 1_000);
    let dropped = format!("{} drop", app.dropped_packets);

    let line = Line::from(vec![
        Span::styled(
            " MW75 EEG Monitor ",
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        sep(),
        Span::styled(label, Style::default().fg(color).add_modifier(Modifier::BOLD)),
        sep(),
        Span::styled(bat, Style::default().fg(Color::White)),
        sep(),
        Span::styled(rate, Style::default().fg(Color::White)),
        sep(),
        Span::styled(
            scale,
            Style::default().fg(Color::LightBlue).add_modifier(Modifier::BOLD),
        ),
        sep(),
        Span::styled(total, Style::default().fg(Color::DarkGray)),
        sep(),
        Span::styled(dropped, Style::default().fg(if app.dropped_packets > 0 { Color::Red } else { Color::DarkGray })),
        Span::raw(" "),
    ]);

    frame.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

#[inline]
fn sep<'a>() -> Span<'a> {
    Span::styled(" │ ", Style::default().fg(Color::DarkGray))
}

fn draw_charts(frame: &mut Frame, area: Rect, app: &App) {
    // Show first 4 channels as individual stacked charts
    let constraints: Vec<Constraint> = (0..TOP_CHANNELS)
        .map(|_| Constraint::Ratio(1, TOP_CHANNELS as u32))
        .collect();
    let rows = Layout::vertical(constraints).split(area);

    let y_range = app.y_range();

    for ch in 0..TOP_CHANNELS {
        let data: Vec<(f64, f64)> = app.bufs[ch]
            .iter()
            .enumerate()
            .map(|(i, &v)| (i as f64 / EEG_HZ, v.clamp(-y_range, y_range)))
            .collect();

        draw_channel(frame, rows[ch], ch, &data, app);
    }
}

fn draw_channel(frame: &mut Frame, area: Rect, ch: usize, data: &[(f64, f64)], app: &App) {
    let color = COLORS[ch % COLORS.len()];
    let y_range = app.y_range();
    let name = EEG_CHANNEL_NAMES.get(ch).copied().unwrap_or("?");

    // Stats from raw buffer
    let (min_v, max_v, rms_v) = {
        let buf = &app.bufs[ch];
        if buf.is_empty() {
            (0.0_f64, 0.0_f64, 0.0_f64)
        } else {
            let min = buf.iter().copied().fold(f64::INFINITY, f64::min);
            let max = buf.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let rms = (buf.iter().map(|&v| v * v).sum::<f64>() / buf.len() as f64).sqrt();
            (min, max, rms)
        }
    };

    let clipping = max_v > y_range || min_v < -y_range;
    let border_color = if clipping { Color::Red } else { color };

    let clip_tag = if clipping { " [CLIP]" } else { "" };
    let smooth_tag = if app.smooth { " [SMOOTH]" } else { "" };
    let title = format!(
        " {name}  min:{min_v:+6.1}  max:{max_v:+6.1}  rms:{rms_v:5.1} µV{clip_tag}{smooth_tag} "
    );

    let smoothed: Vec<(f64, f64)> = if app.smooth {
        smooth_signal(data, SMOOTH_WINDOW)
    } else {
        vec![]
    };

    let datasets: Vec<Dataset> = if app.smooth {
        vec![
            Dataset::default()
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(DIM_COLORS[ch % DIM_COLORS.len()]))
                .data(data),
            Dataset::default()
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(color))
                .data(&smoothed),
        ]
    } else {
        vec![Dataset::default()
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(color))
            .data(data)]
    };

    let y_labels: Vec<String> = [-1.0, -0.5, 0.0, 0.5, 1.0]
        .iter()
        .map(|&f| format!("{:+.0}", f * y_range))
        .collect();
    let x_labels = vec![
        "0s".to_string(),
        format!("{:.1}s", WINDOW_SECS / 2.0),
        format!("{:.0}s", WINDOW_SECS),
    ];

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .title(Span::styled(
                    title,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        )
        .x_axis(
            Axis::default()
                .bounds([0.0, WINDOW_SECS])
                .labels(x_labels)
                .style(Style::default().fg(Color::DarkGray)),
        )
        .y_axis(
            Axis::default()
                .bounds([-y_range, y_range])
                .labels(y_labels)
                .style(Style::default().fg(Color::DarkGray)),
        );

    frame.render_widget(chart, area);
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let pause_span = if app.paused {
        Span::styled(
            "  ⏸ PAUSED",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("")
    };

    let keys = Line::from(vec![
        Span::raw(" "),
        key("[+/-]"),
        Span::raw("Scale  "),
        key("[a]"),
        Span::raw("Auto  "),
        key("[v]"),
        Span::raw(if app.smooth { "Raw  " } else { "Smooth  " }),
        key("[p]"),
        Span::raw("Pause  "),
        key("[r]"),
        Span::raw("Resume  "),
        key("[c]"),
        Span::raw("Clear  "),
        key("[q]"),
        Span::raw("Quit"),
        pause_span,
    ]);

    let ch_info = if app.bufs.len() > 4 {
        let ch5_val = app.bufs[4].back().copied().unwrap_or(0.0);
        let ch6_val = app.bufs[5].back().copied().unwrap_or(0.0);
        format!(
            "Ch5={ch5_val:+.1}  Ch6={ch6_val:+.1}  … (12 ch total)"
        )
    } else {
        String::new()
    };

    let info_line = Line::from(vec![
        Span::raw(" "),
        Span::styled("Extra channels: ", Style::default().fg(Color::DarkGray)),
        Span::styled(ch_info, Style::default().fg(Color::Cyan)),
    ]);

    frame.render_widget(
        Paragraph::new(vec![keys, info_line]).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

#[inline]
fn key(s: &str) -> Span<'_> {
    Span::styled(
        s,
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    use std::io::IsTerminal as _;
    if !io::stdout().is_terminal() {
        eprintln!("Error: mw75-tui requires a real terminal (TTY).");
        std::process::exit(1);
    }

    // ── Logging to file ──────────────────────────────────────────────────────
    {
        use std::fs::File;
        if let Ok(file) = File::create("mw75-tui.log") {
            env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
                .target(env_logger::Target::Pipe(Box::new(file)))
                .init();
        }
    }

    let simulate = std::env::args().any(|a| a == "--simulate");
    let app = Arc::new(Mutex::new(App::new()));

    // ── Start data source ────────────────────────────────────────────────────
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Mw75Event>(512);

    if simulate {
        let mut s = app.lock().unwrap();
        s.mode = AppMode::Simulated;
        s.scale_idx = 4; // ±200 µV for simulated peaks
        drop(s);
        let _sim_handle = spawn_simulator(tx, true);
    } else {
        let app_clone = Arc::clone(&app);
        tokio::spawn(async move {
            let config = Mw75ClientConfig::default();
            let client = Mw75Client::new(config);

            match client.connect().await {
                Ok((mut device_rx, handle)) => {
                    if let Err(e) = handle.start().await {
                        log::error!("Activation failed: {e}");
                        return;
                    }

                    // Start RFCOMM data stream after BLE activation
                    #[cfg(feature = "rfcomm")]
                    {
                        let handle = Arc::new(handle);
                        let bt_address = handle.peripheral_id();
                        log::info!("Starting RFCOMM stream to {bt_address}…");

                        // Disconnect BLE first (required on macOS)
                        handle.disconnect_ble().await.ok();

                        match mw75::rfcomm::start_rfcomm_stream(
                            handle.clone(),
                            &bt_address,
                        )
                        .await
                        {
                            Ok(_task) => {
                                log::info!("RFCOMM reader task started");
                            }
                            Err(e) => {
                                log::error!("RFCOMM connect failed: {e}");
                            }
                        }
                    }

                    // Forward BLE events to the TUI channel
                    while let Some(event) = device_rx.recv().await {
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    log::error!("Connection failed: {e}");
                    let mut s = app_clone.lock().unwrap();
                    s.mode = AppMode::Disconnected;
                }
            }
        });
    }

    // ── Event dispatch task ──────────────────────────────────────────────────
    let app_events = Arc::clone(&app);
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let mut s = app_events.lock().unwrap();
            match event {
                Mw75Event::Connected(name) => {
                    s.mode = AppMode::Connected { name };
                }
                Mw75Event::Disconnected => {
                    s.mode = AppMode::Disconnected;
                }
                Mw75Event::Battery(b) => {
                    s.battery = Some(b.level);
                }
                Mw75Event::Eeg(pkt) => {
                    s.track_counter(pkt.counter);
                    s.push(&pkt.channels);
                }
                _ => {}
            }
        }
    });

    // ── Terminal setup ───────────────────────────────────────────────────────
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let tick = Duration::from_millis(33); // ~30 FPS

    // ── Main loop ────────────────────────────────────────────────────────────
    loop {
        // Render
        {
            let s = app.lock().unwrap();
            terminal.draw(|f| draw(f, &s))?;
        }

        // Handle input
        if !event::poll(tick)? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };

        let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('c');
        if key.code == KeyCode::Char('q') || key.code == KeyCode::Esc || ctrl_c {
            break;
        }

        match key.code {
            KeyCode::Char('+') | KeyCode::Char('=') => app.lock().unwrap().scale_up(),
            KeyCode::Char('-') => app.lock().unwrap().scale_down(),
            KeyCode::Char('a') => app.lock().unwrap().auto_scale(),
            KeyCode::Char('v') => {
                let mut s = app.lock().unwrap();
                s.smooth = !s.smooth;
            }
            KeyCode::Char('p') => app.lock().unwrap().paused = true,
            KeyCode::Char('r') => app.lock().unwrap().paused = false,
            KeyCode::Char('c') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.lock().unwrap().clear();
            }
            _ => {}
        }
    }

    // ── Teardown ─────────────────────────────────────────────────────────────
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
