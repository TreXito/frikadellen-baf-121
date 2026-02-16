# Microsoft Authentication Setup Guide

This guide explains how to set up Microsoft authentication for Frikadellen BAF to connect to Hypixel.

## Initial Setup

When you run Frikadellen BAF for the first time, you will be prompted for:

1. **In-game name**: Your Minecraft username (e.g., `zShadowReaper_`)

After entering your username, Microsoft authentication will happen automatically through your browser.

## Authentication Flow

The application uses Azalea's Microsoft authentication system which:

1. Prompts you to enter your Minecraft username
2. Opens a browser window for you to log in to your Microsoft account
3. Requests permission to access your Minecraft profile
4. Saves authentication tokens locally for future sessions
5. Automatically refreshes tokens when they expire

**Note:** You log in with your Microsoft email/password in the browser - the app only needs your username.

## First Run

```bash
./frikadellen_baf-linux-x86_64
```

You will see:
```
Enter your ingame name: [type your username]
```

After entering your username:
- A browser window will open automatically
- Log in with your Microsoft account (email and password)
- Grant permission for the application to access Minecraft
- The application will connect to Hypixel

## Subsequent Runs

On subsequent runs, the application will:
- Load your username from `config.toml`
- Use cached authentication tokens
- Only prompt for re-authentication in browser if tokens have expired

## Expected Output

When successfully connected, you should see:

```
INFO Configuration loaded for player: zShadowReaper_
INFO Initializing Minecraft bot...
INFO Authenticating with Microsoft account...
INFO A browser window will open for you to log in
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

- **Never share your `config.toml`** - it contains your in-game username
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
