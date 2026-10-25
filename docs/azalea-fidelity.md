# azalea fidelity: what this bot inherits from the framework

Community claim: "bans because of azalea". Verified against upstream
`azalea-rs/azalea` `main` and this tree, 2026-10-24. The claim is not one magic
packet. It is that every azalea bot shares **identical, session-constant
fingerprints**: anything the framework gets wrong is wrong on 100% of your
accounts, so one detection kills the whole fleet, and no per-account variation
helps.

Three verified tells, status, and where the fix lives.

## 1. `ClientInformation::default().allows_listing == false`  (FIXED)

`azalea-protocol/src/common/client_information.rs` ships `allows_listing: false`.
Vanilla's "Allow Server Listings" option defaults to ON, so a vanilla-branded
client with it off is something essentially nobody has. Every unconfigured
azalea bot announced it on every login.

Fix: `Event::Login` calls `bot.set_client_information()` with
`allows_listing: true` and `view_distance: 12` (`src/bot/client.rs`). Sent as a
normal option update, which players do routinely.

## 2. Resource pack acked as "loaded" in 0 ms  (FIXED, plugin replaced)

`azalea/src/accept_resource_packs.rs` answers a pack request with `Accepted` and
`SuccessfullyLoaded` in the same ECS update, never fetching the pack. A real
client cannot: `SuccessfullyLoaded` reports a completed download and apply. So
every azalea bot claims to have loaded a multi-megabyte pack in zero
milliseconds, in the first seconds of every connection.

Whether it fires on Hypixel depends on whether they push a pack on SkyBlock
join. If they do, it is a per-session impossibility that has nothing to do with
how you play, which is exactly the shape of "banned in minutes regardless".

Fix: `src/bot/resource_packs.rs` (`RealisticResourcePacksPlugin`) replaces it.
Azalea's plugin is disabled through the documented route:

```rust
ClientBuilder::new_without_plugins()
    .add_plugins(azalea::DefaultPlugins)
    .add_plugins(
        azalea::bot::DefaultBotPlugins
            .build()
            .disable::<azalea::accept_resource_packs::AcceptResourcePacksPlugin>(),
    )
```

`Accepted` still goes out immediately (correct for a forced pack),
`SuccessfullyLoaded` goes out after 1.2 to 5.0 s, and the request is logged as
`[ResourcePack] requested ...`. Grep the log: if that line never appears, Hypixel
does not push a pack and this tell was never live.

## 3. Brand says "vanilla"  (NOT CHANGED, deliberate)

`azalea-client/src/plugins/brand.rs` sends `minecraft:brand` = `"vanilla"`, with
the comment "pretend to be vanilla".

This is the one to think about rather than patch. A custom brand is
whitelistable: Hypixel publishes an allowed-mods position, and a mod that
announces itself can be allowed. An impersonator cannot be. Claiming vanilla
converts "some mod" into "a client lying about being vanilla", and it makes every
other anomaly in this list worse, because the server's model of you is now
inconsistent rather than merely modded.

Not changed because there is no supported-mod brand to claim. If that changes,
this is a three-line change to `deps/azalea-client/src/plugins/brand.rs`, which
is already a vendored patch target.

## What this does not cover

Framework fidelity is only one axis. The mod's own tells are separate and are
tracked elsewhere: click packet content and click spacing (`send_raw_click` /
`click_gate` in `src/bot/client.rs`) and the earnings curve
(`src/state/earnings.rs`).

## Verify

`cargo check` and `cargo test --lib` off this box. This box refuses to compile
(it froze the control plane on 2026-09-10 and 2026-09-13). rustfmt only proves
the files parse.
