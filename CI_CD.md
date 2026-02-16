# CI/CD Documentation

This repository uses GitHub Actions for continuous integration and automated releases.

## Workflows

### CI Workflow (`.github/workflows/ci.yml`)

Runs on every push and pull request to ensure code quality:

- **Test Suite**: Runs all unit tests (`cargo test`)
- **Format Check**: Verifies code formatting with `rustfmt`
- **Linting**: Checks code quality with `clippy`
- **Build Check**: Ensures code compiles on Linux, Windows, and macOS

**Triggers:**
- Push to `main` branch
- Push to `copilot/**` branches
- Pull requests to `main`

### Release Workflow (`.github/workflows/release.yml`)

Automatically builds and releases executables when changes are merged to main:

**Build Targets:**
- Linux x86_64 (`x86_64-unknown-linux-gnu`)
- macOS x86_64 Intel (`x86_64-apple-darwin`)
- macOS ARM64 Apple Silicon (`aarch64-apple-darwin`)
- Windows x86_64 (`x86_64-pc-windows-msvc`)

**Process:**
1. Builds release binaries for all platforms in parallel
2. Strips debug symbols (Linux/macOS only)
3. Renames binaries with platform suffix
4. Uploads as GitHub artifacts
5. Creates a GitHub release with all binaries attached

**Triggers:**
- Push to `main` branch
- Merged pull requests to `main`
- Tagged releases (`v*`)

**Release Naming:**
- For tags: Uses the tag name (e.g., `v3.0.0`)
- For commits: Uses format `build-YYYYMMDD-<short-sha>` (e.g., `build-20260216-abc1234`)

## Using Released Binaries

Download from the [Releases page](https://github.com/TreXito/frikadellen-baf-121/releases):

**Linux/macOS:**
```bash
# Download and make executable
chmod +x frikadellen_baf-*
./frikadellen_baf-*
```

**Windows:**
```
frikadellen_baf-windows-x86_64.exe
```

## Creating a New Release

### Automatic (Recommended)

Simply merge a pull request to `main`:
1. Create a PR with your changes
2. Wait for CI to pass
3. Merge the PR
4. The release workflow automatically builds and publishes binaries

### Manual (Tagged Release)

For versioned releases:

```bash
# Tag the release
git tag v3.0.1
git push origin v3.0.1

# Workflow will create a release named "v3.0.1"
```

## Build Cache

The workflows use GitHub Actions cache to speed up builds:
- Cargo registry cache
- Cargo git index cache
- Build target cache

Caches are keyed by OS and `Cargo.lock` hash, so they update when dependencies change.

## Optimization

Release builds use:
- `opt-level = 3` - Maximum optimization
- `lto = true` - Link-time optimization
- `codegen-units = 1` - Single codegen unit for better optimization
- `strip = true` - Remove debug symbols

This produces small, fast binaries (~3-5MB).

## Security

- **GITHUB_TOKEN**: Automatically provided by GitHub Actions
- No secrets needed for public repositories
- Workflows run in isolated containers
- Dependencies are cached but verified by Cargo

## Troubleshooting

### Build Fails on Specific Platform

Check the workflow logs for the specific platform. Common issues:
- Missing system dependencies
- Platform-specific code issues
- Cargo.lock conflicts

### Release Not Created

Ensure:
- The PR was merged (not just closed)
- The workflow has `GITHUB_TOKEN` permissions
- The branch is `main`

### Binary Size Too Large

If binaries exceed expectations:
- Verify `strip = true` in `Cargo.toml`
- Check for debug symbols: `file target/release/frikadellen_baf`
- Consider additional compression

## Local Testing

To test the build process locally:

```bash
# Linux
cargo build --release --target x86_64-unknown-linux-gnu

# macOS Intel
cargo build --release --target x86_64-apple-darwin

# macOS ARM
cargo build --release --target aarch64-apple-darwin

# Windows (on Windows)
cargo build --release --target x86_64-pc-windows-msvc
```

Note: Cross-compilation may require additional setup.
