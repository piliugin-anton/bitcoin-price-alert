use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::{Color, Print, SetForegroundColor, ResetColor, Attribute, SetAttribute},
    terminal::{self, ClearType},
    SynchronizedUpdate,
};
use futures_util::StreamExt;
use rodio::{nz, DeviceSinkBuilder, Player, Source};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{stdout, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;

// ── Binance WebSocket trade message ──────────────────────────────────────────

#[derive(Deserialize)]
struct BinanceTrade {
    #[serde(rename = "p")]
    price: String,
}

// ── Alert configuration ─────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum AlertDirection {
    Above,
    Below,
}

impl std::fmt::Display for AlertDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AlertDirection::Above => write!(f, "ABOVE"),
            AlertDirection::Below => write!(f, "BELOW"),
        }
    }
}

const ALERT_DEBOUNCE_SECS: u64 = 5;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedAlert {
    price: f64,
    direction: AlertDirection,
}

#[derive(Clone, Debug)]
struct Alert {
    price: f64,
    direction: AlertDirection,
    last_triggered: Option<Instant>,
}

// ── Sound generation ────────────────────────────────────────────────────────

fn generate_sine_wave(frequency: f32, duration_ms: u64, volume: f32) -> Vec<f32> {
    let sample_rate = 44100u32;
    let num_samples = (sample_rate as u64 * duration_ms / 1000) as usize;
    let mut samples = Vec::with_capacity(num_samples);

    for i in 0..num_samples {
        let t = i as f32 / sample_rate as f32;
        let envelope = if i < num_samples / 10 {
            i as f32 / (num_samples as f32 / 10.0)
        } else {
            1.0 - ((i - num_samples / 10) as f32 / (num_samples as f32 * 0.9))
        };
        let sample = (t * frequency * 2.0 * std::f32::consts::PI).sin() * volume * envelope;
        samples.push(sample);
    }
    samples
}

fn play_alert_sound(player: &Player, direction: &AlertDirection) {
    // C5 → E5 → G5 triad; Above = ascending, Below = descending
    let notes = [523.25f32, 659.25, 783.99];
    let notes_ordered: Vec<f32> = match direction {
        AlertDirection::Above => notes.to_vec(),
        AlertDirection::Below => notes.iter().copied().rev().collect(),
    };
    for _repeat in 0..2 {
        for &freq in &notes_ordered {
            let samples = generate_sine_wave(freq, 150, 0.4);
            let source = rodio::buffer::SamplesBuffer::new(nz!(1), nz!(44100), samples);
            player.append(source);
            player.append(rodio::source::Zero::new(nz!(1), nz!(44100)).take_duration(
                Duration::from_millis(30),
            ));
        }
        // Pause between repeats
        player.append(
            rodio::source::Zero::new(nz!(1), nz!(44100))
                .take_duration(Duration::from_millis(200)),
        );
    }
}

fn generate_sine_wave_fade_out(frequency: f32, duration_ms: u64, volume: f32, fade_out_ms: u64) -> Vec<f32> {
    let sample_rate = 44100u32;
    let num_samples = (sample_rate as u64 * duration_ms / 1000) as usize;
    let fade_samples = (sample_rate as u64 * fade_out_ms / 1000) as usize;
    let mut samples = Vec::with_capacity(num_samples);

    for i in 0..num_samples {
        let t = i as f32 / sample_rate as f32;
        let envelope = if i + fade_samples >= num_samples {
            (num_samples - i) as f32 / fade_samples as f32
        } else {
            1.0
        };
        let sample = (t * frequency * 2.0 * std::f32::consts::PI).sin() * volume * envelope;
        samples.push(sample);
    }
    samples
}

fn play_disconnect_sound(player: &Player) {
    // C5 (523.25 Hz) for 200ms, then G4 (392.00 Hz) for 300ms with quick fade-out (transposed 2 octaves up from C3/G2)
    const C5: f32 = 523.25;
    const G4: f32 = 392.00;

    let c5_samples = generate_sine_wave(C5, 200, 0.5);
    player.append(rodio::buffer::SamplesBuffer::new(nz!(1), nz!(44100), c5_samples));

    let g4_samples = generate_sine_wave_fade_out(G4, 300, 0.5, 50);
    player.append(rodio::buffer::SamplesBuffer::new(nz!(1), nz!(44100), g4_samples));
}

// ── Application state ───────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    price: Option<f64>,
    prev_price: Option<f64>,
    alert: Option<Alert>,
    input_mode: InputMode,
    input_buffer: String,
    alert_direction: AlertDirection,
    status: ConnectionStatus,
    alert_flash: bool,
}

#[derive(Clone, PartialEq)]
enum InputMode {
    Normal,
    EnteringPrice,
}

#[derive(Clone, PartialEq)]
enum ConnectionStatus {
    Connecting,
    Connected,
    Disconnected,
}

impl std::fmt::Display for ConnectionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionStatus::Connecting => write!(f, "CONNECTING"),
            ConnectionStatus::Connected => write!(f, "● LIVE"),
            ConnectionStatus::Disconnected => write!(f, "DISCONNECTED"),
        }
    }
}

// ── Alert persistence ───────────────────────────────────────────────────────

fn alerts_file_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "btc-price-alert")
        .map(|d| d.config_dir().join("alerts.json"))
}

fn load_alert() -> Option<Alert> {
    let path = alerts_file_path()?;
    let contents = fs::read_to_string(&path).ok()?;
    let saved: Option<SavedAlert> = serde_json::from_str(&contents).ok()?;
    let saved = saved?;
    if saved.price <= 0.0 {
        return None;
    }
    Some(Alert {
        price: saved.price,
        direction: saved.direction,
        last_triggered: None,
    })
}

fn save_alert(alert: Option<&Alert>) -> std::io::Result<()> {
    let path = alerts_file_path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "config dir not found")
    })?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let saved: Option<SavedAlert> = alert.map(|a| SavedAlert {
        price: a.price,
        direction: a.direction.clone(),
    });
    let json = serde_json::to_string_pretty(&saved)?;
    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, json)?;
    fs::rename(temp_path, path)?;
    Ok(())
}

// ── UI rendering ────────────────────────────────────────────────────────────

fn render(state: &AppState, full_redraw: bool) -> std::io::Result<()> {
    let (cols, rows) = terminal::size()?;
    let w = cols as usize;
    let center_row = rows / 2;

    // Only clear the rows we use; wrap in sync_update so all changes appear at once
    let mut out = stdout();
    out.sync_update(|out| {
        execute!(out, cursor::Hide, cursor::MoveTo(0, 0))?;

        // On resize: clear entire screen with ClearType::All; otherwise only content rows
        if full_redraw {
            execute!(out, terminal::Clear(ClearType::All), cursor::MoveTo(0, 0))?;
        } else {
            let content_rows = [
                1,
                center_row.saturating_sub(5),
                center_row.saturating_sub(3),
                center_row,
                center_row + 3,
                center_row + 5,
                rows.saturating_sub(2),
            ];
            for &row in &content_rows {
                if row > 0 && row <= rows {
                    execute!(out, cursor::MoveTo(0, row), terminal::Clear(ClearType::CurrentLine))?;
                }
            }
        }

        // ── Status line (top right) ─────────────────────────────────────────
    let status_str = format!("{}", state.status);
    let status_col = if w > status_str.len() + 2 { w - status_str.len() - 2 } else { 0 };
    execute!(out, cursor::MoveTo(status_col as u16, 1))?;
    let status_color = match state.status {
        ConnectionStatus::Connected => Color::Green,
        ConnectionStatus::Connecting => Color::Yellow,
        ConnectionStatus::Disconnected => Color::Red,
    };
    execute!(
        out,
        SetForegroundColor(status_color),
        Print(&status_str),
        ResetColor
    )?;

    // ── Header ──────────────────────────────────────────────────────────
    let header = "BINANCE · BTCUSDT · SPOT";
    let hx = w.saturating_sub(header.len()) / 2;
    execute!(
        out,
        cursor::MoveTo(hx as u16, center_row.saturating_sub(5)),
        SetForegroundColor(Color::DarkGrey),
        Print(header),
        ResetColor
    )?;

    // ── Price ───────────────────────────────────────────────────────────
    let price_row = center_row.saturating_sub(3);
    if let Some(price) = state.price {
        let price_str = format!("$ {:.2}", price);
        let px = w.saturating_sub(price_str.len()) / 2;

        let is_up = match state.prev_price {
            Some(prev) => price >= prev,
            None => true,
        };
        let price_color = if is_up { Color::Green } else { Color::Red };

        execute!(
            out,
            cursor::MoveTo(px as u16, price_row),
            SetForegroundColor(price_color),
            SetAttribute(Attribute::Bold),
            Print(&price_str),
            SetAttribute(Attribute::Reset),
            ResetColor
        )?;
    } else {
        let msg = "Waiting for data...";
        let mx = w.saturating_sub(msg.len()) / 2;
        execute!(
            out,
            cursor::MoveTo(mx as u16, price_row),
            SetForegroundColor(Color::DarkGrey),
            Print(msg),
            ResetColor
        )?;
    }

    // ── Alert info ──────────────────────────────────────────────────────
    let alert_row = center_row;
    if let Some(ref alert) = state.alert {
        if alert.last_triggered.is_some() {
            let msg = format!(
                "⚡ ALERT TRIGGERED — Price went {} ${:.2}",
                alert.direction, alert.price
            );
            let ax = w.saturating_sub(msg.len()) / 2;
            let flash_color = if state.alert_flash { Color::Yellow } else { Color::DarkYellow };
            execute!(
                out,
                cursor::MoveTo(ax as u16, alert_row),
                SetForegroundColor(flash_color),
                SetAttribute(Attribute::Bold),
                Print(&msg),
                SetAttribute(Attribute::Reset),
                ResetColor
            )?;
        } else {
            let msg = format!(
                "Alert set: {} ${:.2}",
                alert.direction, alert.price
            );
            let ax = w.saturating_sub(msg.len()) / 2;
            execute!(
                out,
                cursor::MoveTo(ax as u16, alert_row),
                SetForegroundColor(Color::DarkYellow),
                Print(&msg),
                ResetColor
            )?;
        }
    }

    // ── Input / Help ────────────────────────────────────────────────────
    let input_row = center_row + 3;
    match state.input_mode {
        InputMode::EnteringPrice => {
            let dir_str = format!("{}", state.alert_direction);
            let prompt = format!("Alert {} at price: ${}", dir_str, state.input_buffer);
            let ix = w.saturating_sub(prompt.len() + 1) / 2;
            execute!(
                out,
                cursor::MoveTo(ix as u16, input_row),
                SetForegroundColor(Color::Cyan),
                Print(&prompt),
                Print("█"),
                ResetColor
            )?;

            let hint = "[Tab] toggle Above/Below  [Enter] confirm  [Esc] cancel";
            let hx = w.saturating_sub(hint.len()) / 2;
            execute!(
                out,
                cursor::MoveTo(hx as u16, input_row + 2),
                SetForegroundColor(Color::DarkGrey),
                Print(hint),
                ResetColor
            )?;
        }
        InputMode::Normal => {
            let help = if state.alert.is_some() {
                "[A] new alert  [C] clear alert  [Q] quit"
            } else {
                "[A] set alert  [Q] quit"
            };
            let hx = w.saturating_sub(help.len()) / 2;
            execute!(
                out,
                cursor::MoveTo(hx as u16, input_row),
                SetForegroundColor(Color::DarkGrey),
                Print(help),
                ResetColor
            )?;
        }
    }

        // ── Bottom border ───────────────────────────────────────────────────
        let border = "─".repeat(w.min(60));
        let bx = w.saturating_sub(border.len()) / 2;
        execute!(
            out,
            cursor::MoveTo(bx as u16, rows.saturating_sub(2)),
            SetForegroundColor(Color::Rgb { r: 30, g: 30, b: 30 }),
            Print(&border),
            ResetColor
        )?;

        out.flush()?;
        Ok(())
    })
    .and_then(std::convert::identity)
}

/// Redraws only the price row, status row, and optionally the alert row (when alert is flashing).
/// Used when only price changed or when alert flash toggles — avoids full UI redraw.
fn render_partial(state: &AppState) -> std::io::Result<()> {
    let (cols, rows) = terminal::size()?;
    let w = cols as usize;
    let center_row = rows / 2;
    let price_row = center_row.saturating_sub(3);
    let alert_row = center_row;

    let mut out = stdout();
    out.sync_update(|out| {
        execute!(out, cursor::Hide)?;

        // ── Status row (top right) ─────────────────────────────────────────
        let status_str = format!("{}", state.status);
        let status_col = if w > status_str.len() + 2 { w - status_str.len() - 2 } else { 0 };
        execute!(out, cursor::MoveTo(0, 1), terminal::Clear(ClearType::CurrentLine))?;
        execute!(out, cursor::MoveTo(status_col as u16, 1))?;
        let status_color = match state.status {
            ConnectionStatus::Connected => Color::Green,
            ConnectionStatus::Connecting => Color::Yellow,
            ConnectionStatus::Disconnected => Color::Red,
        };
        execute!(
            out,
            SetForegroundColor(status_color),
            Print(&status_str),
            ResetColor
        )?;

        // ── Price row ──────────────────────────────────────────────────────
        execute!(out, cursor::MoveTo(0, price_row as u16), terminal::Clear(ClearType::CurrentLine))?;
        if let Some(price) = state.price {
            let price_str = format!("$ {:.2}", price);
            let px = w.saturating_sub(price_str.len()) / 2;
            let is_up = match state.prev_price {
                Some(prev) => price >= prev,
                None => true,
            };
            let price_color = if is_up { Color::Green } else { Color::Red };
            execute!(
                out,
                cursor::MoveTo(px as u16, price_row as u16),
                SetForegroundColor(price_color),
                SetAttribute(Attribute::Bold),
                Print(&price_str),
                SetAttribute(Attribute::Reset),
                ResetColor
            )?;
        } else {
            let msg = "Waiting for data...";
            let mx = w.saturating_sub(msg.len()) / 2;
            execute!(
                out,
                cursor::MoveTo(mx as u16, price_row as u16),
                SetForegroundColor(Color::DarkGrey),
                Print(msg),
                ResetColor
            )?;
        }

        // ── Alert row (only when triggered and flashing) ───────────────────
        if state.alert.as_ref().and_then(|a| a.last_triggered).is_some() {
            execute!(out, cursor::MoveTo(0, alert_row as u16), terminal::Clear(ClearType::CurrentLine))?;
            if let Some(ref alert) = state.alert {
                let msg = format!(
                    "⚡ ALERT TRIGGERED — Price went {} ${:.2}",
                    alert.direction, alert.price
                );
                let ax = w.saturating_sub(msg.len()) / 2;
                let flash_color = if state.alert_flash { Color::Yellow } else { Color::DarkYellow };
                execute!(
                    out,
                    cursor::MoveTo(ax as u16, alert_row as u16),
                    SetForegroundColor(flash_color),
                    SetAttribute(Attribute::Bold),
                    Print(&msg),
                    SetAttribute(Attribute::Reset),
                    ResetColor
                )?;
            }
        }

        out.flush()?;
        Ok(())
    })
    .and_then(std::convert::identity)
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Set up terminal
    terminal::enable_raw_mode()?;
    let mut out = stdout();
    execute!(
        out,
        terminal::EnterAlternateScreen,
        cursor::Hide,
        terminal::Clear(ClearType::All)
    )?;

    let initial_alert = load_alert();
    let state = Arc::new(Mutex::new(AppState {
        price: None,
        prev_price: None,
        alert: initial_alert,
        input_mode: InputMode::Normal,
        input_buffer: String::new(),
        alert_direction: AlertDirection::Above,
        status: ConnectionStatus::Connecting,
        alert_flash: false,
    }));

    // Single long-lived audio output — avoids repeated opening/closing of device on Linux
    let mut stream_handle = DeviceSinkBuilder::open_default_sink().expect("Cannot open audio device");
    stream_handle.log_on_drop(false);
    let player = Player::connect_new(stream_handle.mixer());

    let (tx, mut rx) = mpsc::channel::<f64>(256);

    // ── WebSocket task ──────────────────────────────────────────────────
    let ws_state = state.clone();
    tokio::spawn(async move {
        loop {
            {
                let mut s = ws_state.lock().unwrap();
                s.status = ConnectionStatus::Connecting;
            }

            let url = "wss://stream.binance.com:9443/ws/btcusdt@trade";
            match connect_async(url).await {
                Ok((ws_stream, _)) => {
                    {
                        let mut s = ws_state.lock().unwrap();
                        s.status = ConnectionStatus::Connected;
                    }

                    let (_, mut read) = ws_stream.split();

                    const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
                    loop {
                        match tokio::time::timeout(IDLE_TIMEOUT, read.next()).await {
                            Ok(Some(msg)) => match msg {
                                Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                                    if let Ok(trade) = serde_json::from_str::<BinanceTrade>(&text) {
                                        if let Ok(price) = trade.price.parse::<f64>() {
                                            let _ = tx.send(price).await;
                                        }
                                    }
                                }
                                Err(_) => break,
                                _ => {}
                            },
                            Ok(None) => break,
                            Err(_) => break, // no message in 30s — reconnect
                        }
                    }

                    {
                        let mut s = ws_state.lock().unwrap();
                        s.status = ConnectionStatus::Disconnected;
                    }
                }
                Err(_) => {
                    let mut s = ws_state.lock().unwrap();
                    s.status = ConnectionStatus::Disconnected;
                    // No disconnect sound — we never had a connection
                }
            }

            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });

    // ── Main event loop ─────────────────────────────────────────────────
    let mut flash_counter: u32 = 0;
    let mut need_full_redraw = true; // initial full render after Clear
    let mut last_rendered_status: Option<ConnectionStatus> = None;
    const DISCONNECT_SOUND_INTERVAL: u64 = 5;
    let mut last_disconnect_sound: Option<Instant> = None;

    loop {
        let mut got_new_price = false;

        // Play disconnect sound every 5s while disconnected
        {
            let s = state.lock().unwrap();
            if s.status == ConnectionStatus::Disconnected {
                let should_play = last_disconnect_sound.map_or(true, |t| {
                    t.elapsed() >= Duration::from_secs(DISCONNECT_SOUND_INTERVAL)
                });
                if should_play {
                    drop(s);
                    last_disconnect_sound = Some(Instant::now());
                    play_disconnect_sound(&player);
                }
            } else if s.status == ConnectionStatus::Connected {
                last_disconnect_sound = None;
            }
        }

        // Process incoming prices
        while let Ok(price) = rx.try_recv() {
            got_new_price = true;
            let mut s = state.lock().unwrap();
            s.prev_price = s.price;
            s.price = Some(price);

            // Check alert (with 5s debounce — can trigger again after cooldown)
            if let Some(ref mut alert) = s.alert {
                let can_trigger = alert.last_triggered.map_or(true, |t| {
                    t.elapsed() >= Duration::from_secs(ALERT_DEBOUNCE_SECS)
                });
                if can_trigger {
                    let triggered = match alert.direction {
                        AlertDirection::Above => price >= alert.price,
                        AlertDirection::Below => price <= alert.price,
                    };
                    if triggered {
                        alert.last_triggered = Some(Instant::now());
                        play_alert_sound(&player, &alert.direction);
                    }
                }
            }
        }

        // Flash effect for triggered alerts
        flash_counter = flash_counter.wrapping_add(1);
        {
            let mut s = state.lock().unwrap();
            s.alert_flash = flash_counter % 6 < 3;
        }

        // Handle input (keyboard, resize)
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Resize(_, _) => need_full_redraw = true,
                Event::Key(KeyEvent { code, modifiers, .. }) => {
                    need_full_redraw = true;
                let mut s = state.lock().unwrap();

                match s.input_mode {
                    InputMode::Normal => match code {
                        KeyCode::Char('q') | KeyCode::Char('Q') => break,
                        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => break,
                        KeyCode::Char('a') | KeyCode::Char('A') => {
                            s.input_mode = InputMode::EnteringPrice;
                            s.input_buffer.clear();
                            s.alert_direction = AlertDirection::Above;
                        }
                        KeyCode::Char('c') | KeyCode::Char('C') => {
                            s.alert = None;
                            let _ = save_alert(None);
                        }
                        _ => {}
                    },
                    InputMode::EnteringPrice => match code {
                        KeyCode::Esc => {
                            s.input_mode = InputMode::Normal;
                            s.input_buffer.clear();
                        }
                        KeyCode::Tab => {
                            s.alert_direction = match s.alert_direction {
                                AlertDirection::Above => AlertDirection::Below,
                                AlertDirection::Below => AlertDirection::Above,
                            };
                        }
                        KeyCode::Enter => {
                            if let Ok(price) = s.input_buffer.parse::<f64>() {
                                if price > 0.0 {
                                    let alert = Alert {
                                        price,
                                        direction: s.alert_direction.clone(),
                                        last_triggered: None,
                                    };
                                    let _ = save_alert(Some(&alert));
                                    s.alert = Some(alert);
                                }
                            }
                            s.input_mode = InputMode::Normal;
                            s.input_buffer.clear();
                        }
                        KeyCode::Backspace => {
                            s.input_buffer.pop();
                        }
                        KeyCode::Char(c) if c.is_ascii_digit() || c == '.' => {
                            s.input_buffer.push(c);
                        }
                        _ => {}
                    },
                }
            }
            _ => {}
            }
        }

        // Render: full redraw on resize/key; otherwise price, status, alert when changed
        let s = state.lock().unwrap();
        let status_changed = last_rendered_status.as_ref() != Some(&s.status);
        if status_changed {
            last_rendered_status = Some(s.status.clone());
        }
        if need_full_redraw {
            render(&s, true)?;
            need_full_redraw = false;
            last_rendered_status = Some(s.status.clone());
        } else if got_new_price || s.alert.as_ref().and_then(|a| a.last_triggered).is_some() || status_changed {
            render_partial(&s)?;
        }
    }

    // Cleanup terminal
    terminal::disable_raw_mode()?;
    execute!(
        stdout(),
        terminal::LeaveAlternateScreen,
        cursor::Show
    )?;

    Ok(())
}
