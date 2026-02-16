# Microsoft Authentication Setup Guide

This guide explains how to set up Microsoft authentication for Frikadellen BAF to connect to Hypixel.

## Initial Setup

When you run Frikadellen BAF for the first time, you will be prompted for:

1. **In-game name**: Your Minecraft username (e.g., `zShadowReaper_`)
2. **Microsoft account email**: The email address associated with your Microsoft/Minecraft account

These credentials will be saved to `config.toml` in the same directory as the executable.

## Authentication Flow

The application uses Azalea's Microsoft authentication system which:

1. Opens a browser window for you to log in to your Microsoft account
2. Requests permission to access your Minecraft profile
3. Saves authentication tokens locally for future sessions
4. Automatically refreshes tokens when they expire

## First Run

```bash
./frikadellen_baf-linux-x86_64
```

You will see:
```
Enter your ingame name: [type your username]
Enter your Microsoft account email: [type your email]
```

After entering your credentials:
- A browser window will open
- Log in with your Microsoft account
- Grant permission for the application to access Minecraft
- The application will connect to Hypixel

## Subsequent Runs

On subsequent runs, the application will:
- Load your credentials from `config.toml`
- Use cached authentication tokens
- Only prompt for re-authentication if tokens have expired

## Expected Output

When successfully connected, you should see:

```
INFO Connecting to Hypixel with Microsoft account: your@email.com
INFO Bot connection initiated successfully
INFO ✓ Bot logged into Minecraft successfully
INFO ✓ Bot spawned in world and ready
```

## Troubleshooting

### "Failed to connect bot" Error

If you see:
```
WARN Failed to connect bot: [error message]
WARN The bot will continue running in limited mode (WebSocket only)
```

Possible causes:
1. **Invalid Microsoft account**: Ensure the email is correct and has a Minecraft license
2. **Network issues**: Check your internet connection
3. **Hypixel ban**: Your account may be temporarily banned from Hypixel
4. **Authentication expired**: Delete cached tokens and re-authenticate

### Authentication Cache Location

Authentication tokens are cached in:
- **Linux**: `~/.minecraft/` or `~/.local/share/minecraft/`
- **Windows**: `%APPDATA%\.minecraft\`
- **macOS**: `~/Library/Application Support/minecraft/`

To force re-authentication, delete the cached tokens in these directories.

### Multiple Instances

Each instance of Frikadellen BAF uses its own `config.toml` file in the executable's directory, allowing you to run multiple bots with different accounts simultaneously.

## Security Notes

- **Never share your `config.toml`** - it contains your in-game name and email
- **Keep authentication tokens secure** - they provide access to your Minecraft account
- **Use a strong password** - protect your Microsoft account with 2FA if possible

## Account Requirements

To use Frikadellen BAF, you need:
1. A valid Microsoft account
2. A Minecraft: Java Edition license linked to that account
3. Access to Hypixel server (not banned)

## Support

If you continue to have authentication issues:
1. Verify your Microsoft account has Minecraft access
2. Try logging into Hypixel manually with the official Minecraft launcher
3. Check Hypixel's status page for server issues
4. Review the logs in `baf.log` for detailed error messages
