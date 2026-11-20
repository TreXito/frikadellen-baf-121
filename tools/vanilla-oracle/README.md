# Vanilla oracle

Replays a BAF packet dump through Mojang's own 26.3 client code. For every
click and every login packet it reports whether a vanilla client, holding the
exact items Hypixel sent, would have sent the same bytes.

Nothing here touches the bot's traffic. The dump only observes, and the oracle
runs offline.

## 1. Record a dump

Either set `packet_dump = true` in `config.toml`, which writes a new
`logs/packets-<UTC time>.bin` next to the executable on every start (restarts
included) and deletes the oldest of those files beyond 2 GB in total, or name
one file with the variable:

```bash
BAF_PACKET_DUMP=/path/session.bin ./frikadellen_baf
# optional, per-file cap, default 512:
BAF_PACKET_DUMP_MAX_MB=256
# optional, total kept by packet_dump = true, default 2048:
BAF_PACKET_DUMP_KEEP_MB=4096
```

The log shows `[PacketDump] Recording ... to <file>`. Every process start
appends a `session_start` record, so one file can hold several runs.

Recorded: every configuration packet Hypixel sends (registries, tags, known
packs, resource pack pushes, pings, cookies), every container open, content,
slot, cursor and close packet, keepalives, pings, resource pack, disconnect and
transfer packets, and the bot's clicks, closes, button clicks, sign updates,
carried slot changes, client information, custom payloads, resource pack
replies, keepalives, pongs, ping requests and cookie replies. Chat, movement
and chunks are never written.

## 2. Check it

Needs Java 25 (Minecraft 26.3 targets it), `curl` and about 1 GB of RAM.

```bash
JAVA_HOME=/path/to/jdk-25 tools/vanilla-oracle/run.sh /path/session.bin        # differences only
JAVA_HOME=/path/to/jdk-25 tools/vanilla-oracle/run.sh /path/session.bin --all  # every packet
```

The first run downloads the official `client.jar` and `server.jar` from
piston-data.mojang.com, checks their sha1, takes the libraries out of the
server bundler and compiles `ClickOracle.java`. Cache:
`$VANILLA_ORACLE_CACHE`, default `~/.cache/baf-vanilla-oracle`.

## 3. Read the report

Each difference prints the record number, time, packet, open menu and one line
per category. A summary with counts per category ends the report.

| Category | Meaning |
| --- | --- |
| `OK` | identical to vanilla, byte for byte |
| `RAW_PATCH` | item hash built from the wire patch; vanilla drops entries equal to the item default first |
| `HASH` | item hash differs from vanilla `HashedStack.create` for another reason |
| `SLOTS` | the changed-slot set differs from what vanilla's click simulation produces |
| `STATE_ID` | state id is not the menu's latest |
| `NOT_OPEN` | click for a container that is not open; vanilla drops it and sends nothing |
| `OUT_OF_RANGE` | slot outside the menu; vanilla throws before sending |
| `ORDER` | same content, changed-slot entries in a different order than vanilla |
| `BYTES` | other byte difference |
| `COMPONENT_ORDER` | same packet, but an item's component hashes are in another order. Not a difference: vanilla collects them in an `IdentityHashMap`, so its order changes from one client JVM to the next, and the server decodes them into a map |
| `DECODE` | vanilla cannot decode the packet, or it has trailing bytes |
| `UNSIMULATED` | click the oracle does not simulate (player inventory shift-click, some special slots); item hashes are still checked |
| `LOGIN` | login or configuration packet that differs from vanilla: brand, known packs reply, a client_information resend vanilla would never send |
| `WRONG_STATE` | a game-only packet sent while the connection is in configuration (between the bot's `configuration_acknowledged` and Hypixel's `finish_configuration`). Vanilla sends none there; the server reads it with the configuration packet table |

How it decides what vanilla sends:

1. Registries: `RegistryDataCollector` and `KnownPacksManager`, the same calls
   `ClientConfigurationPacketListenerImpl` makes.
2. Items: decoded with the vanilla stream codecs bound to those registries.
3. Clicks: `AbstractContainerMenu.doClick` and `ChestMenu.quickMoveStack`,
   transcribed, then `MultiPlayerGameMode.handleContainerInput`'s changed-slot
   map (`Int2ObjectOpenHashMap`) and `HashedStack.create`, hashed with
   `HashOps.CRC32C_INSTANCE` like `ClientPacketListener`.
4. Encoding: `GameProtocols.SERVERBOUND_TEMPLATE`, compared with the bot's raw
   bytes.

## Self-test

```bash
tools/vanilla-oracle/run.sh --selftest [out.bin]
```

Builds a synthetic session with Mojang's encoders (vanilla registries,
Hypixel-style Bazaar items, one deliberate fault per category), writes it in
the dump format, runs the checker on it and fails unless every record lands in
its expected category. CI runs it on every change under `tools/vanilla-oracle`.

## Vanilla core registries

A vanilla 26.3 client answers Hypixel's `select_known_packs` offer with
`minecraft:core:26.3`, and the server then sends the core registry entries
without data. azalea-client does the same and fills those entries from
`deps/azalea-client/src/plugins/vanilla_core_26_3_registries.bin`, generated
here with Mojang's own `RegistrySynchronization.packRegistries`:

```bash
tools/vanilla-oracle/run.sh --core-registries \
  deps/azalea-client/src/plugins/vanilla_core_26_3_registries.bin [dump.bin]
```

With a dump it also compares every entry Hypixel sent with data against the
vanilla encoding. Hypixel encodes some numbers differently (floats for doubles,
int lists for int arrays), so expect differences there. Entries the server
sends without data that the core pack does not have are flagged `NO DATA`.
Regenerate the file whenever the Minecraft version changes, together with
`CORE_VERSION` in `known_packs.rs`.

## Dump format

Little-endian. The file starts with `BAFPKT1\n`. Each record is
`u8 kind | u64 unix micros | u8 name len | name | u32 len | raw packet`, where
kind is 0 inbound configuration, 1 inbound game, 2 outbound, 3 session start,
and raw packet is the VarInt packet id followed by the fields, exactly as on
the wire after decompression. Writer: `src/bot/packet_dump.rs`.
