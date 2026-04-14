<div align="center">

# Frikadellen BAF

### Bazaar & Auction Flipper for Hypixel Skyblock

A high-performance, Rust-powered macro that automates auction house sniping, bazaar trading, and inventory management on Hypixel Skyblock.

[![Discord](https://img.shields.io/badge/Discord-Join%20Server-5865F2?logo=discord&logoColor=white)](https://discord.gg/42DvX6T9jh)
[![Website](https://img.shields.io/badge/Website-auctionflipper.bz-blue)](https://auctionflipper.bz)
[![Releases](https://img.shields.io/github/v/release/TreXito/frikadellen-baf-121?label=Latest%20Release)](../../releases/latest)
[![License](https://img.shields.io/badge/License-AGPLv3-green)](#license)

</div>

---

> **⚠️ Use at your own risk.** This is a macro — using it may result in a ban. If you do get banned, please share your logs in the [Discord server](https://discord.gg/42DvX6T9jh) so we can improve detection evasion.

---

## Table of Contents

- [Features](#features)
- [Installation](#installation)
- [Quick Start — Your First Run](#quick-start--your-first-run)
- [Web Control Panel](#web-control-panel)
- [Discord Webhooks](#discord-webhooks)
- [Multi-Account Support](#multi-account-support)
- [Humanization / Rest Breaks](#humanization--rest-breaks)
- [Proxy Support](#proxy-support)
- [Configuration Reference](#configuration-reference)
- [Troubleshooting](#troubleshooting)
- [Building from Source](#building-from-source)
- [License](#license)

---

## Features

### ⚡ Fast Auction House Buying
Snipes BIN (Buy It Now) auctions in sub-second time. The bot runs on a patched engine at 300 FPS for ~1.65 ms packet detection latency, making purchases before most other players can react. Optional **fastbuy** mode skips the confirmation window entirely for even faster execution.

### 💰 Automatic Selling & Relisting
When your inventory is full the bot enters selling mode — it lists items to the auction house, claims sold auctions, and relist items automatically. An idle failsafe triggers every 30 minutes if no auctions have been listed, forcing a sell cycle so nothing sits idle.

### 📊 Bazaar Trading
Automated bazaar flip macro: places buy/sell orders, monitors fill status, collects filled orders, and cancels stale orders based on their age and value. Partially-filled buy orders are auto-cancelled and items relisted. The bot tracks order history and profit persistently across restarts.

### 🔄 Multi-Account Rotation
Provide a comma-separated list of Minecraft usernames and set a rotation interval. The bot automatically switches between accounts on schedule, authenticating each one independently.

### 🌐 Web Control Panel
A full-featured browser dashboard for monitoring and controlling the bot from any device. Includes real-time stats, profit charts, game chat, inventory view, and a configuration editor — all accessible through a single port. *(See [Web Control Panel](#web-control-panel) below for details.)*

### 📢 Discord Webhooks
Get notified about flips, profits, and events directly in your Discord server. Separate webhook URLs for AH and Bazaar events, with optional user pings on legendary or divine flips.

### 🛡️ Humanization
Optional periodic rest breaks that simulate human behavior — the bot disconnects, waits a random duration, and reconnects. Fully configurable intervals and break lengths.

### 🔒 Proxy Support
Route the bot through a SOCKS5/HTTP proxy for VPS or network-restricted environments.

### 📝 Logging
All activity is logged to `logs/latest.log`. Old logs are archived with timestamps and automatically deleted after 7 days.

### 🖥️ Cross-Platform
Pre-built binaries for **Linux x86_64**, **macOS Intel**, **macOS Apple Silicon**, and **Windows x86_64**. A self-updating **loader** binary is also available that auto-downloads the latest version on each launch.

---

## Installation

### Download Pre-Built Binaries

Go to the [Releases page](../../releases/latest) and download the binary for your platform.

| Platform | Binary | Loader (auto-updater) |
|---|---|---|
| Linux x86_64 | `frikadellen_baf-linux-x86_64` | `FrikadellenBAF-loader-linux-x86_64` |
| macOS Intel | `frikadellen_baf-macos-x86_64` | `FrikadellenBAF-loader-macos-x86_64` |
| macOS Apple Silicon | `frikadellen_baf-macos-aarch64` | `FrikadellenBAF-loader-macos-aarch64` |
| Windows | `frikadellen_baf-windows-x86_64.exe` | `FrikadellenBAF-loader-windows-x86_64.exe` |

> **Tip:** The **Loader** binary is recommended — it automatically checks for and downloads the latest version every time you run it.

### Linux VPS (one-liner)

Using the loader (recommended):

```bash
wget https://github.com/TreXito/frikadellen-baf-121/releases/latest/download/FrikadellenBAF-loader-linux-x86_64 && chmod +x FrikadellenBAF-loader-linux-x86_64
./FrikadellenBAF-loader-linux-x86_64
```

Or the standalone binary:

```bash
wget https://github.com/TreXito/frikadellen-baf-121/releases/latest/download/frikadellen_baf-linux-x86_64 && chmod +x frikadellen_baf-linux-x86_64
./frikadellen_baf-linux-x86_64
```

### Windows

1. Download the `.exe` from [Releases](../../releases/latest)
2. Double-click to run (you may need to allow it through Windows Defender)

### macOS

1. Download the binary for your chip (Intel or Apple Silicon) from [Releases](../../releases/latest)
2. Open Terminal, navigate to the download folder, and run:
   ```bash
   chmod +x frikadellen_baf-macos-*
   ./frikadellen_baf-macos-*
   ```
   If macOS blocks it, go to **System Settings → Privacy & Security** and click **Allow Anyway**.

---

## Quick Start — Your First Run

1. **Run the binary** (or loader) — a terminal window will open.
2. **Enter your Minecraft username** when prompted.
3. **Authenticate with Microsoft** — a browser window will open. Sign in with the Microsoft account linked to your Minecraft: Java Edition license.
4. **Wait for connection** — the bot will connect to Hypixel and automatically travel to Skyblock.
5. **Flipping starts automatically** — the bot connects to [Coflnet](https://sky.coflnet.com) via WebSocket and begins receiving and executing flip recommendations.
6. **Open the Web Panel** — go to `http://localhost:8080` in your browser (or `http://<your-vps-ip>:8080` if on a remote server) to monitor the bot, view profits, and change settings.

A `config.toml` file is created in the same directory as the binary after the first run. You can edit it manually or use the Config tab in the web panel.

---

## Web Control Panel

The bot runs a built-in web server (default port **8080**). Open it in your browser:

```
http://localhost:8080
```

If running on a VPS, replace `localhost` with your server's IP address. You can change the port with the `web_gui_port` config option and optionally set a password with `web_gui_password`.

### Panel Tabs

| Tab | What It Shows |
|---|---|
| **Controls** | Connection status, uptime, session profit, active account, inventory grid, active auction listings, and a game chat box where you can type commands |
| **AH Profit** | Auction house profit chart and flip history |
| **BZ Profit** | Bazaar profit chart and order history |
| **Total Profit** | Combined AH + Bazaar profit overview |
| **Game View** | Your character's full inventory and guild vault contents |
| **Config** | GUI editor for all `config.toml` settings, organized by category — change settings without restarting the bot |

### Panel Features

- **5 Themes**: Midnight (default), Obsidian, Emerald, Sakura, Ocean — switch from the dropdown in the header
- **Anonymize Mode**: One-click toggle to hide your in-game name in the panel (useful for streaming/screenshots)
- **Real-Time Updates**: All data updates live via WebSocket — no need to refresh
- **Mobile Friendly**: Works on phones and tablets

---

## Discord Webhooks

Send flip notifications and profit summaries to a Discord channel.

1. In your Discord server, go to **Channel Settings → Integrations → Webhooks** and create a new webhook. Copy the URL.
2. Set `webhook_url` in `config.toml` (or in the web panel Config tab) to the copied URL.
3. *(Optional)* Set `bazaar_webhook_url` to a different webhook URL if you want Bazaar events in a separate channel.
4. *(Optional)* Set `discord_id` to your Discord user ID (right-click your name → Copy User ID) to get **@pinged** when legendary or divine flips occur.

---

## Multi-Account Support

To rotate between multiple accounts:

1. Set `ingame_name` to a comma-separated list of usernames:
   ```toml
   ingame_name = "Account1,Account2,Account3"
   ```
2. Set `multi_switch_time` to the number of hours between rotations:
   ```toml
   multi_switch_time = 6.0
   ```
3. Each account will be authenticated via Microsoft login on first use. The bot stores session tokens so you only need to authenticate once per account.

Set `multi_switch_time = 0` or leave it unset to disable rotation.

---

## Humanization / Rest Breaks

Enable random periodic breaks to simulate a human player stepping away:

```toml
humanization_enabled = true
humanization_min_interval_minutes = 45
humanization_max_interval_minutes = 120
humanization_min_break_minutes = 2
humanization_max_break_minutes = 10
```

When a break triggers, the bot disconnects from the server, waits a random duration within the configured range, and then reconnects. Discord webhook notifications are sent when breaks start and end.

---

## Proxy Support

Route the bot's connection through a proxy:

```toml
proxy_enabled = true
proxy_address = "host:port"
proxy_credentials = "username:password"
```

---

## Configuration Reference

The `config.toml` file is created automatically on first run. You can edit it with a text editor or use the **Config** tab in the web panel. All settings are hot-reloadable through the web panel.

### General

| Field | Type | Default | Description |
|---|---|---|---|
| `ingame_name` | string | — | Minecraft username(s), comma-separated for multi-account |
| `multi_switch_time` | float | `0` | Hours between account switches (0 = disabled) |
| `web_gui_port` | integer | `8080` | Port for the web control panel |
| `web_gui_password` | string | — | Optional password for the web panel |
| `enable_console_input` | boolean | `true` | Allow typing commands in the terminal |
| `share_legendary_flips` | boolean | `true` | Share legendary/divine flips with the community |
| `websocket_url` | string | `wss://sky.coflnet.com/modsocket` | Coflnet WebSocket URL (advanced — don't change unless you know what you're doing) |

### Timing & Delays

| Field | Type | Default | Description |
|---|---|---|---|
| `command_delay_ms` | integer | `500` | Delay in ms between bot commands |
| `bed_spam_click_delay` | integer | `100` | Delay in ms for bed spam clicks |
| `bed_multiple_clicks_delay` | integer | `0` | Delay in ms between multiple clicks |
| `bed_pre_click_ms` | integer | `30` | Pre-click timing in ms (used with freemoney) |
| `bed_spam` | boolean | `false` | Enable bed spam clicking |

### Auction House

| Field | Type | Default | Description |
|---|---|---|---|
| `auction_duration_hours` | integer | `24` | Duration for new auction listings (hours) |
| `auction_listing_delay_ms` | integer | `1500` | Delay in ms between listing items |
| `max_items_in_inventory` | integer | `12` | Maximum flip items to hold in inventory |

### Bazaar

| Field | Type | Default | Description |
|---|---|---|---|
| `bazaar_order_check_interval_seconds` | integer | `60` | How often to check bazaar order status (seconds) |
| `bazaar_order_cancel_minutes_per_million` | integer | `1` | Minutes to wait per million coins before cancelling stale orders |
| `bazaar_tax_rate` | float | `1.25` | Bazaar sell tax percentage (1.0 if you have the tax perk) |

### Webhooks

| Field | Type | Default | Description |
|---|---|---|---|
| `webhook_url` | string | — | Discord webhook URL for flip notifications |
| `bazaar_webhook_url` | string | — | Separate Discord webhook for bazaar events |
| `discord_id` | string | — | Your Discord user ID for legendary/divine pings |

### Humanization

| Field | Type | Default | Description |
|---|---|---|---|
| `humanization_enabled` | boolean | `false` | Enable random rest breaks |
| `humanization_min_interval_minutes` | integer | `45` | Minimum minutes between breaks |
| `humanization_max_interval_minutes` | integer | `120` | Maximum minutes between breaks |
| `humanization_min_break_minutes` | integer | `2` | Minimum break duration in minutes |
| `humanization_max_break_minutes` | integer | `10` | Maximum break duration in minutes |

### Proxy

| Field | Type | Default | Description |
|---|---|---|---|
| `proxy_enabled` | boolean | `false` | Enable proxy connection |
| `proxy_address` | string | — | Proxy address (`host:port`) |
| `proxy_credentials` | string | — | Proxy credentials (`username:password`) |

### Advanced

| Field | Type | Default | Description |
|---|---|---|---|
| `use_cofl_chat` | boolean | `true` | Use Coflnet in-game chat |
| `auto_cookie` | integer | `0` | Auto-purchase booster cookie interval (0 = disabled) |
| `hypixel_api_key` | string | — | Hypixel API key for fetching active auctions |

---

## Troubleshooting

| Problem | Solution |
|---|---|
| **Authentication fails** | Re-run the bot and complete the Microsoft login flow again. Make sure you're using the Microsoft account linked to your Minecraft: Java Edition license. |
| **Bot won't connect to Hypixel** | Check your internet connection. If using a proxy, verify the address and credentials. Hypixel may be down — check [status.hypixel.net](https://status.hypixel.net). |
| **Web panel not loading** | Make sure the port (default 8080) is not blocked by a firewall. On a VPS, you may need to open the port: `sudo ufw allow 8080`. |
| **Bot keeps disconnecting** | The bot has automatic reconnection with exponential backoff (up to 5 attempts). If it keeps failing, check if your account is banned or the server is under maintenance. |
| **Inventory stuck / not selling** | The idle failsafe triggers every 30 minutes. If items are still stuck, restart the bot. |
| **Logs** | Check `logs/latest.log` in the same directory as the binary for detailed error information. |

---

## Building from Source

Requires the Rust **nightly** toolchain:

```bash
# Install Rust (if you don't have it)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install nightly toolchain
rustup install nightly
rustup default nightly

# Clone and build
git clone https://github.com/TreXito/frikadellen-baf-121.git
cd frikadellen-baf-121
cargo build --release
```

The compiled binary will be at `target/release/frikadellen_baf`.

### Launcher Script

A convenience script is included in the repo:

```bash
chmod +x frikadellen-baf-121
./frikadellen-baf-121
```

It checks for an existing binary, builds from source if needed, and runs the bot.

---

## Links

- **Discord**: [discord.gg/42DvX6T9jh](https://discord.gg/42DvX6T9jh)
- **Website**: [auctionflipper.bz](https://auctionflipper.bz)
- **Releases**: [GitHub Releases](../../releases)
- **Coflnet**: [sky.coflnet.com](https://sky.coflnet.com)

## License

This project is licensed under the **GNU Affero General Public License v3.0 (AGPLv3)**.
