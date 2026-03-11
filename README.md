# BTC Price Alert

Terminal app that shows live Bitcoin price from Binance and plays a sound when price crosses your alert threshold.

## Requirements

- **Rust** (1.70+)
- **Linux:** `libasound2-dev` and `pkg-config` for audio:
  ```bash
  sudo apt install libasound2-dev pkg-config
  ```
- **Windows:** no additional dependencies (audio via WASAPI)

## Usage

```bash
cargo run
```

- **A** — set alert (enter price, Tab toggles Above/Below, Enter confirms, Esc cancels)
- **C** — clear current alert
- **Q** — quit
