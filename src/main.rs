use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::{Color, Print, SetForegroundColor, ResetColor, Attribute, SetAttribute},
    terminal::{self, ClearType},
};
use futures_util::StreamExt;
use rodio::{nz, DeviceSinkBuilder, Player, Source};
use serde::Deserialize;
use std::io::{stdout, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;

// ── Binance WebSocket trade message ──────────────────────────────────────────

#[derive(Deserialize)]
struct BinanceTrade {
    #[serde(rename = "p")]
    price: String,
}

// ── Alert configuration ─────────────────────────────────────────────────────

#[derive(Clone, Debug)]
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

#[derive(Clone, Debug)]
struct Alert {
    price: f64,
    direction: AlertDirection,
    triggered: bool,
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

fn play_alert_sound() {
    std::thread::spawn(|| {
        if let Ok(stream_handle) = DeviceSinkBuilder::open_default_sink() {
            let player = Player::connect_new(stream_handle.mixer());
                // C5 → E5 → G5 ascending triad, played twice
                let notes = [523.25f32, 659.25, 783.99];
                for _repeat in 0..2 {
                    for &freq in &notes {
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
                player.sleep_until_end();
        }
    });
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

// ── UI rendering ────────────────────────────────────────────────────────────

fn render(state: &AppState) -> std::io::Result<()> {
    let mut out = stdout();
    let (cols, rows) = terminal::size()?;
    let w = cols as usize;

    execute!(out, cursor::Hide, cursor::MoveTo(0, 0))?;

    // Clear screen
    for row in 0..rows {
        execute!(out, cursor::MoveTo(0, row), terminal::Clear(ClearType::CurrentLine))?;
    }

    let center_row = rows / 2;

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
        if alert.triggered {
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

    let state = Arc::new(Mutex::new(AppState {
        price: None,
        prev_price: None,
        alert: None,
        input_mode: InputMode::Normal,
        input_buffer: String::new(),
        alert_direction: AlertDirection::Above,
        status: ConnectionStatus::Connecting,
        alert_flash: false,
    }));

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

                    while let Some(msg) = read.next().await {
                        match msg {
                            Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                                if let Ok(trade) = serde_json::from_str::<BinanceTrade>(&text) {
                                    if let Ok(price) = trade.price.parse::<f64>() {
                                        let _ = tx.send(price).await;
                                    }
                                }
                            }
                            Err(_) => break,
                            _ => {}
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
                }
            }

            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });

    // ── Main event loop ─────────────────────────────────────────────────
    let mut flash_counter: u32 = 0;

    loop {
        // Process incoming prices
        while let Ok(price) = rx.try_recv() {
            let mut s = state.lock().unwrap();
            s.prev_price = s.price;
            s.price = Some(price);

            // Check alert
            if let Some(ref mut alert) = s.alert {
                if !alert.triggered {
                    let triggered = match alert.direction {
                        AlertDirection::Above => price >= alert.price,
                        AlertDirection::Below => price <= alert.price,
                    };
                    if triggered {
                        alert.triggered = true;
                        play_alert_sound();
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

        // Handle keyboard input
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(KeyEvent { code, modifiers, .. }) = event::read()? {
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
                                    s.alert = Some(Alert {
                                        price,
                                        direction: s.alert_direction.clone(),
                                        triggered: false,
                                    });
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
        }

        // Render
        let s = state.lock().unwrap();
        render(&s)?;
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
