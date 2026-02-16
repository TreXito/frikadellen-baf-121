# frikadellen-baf-121
Frikadellen BAF in the newest minecraft version based on Rust

## Features

- **Automated Auction House Flips**: Monitors and executes profitable BIN (Buy It Now) auctions
- **Bazaar Trading**: Automated bazaar order management and flipping
- **Microsoft Authentication**: Secure login with your Microsoft/Minecraft account
- **Hypixel Integration**: Direct connection to Hypixel Skyblock servers
- **Real-time Updates**: WebSocket connection to Coflnet for flip notifications
- **Configurable**: Easy-to-use configuration system

## Quick Start

1. Download the latest release for your platform from the [Releases](../../releases) page
2. Run the executable
3. Enter your Minecraft username when prompted
4. Complete Microsoft authentication in the browser that opens
5. The bot will connect to Hypixel and start monitoring for flips

For detailed setup instructions, see [Microsoft Authentication Setup Guide](MICROSOFT_AUTH_SETUP.md)

## Configuration

The application creates a `config.toml` file in the same directory as the executable. You can manually edit this file to customize settings:

- `ingame_name`: Your Minecraft username
- `enable_ah_flips`: Enable/disable auction house flips
- `enable_bazaar_flips`: Enable/disable bazaar flips
- `web_gui_port`: Port for the web interface (default: 8080)

## Requirements

- Minecraft: Java Edition license linked to a Microsoft account
- Access to Hypixel server (not banned)
- Internet connection

## Troubleshooting

See the [Microsoft Authentication Setup Guide](MICROSOFT_AUTH_SETUP.md) for common issues and solutions.

## Building from Source

Requires Rust nightly toolchain:

```bash
rustup install nightly
rustup default nightly
cargo build --release
```

### Using the Launcher Script

For convenience, you can use the `frikadellen-baf-121` launcher script:

```bash
chmod +x frikadellen-baf-121
./frikadellen-baf-121
```

The launcher script will:
- Check for an existing binary
- Automatically build from source if needed
- Run the application with any arguments you provide

## License

MIT
