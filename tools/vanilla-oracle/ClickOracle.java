// Replays a BAF packet dump (BAF_PACKET_DUMP) through Mojang's own 26.3 client
// code and reports every packet whose bytes differ from what a vanilla client
// would have sent in the same state.
//
//   java ClickOracle <dump.bin> [--all]     check a recording
//   java ClickOracle --selftest [out.bin]   prove the checker on a synthetic session
//   java ClickOracle --core-registries <out.bin> [dump.bin]   vanilla core registry data for azalea-client
//
// Nothing here is a reimplementation of the hashing or the codecs: registries
// are rebuilt with RegistryDataCollector + KnownPacksManager exactly like
// ClientConfigurationPacketListenerImpl, packets are decoded and encoded with
// the vanilla protocol codecs, and item hashes come from HashedStack.create
// with the same CRC32C RegistryOps generator ClientPacketListener uses. The
// only hand-written part is the container click simulation (a transcription of
// AbstractContainerMenu.doClick and ChestMenu.quickMoveStack for plain slots),
// which decides which slots changed. Anything it cannot simulate is reported as
// UNSIMULATED and still has every item hash checked against the menu.

import com.google.common.hash.HashCode;
import com.google.gson.JsonElement;
import com.google.gson.JsonParser;
import com.mojang.serialization.JsonOps;
import io.netty.buffer.ByteBuf;
import io.netty.buffer.ByteBufUtil;
import io.netty.buffer.Unpooled;
import it.unimi.dsi.fastutil.ints.Int2ObjectLinkedOpenHashMap;
import it.unimi.dsi.fastutil.ints.Int2ObjectMap;
import it.unimi.dsi.fastutil.ints.Int2ObjectOpenHashMap;
import it.unimi.dsi.fastutil.ints.IntArrayList;
import it.unimi.dsi.fastutil.ints.IntList;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Instant;
import java.time.ZoneOffset;
import java.time.format.DateTimeFormatter;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.EnumMap;
import java.util.HashMap;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.Set;
import java.util.TreeMap;
import java.util.function.Function;
import net.minecraft.SharedConstants;
import net.minecraft.client.multiplayer.ClientRegistryLayer;
import net.minecraft.client.multiplayer.KnownPacksManager;
import net.minecraft.client.multiplayer.RegistryDataCollector;
import net.minecraft.core.Registry;
import net.minecraft.core.RegistryAccess;
import net.minecraft.core.RegistrySynchronization;
import net.minecraft.core.component.DataComponentMap;
import net.minecraft.core.component.DataComponentPatch;
import net.minecraft.core.component.DataComponentType;
import net.minecraft.core.component.DataComponents;
import net.minecraft.core.component.TypedDataComponent;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.core.registries.Registries;
import net.minecraft.network.HashedPatchMap;
import net.minecraft.network.HashedStack;
import net.minecraft.network.ProtocolInfo;
import net.minecraft.network.RegistryFriendlyByteBuf;
import net.minecraft.network.chat.Component;
import net.minecraft.network.codec.ByteBufCodecs;
import net.minecraft.network.codec.StreamCodec;
import net.minecraft.network.protocol.Packet;
import net.minecraft.network.protocol.common.ClientboundUpdateTagsPacket;
import net.minecraft.network.protocol.common.ServerboundClientInformationPacket;
import net.minecraft.network.protocol.common.ServerboundCustomPayloadPacket;
import net.minecraft.network.protocol.common.custom.BrandPayload;
import net.minecraft.network.protocol.configuration.ClientboundFinishConfigurationPacket;
import net.minecraft.network.protocol.configuration.ClientboundRegistryDataPacket;
import net.minecraft.network.protocol.configuration.ClientboundSelectKnownPacks;
import net.minecraft.network.protocol.configuration.ConfigurationProtocols;
import net.minecraft.network.protocol.configuration.ServerboundFinishConfigurationPacket;
import net.minecraft.network.protocol.configuration.ServerboundSelectKnownPacks;
import net.minecraft.network.protocol.game.ClientboundContainerClosePacket;
import net.minecraft.network.protocol.game.ClientboundContainerSetContentPacket;
import net.minecraft.network.protocol.game.ClientboundContainerSetSlotPacket;
import net.minecraft.network.protocol.game.ClientboundLoginPacket;
import net.minecraft.network.protocol.game.ClientboundOpenScreenPacket;
import net.minecraft.network.protocol.game.ClientboundRespawnPacket;
import net.minecraft.network.protocol.game.ClientboundSetCursorItemPacket;
import net.minecraft.network.protocol.game.ClientboundSetPlayerInventoryPacket;
import net.minecraft.network.protocol.game.ClientboundStartConfigurationPacket;
import net.minecraft.network.protocol.game.GameProtocols;
import net.minecraft.network.protocol.game.ServerboundContainerButtonClickPacket;
import net.minecraft.network.protocol.game.ServerboundContainerClickPacket;
import net.minecraft.network.protocol.game.ServerboundContainerClosePacket;
import net.minecraft.network.protocol.game.ServerboundSetCarriedItemPacket;
import net.minecraft.resources.Identifier;
import net.minecraft.resources.RegistryDataLoader;
import net.minecraft.resources.RegistryOps;
import net.minecraft.resources.ResourceKey;
import net.minecraft.server.Bootstrap;
import net.minecraft.server.level.ClientInformation;
import net.minecraft.server.level.ParticleStatus;
import net.minecraft.world.entity.HumanoidArm;
import net.minecraft.world.entity.player.ChatVisiblity;
import net.minecraft.server.packs.repository.KnownPack;
import net.minecraft.server.packs.resources.CloseableResourceManager;
import net.minecraft.server.packs.resources.ResourceManager;
import net.minecraft.server.packs.resources.ResourceProvider;
import net.minecraft.tags.TagLoader;
import net.minecraft.tags.TagNetworkSerialization;
import net.minecraft.util.HashOps;
import net.minecraft.world.inventory.ContainerInput;
import net.minecraft.world.inventory.MenuType;
import net.minecraft.world.item.Item;
import net.minecraft.world.item.ItemStack;
import net.minecraft.world.item.Rarity;

@SuppressWarnings({"unchecked", "rawtypes"})
public final class ClickOracle {

    // ---------------------------------------------------------------- dump file

    static final byte[] MAGIC = "BAFPKT1\n".getBytes(StandardCharsets.US_ASCII);
    static final int IN_CONFIG = 0, IN_GAME = 1, OUT = 2, SESSION = 3;

    record Rec(int kind, long micros, String name, byte[] raw) {}

    static List<Rec> readDump(byte[] bytes) throws IOException {
        if (bytes.length < MAGIC.length || !Arrays.equals(Arrays.copyOf(bytes, MAGIC.length), MAGIC)) {
            throw new IOException("not a BAF packet dump (missing BAFPKT1 header)");
        }
        ByteBuffer b = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN);
        b.position(MAGIC.length);
        List<Rec> out = new ArrayList<>();
        while (b.hasRemaining()) {
            if (b.remaining() < 1 + 8 + 1) break;
            int kind = b.get() & 0xff;
            long micros = b.getLong();
            int nameLen = b.get() & 0xff;
            if (b.remaining() < nameLen + 4) break;
            byte[] name = new byte[nameLen];
            b.get(name);
            int len = b.getInt();
            if (len < 0 || b.remaining() < len) break;
            byte[] raw = new byte[len];
            b.get(raw);
            out.add(new Rec(kind, micros, new String(name, StandardCharsets.UTF_8), raw));
        }
        if (b.hasRemaining()) {
            System.out.println("note: the last " + b.remaining() + " bytes are a partial record (process killed mid-write); ignored");
        }
        return out;
    }

    static void writeRec(ByteArrayOutputStream out, Rec r) {
        ByteBuffer b = ByteBuffer.allocate(1 + 8 + 1 + r.name.length() + 4).order(ByteOrder.LITTLE_ENDIAN);
        b.put((byte) r.kind).putLong(r.micros).put((byte) r.name.length());
        b.put(r.name.getBytes(StandardCharsets.UTF_8)).putInt(r.raw.length);
        out.writeBytes(b.array());
        out.writeBytes(r.raw);
    }

    // ---------------------------------------------------------------- findings

    enum Cat {
        OK("identical to vanilla, byte for byte"),
        RAW_PATCH("item hash built from the wire patch; vanilla drops default-equal entries first"),
        HASH("item hash differs from vanilla HashedStack.create"),
        SLOTS("changed-slot set differs from what vanilla's click changes"),
        STATE_ID("state id is not the menu's latest"),
        NOT_OPEN("click for a container that is not open; vanilla never sends it"),
        OUT_OF_RANGE("slot outside the menu; vanilla crashes before sending"),
        ORDER("same content, changed-slot entries in a different order than vanilla"),
        BYTES("decodes the same but the bytes differ from vanilla's encoding"),
        DECODE("vanilla's codec rejects the packet"),
        UNSIMULATED("click type not simulated; item hashes still checked"),
        LOGIN("login/configuration packet differs from vanilla");

        final String meaning;

        Cat(String meaning) {
            this.meaning = meaning;
        }
    }

    static final class Finding {
        final int record;
        final long micros;
        final String header;
        final EnumMap<Cat, List<String>> lines = new EnumMap<>(Cat.class);

        Finding(int record, long micros, String header) {
            this.record = record;
            this.micros = micros;
            this.header = header;
        }

        void add(Cat cat, String line) {
            lines.computeIfAbsent(cat, k -> new ArrayList<>()).add(line);
        }

        boolean ok() {
            return lines.isEmpty() || (lines.size() == 1 && lines.containsKey(Cat.OK));
        }
    }

    // ---------------------------------------------------------------- items and menus

    /** An item as the vanilla client holds it, plus its component patch exactly as it came off the wire. */
    record St(ItemStack stack, DataComponentPatch wire) {
        static final St EMPTY = new St(ItemStack.EMPTY, DataComponentPatch.EMPTY);

        boolean isEmpty() {
            return stack.isEmpty();
        }

        int count() {
            return stack.getCount();
        }

        St withCount(int n) {
            return n <= 0 ? EMPTY : new St(stack.copyWithCount(n), wire);
        }
    }

    static final class Menu {
        final int id;
        final String type;
        final String title;
        int containerSlots; // -1 until known; menu 0 uses the inventory layout
        St[] own;
        St carried = St.EMPTY;
        int stateId;

        Menu(int id, String type, String title, int containerSlots) {
            this.id = id;
            this.type = type;
            this.title = title;
            this.containerSlots = containerSlots;
            this.own = new St[Math.max(id == 0 ? 5 : containerSlots, 0)];
            Arrays.fill(own, St.EMPTY);
        }

        int size() {
            if (id == 0) return 46;
            return containerSlots < 0 ? -1 : containerSlots + 36;
        }

        /** Player Inventory index behind a menu slot, or -1 for the menu's own container. */
        int invIndex(int slot) {
            if (id == 0) {
                if (slot >= 5 && slot <= 8) return 39 - (slot - 5);
                if (slot >= 9 && slot <= 35) return slot;
                if (slot >= 36 && slot <= 44) return slot - 36;
                if (slot == 45) return 40;
                return -1;
            }
            int n = containerSlots;
            if (slot >= n && slot < n + 27) return slot - n + 9;
            if (slot >= n + 27 && slot < n + 36) return slot - n - 27;
            return -1;
        }

        void resize(int containerSlots) {
            this.containerSlots = containerSlots;
            St[] next = new St[containerSlots];
            Arrays.fill(next, St.EMPTY);
            System.arraycopy(own, 0, next, 0, Math.min(own.length, next.length));
            own = next;
        }

        String label() {
            return id == 0 ? "inventory" : id + " " + type + " \"" + title + "\"";
        }
    }

    /** A menu plus the shared player inventory, copied so a click can be simulated and then committed. */
    static final class View {
        final Menu menu;
        final St[] own;
        final St[] inv;
        St carried;

        View(Menu menu, St[] inv) {
            this.menu = menu;
            this.own = menu.own.clone();
            this.inv = inv.clone();
            this.carried = menu.carried;
        }

        St get(int slot) {
            int i = menu.invIndex(slot);
            return i >= 0 ? inv[i] : own[slot];
        }

        void set(int slot, St st) {
            int i = menu.invIndex(slot);
            if (i >= 0) inv[i] = st;
            else own[slot] = st;
        }

        void commit(St[] liveInv) {
            System.arraycopy(own, 0, menu.own, 0, own.length);
            System.arraycopy(inv, 0, liveInv, 0, inv.length);
            menu.carried = carried;
        }
    }

    static int typeSize(MenuType<?> type) {
        if (type == MenuType.GENERIC_9x1) return 9;
        if (type == MenuType.GENERIC_9x2) return 18;
        if (type == MenuType.GENERIC_9x3) return 27;
        if (type == MenuType.GENERIC_9x4) return 36;
        if (type == MenuType.GENERIC_9x5) return 45;
        if (type == MenuType.GENERIC_9x6) return 54;
        if (type == MenuType.GENERIC_3x3) return 9;
        if (type == MenuType.HOPPER) return 5;
        return -1;
    }

    /** Menus whose slots are all plain Slot and whose quickMoveStack is the chest shape. */
    static boolean plainMenu(String type) {
        return type.startsWith("minecraft:generic_") || type.equals("minecraft:hopper");
    }

    // ---------------------------------------------------------------- the oracle

    final List<Finding> findings = new ArrayList<>();
    final Map<Cat, Integer> counts = new EnumMap<>(Cat.class);
    final boolean printAll;
    List<Rec> recs = List.of();
    int at = -1;
    int sessions;
    int clicks;

    // configuration
    boolean inConfig;
    boolean reconfigurePending;
    RegistryDataCollector collector;
    KnownPacksManager knownPacks;
    List<KnownPack> serverPacks;
    List<KnownPack> vanillaReply;
    RegistryAccess.Frozen registries;

    // game
    ProtocolInfo gameIn;
    ProtocolInfo gameOut;
    HashedPatchMap.HashGenerator hasher;
    RegistryOps<JsonElement> jsonOps;
    Menu inventory;
    Menu open;
    St[] inv = emptyInv();
    final Set<String> notedOnce = new HashSet<>();
    ClientInformation lastInfo;
    long lastInfoMicros;

    ClickOracle(boolean printAll) {
        this.printAll = printAll;
    }

    static St[] emptyInv() {
        St[] a = new St[41];
        Arrays.fill(a, St.EMPTY);
        return a;
    }

    Menu current() {
        return open != null ? open : inventory;
    }

    void run(List<Rec> all) {
        recs = all;
        for (at = 0; at < all.size(); at++) {
            Rec r = all.get(at);
            try {
                switch (r.kind) {
                    case SESSION -> startSession();
                    case IN_CONFIG -> configIn(r);
                    case IN_GAME -> gameIn(r);
                    case OUT -> out(r);
                    default -> once("kind" + r.kind, "unknown record kind " + r.kind + "; skipped");
                }
            } catch (RuntimeException e) {
                Finding f = finding(r, r.name + " (oracle error)");
                f.add(Cat.DECODE, "oracle could not process this record: " + e);
                report(f);
            }
        }
    }

    void startSession() {
        sessions++;
        inConfig = false;
        reconfigurePending = false;
        registries = null;
        gameIn = null;
        gameOut = null;
        inventory = null;
        open = null;
        inv = emptyInv();
        lastInfo = null;
        System.out.println("== session " + sessions + " (" + time(recs.get(at).micros) + ")");
    }

    void once(String key, String message) {
        if (notedOnce.add(key)) System.out.println("note: " + message);
    }

    Finding finding(Rec r, String header) {
        return new Finding(at, r.micros, header);
    }

    void report(Finding f) {
        if (f.lines.isEmpty()) f.add(Cat.OK, "");
        findings.add(f);
        for (Cat c : f.lines.keySet()) counts.merge(c, 1, Integer::sum);
        if (f.ok() && !printAll) return;
        System.out.println("#" + f.record + " " + time(f.micros) + " " + f.header);
        f.lines.forEach((cat, lines) -> {
            for (String line : lines) {
                System.out.println("    " + cat + (line.isEmpty() ? "" : "  " + line));
            }
        });
    }

    // ---------------------------------------------------------------- configuration

    void enterConfig() {
        boolean reconfigure = reconfigurePending && registries != null;
        reconfigurePending = false;
        inConfig = true;
        collector = new RegistryDataCollector();
        knownPacks = null;
        serverPacks = null;
        vanillaReply = null;
        if (!reconfigure) {
            registries = null;
            inventory = null;
            open = null;
            inv = emptyInv();
        }
        gameIn = null;
        gameOut = null;
    }

    void configIn(Rec r) {
        if (!inConfig) enterConfig();
        Packet<?> p = decode(ConfigurationProtocols.CLIENTBOUND.codec(), r, "inbound");
        if (p == null) return;
        switch (p) {
            case ClientboundRegistryDataPacket d -> collector.appendContents(d.registry(), d.entries());
            case ClientboundUpdateTagsPacket t -> collector.appendTags(t.tags());
            case ClientboundSelectKnownPacks k -> {
                if (knownPacks == null) knownPacks = new KnownPacksManager();
                serverPacks = k.knownPacks();
                vanillaReply = knownPacks.trySelectingPacks(k.knownPacks());
            }
            case ClientboundFinishConfigurationPacket f -> finishConfig();
            default -> {}
        }
    }

    void finishConfig() {
        RegistryAccess.Frozen original = registries != null ? registries : ClientRegistryLayer.createRegistryAccess().compositeAccess();
        RegistryAccess.Frozen regs;
        if (knownPacks == null) {
            regs = collector.collectGameRegistries(ResourceProvider.EMPTY, original, false);
        } else {
            try (CloseableResourceManager rm = knownPacks.createResourceManager()) {
                regs = collector.collectGameRegistries(rm, original, false);
            }
        }
        bindGame(regs);
        inConfig = false;
        if (inventory == null) inventory = new Menu(0, "inventory", "inventory", 0);
        System.out.println("   registries rebuilt the vanilla way (" + regs.listRegistryKeys().count() + " registries)");
    }

    void bindGame(RegistryAccess.Frozen regs) {
        registries = regs;
        Function<ByteBuf, RegistryFriendlyByteBuf> decorator = RegistryFriendlyByteBuf.decorator(regs);
        gameIn = GameProtocols.CLIENTBOUND_TEMPLATE.bind(decorator);
        gameOut = GameProtocols.SERVERBOUND_TEMPLATE.bind(decorator, new GameProtocols.Context() {
            @Override
            public boolean hasInfiniteMaterials() {
                return false;
            }

            @Override
            public boolean canUseCommandBlocks() {
                return false;
            }
        });
        // Same generator as ClientPacketListener.decoratedHashOpsGenerator.
        RegistryOps<HashCode> hashOps = regs.createSerializationContext(HashOps.CRC32C_INSTANCE);
        hasher = component -> ((HashCode) component.encodeValue(hashOps)
                .getOrThrow(msg -> new IllegalArgumentException("Failed to hash " + component + ": " + msg)))
                .asInt();
        jsonOps = regs.createSerializationContext(JsonOps.INSTANCE);
    }

    // ---------------------------------------------------------------- game, inbound

    void gameIn(Rec r) {
        if (gameIn == null) {
            once("early-game", "game packets before configuration finished are skipped");
            return;
        }
        Packet<?> p = decode(gameIn.codec(), r, "inbound");
        if (p == null) return;
        switch (p) {
            case ClientboundLoginPacket l -> resetPlayer();
            case ClientboundRespawnPacket rp -> resetPlayer();
            case ClientboundStartConfigurationPacket s -> reconfigurePending = true;
            case ClientboundUpdateTagsPacket t -> t.tags().forEach((key, payload) -> applyTags(registries, key, payload));
            case ClientboundOpenScreenPacket o -> {
                String type = String.valueOf(BuiltInRegistries.MENU.getKey(o.getType()));
                open = new Menu(o.getContainerId(), type, o.getTitle().getString(), typeSize(o.getType()));
            }
            case ClientboundContainerSetContentPacket c -> setContent(c, r);
            case ClientboundContainerSetSlotPacket s -> setSlot(s, r);
            case ClientboundSetCursorItemPacket c -> current().carried = new St(c.contents(), wireOfSingle(r, 0));
            case ClientboundSetPlayerInventoryPacket s -> {
                if (s.slot() >= 0 && s.slot() < inv.length) inv[s.slot()] = new St(s.contents(), wireOfSingle(r, 1));
            }
            case ClientboundContainerClosePacket c -> open = null;
            default -> {}
        }
    }

    void resetPlayer() {
        inventory = new Menu(0, "inventory", "inventory", 0);
        open = null;
        inv = emptyInv();
    }

    static <T> void applyTags(RegistryAccess regs, ResourceKey<?> key, TagNetworkSerialization.NetworkPayload payload) {
        Registry<T> registry = regs.lookupOrThrow((ResourceKey<? extends Registry<? extends T>>) key);
        registry.prepareTagReload(payload.resolve(registry)).apply();
    }

    void setContent(ClientboundContainerSetContentPacket c, Rec r) {
        Menu m;
        if (c.containerId() == 0) m = inventory;
        else if (open != null && c.containerId() == open.id) m = open;
        else return; // vanilla ignores content for a menu that is not open
        WireContent wire = wireOfContent(r, c.items().size());
        if (m.id != 0) {
            int n = c.items().size() - 36;
            if (m.containerSlots != n && n >= 0) m.resize(n);
        }
        View v = new View(m, inv);
        for (int i = 0; i < c.items().size() && i < m.size(); i++) {
            v.set(i, new St(c.items().get(i), wire.items.get(i)));
        }
        v.carried = new St(c.carriedItem(), wire.carried);
        v.commit(inv);
        m.stateId = c.stateId();
    }

    void setSlot(ClientboundContainerSetSlotPacket s, Rec r) {
        Menu m;
        if (s.getContainerId() == 0) m = inventory;
        else if (open != null && s.getContainerId() == open.id) m = open;
        else return;
        if (m.size() < 0 || s.getSlot() < 0 || s.getSlot() >= m.size()) return;
        View v = new View(m, inv);
        v.set(s.getSlot(), new St(s.getItem(), wireOfSingle(r, 2)));
        v.commit(inv);
        m.stateId = s.getStateId();
    }

    // Raw component patches, read straight off the wire with the same layout the
    // vanilla codecs use, so a hash built from the unnormalized patch can be told
    // apart from any other mismatch.

    record WireContent(List<DataComponentPatch> items, DataComponentPatch carried) {}

    DataComponentPatch readWireItem(RegistryFriendlyByteBuf b) {
        int count = b.readVarInt();
        if (count <= 0) return DataComponentPatch.EMPTY;
        Item.STREAM_CODEC.decode(b);
        return DataComponentPatch.STREAM_CODEC.decode(b);
    }

    WireContent wireOfContent(Rec r, int n) {
        RegistryFriendlyByteBuf b = new RegistryFriendlyByteBuf(Unpooled.wrappedBuffer(r.raw), registries);
        b.readVarInt(); // packet id
        b.readContainerId();
        b.readVarInt(); // state id
        int size = b.readVarInt();
        List<DataComponentPatch> items = new ArrayList<>(size);
        for (int i = 0; i < size; i++) items.add(readWireItem(b));
        return new WireContent(items, readWireItem(b));
    }

    /** layout 0: cursor item, 1: player inventory (VarInt slot first), 2: container slot. */
    DataComponentPatch wireOfSingle(Rec r, int layout) {
        RegistryFriendlyByteBuf b = new RegistryFriendlyByteBuf(Unpooled.wrappedBuffer(r.raw), registries);
        b.readVarInt(); // packet id
        if (layout == 1) b.readVarInt();
        if (layout == 2) {
            b.readContainerId();
            b.readVarInt();
            b.readShort();
        }
        return readWireItem(b);
    }

    // ---------------------------------------------------------------- outbound

    void out(Rec r) {
        boolean configName = r.name.equals("select_known_packs") || r.name.equals("finish_configuration");
        // Outbound records carry no protocol state, and client_information or
        // custom_payload exist in both. Try the likely state first, then the other.
        StreamCodec config = ConfigurationProtocols.SERVERBOUND.codec();
        List<StreamCodec> codecs = new ArrayList<>();
        if (configName || inConfig || gameOut == null) {
            codecs.add(config);
            if (gameOut != null && !configName) codecs.add(gameOut.codec());
        } else {
            codecs.add(gameOut.codec());
            codecs.add(config);
        }
        Packet<?> p = null;
        StreamCodec used = null;
        String lastError = null;
        for (StreamCodec codec : codecs) {
            try {
                ByteBuf b = Unpooled.wrappedBuffer(r.raw);
                Packet<?> candidate = (Packet<?>) codec.decode(b);
                if (!candidate.type().id().getPath().equals(r.name)) {
                    lastError = "id decodes as " + candidate.type().id() + ", azalea calls it " + r.name;
                    continue;
                }
                if (b.readableBytes() != 0) {
                    lastError = b.readableBytes() + " trailing bytes vanilla would reject";
                    continue;
                }
                p = candidate;
                used = codec;
                break;
            } catch (RuntimeException e) {
                lastError = e.getMessage();
            }
        }
        if (p == null) {
            Finding f = finding(r, "out " + r.name);
            f.add(Cat.DECODE, lastError == null ? "no protocol state to decode it in" : lastError);
            report(f);
            return;
        }
        switch (p) {
            case ServerboundContainerClickPacket c -> click(r, c);
            case ServerboundSelectKnownPacks k -> knownPacksReply(r, k, used);
            case ServerboundClientInformationPacket ci -> clientInformation(r, ci, used);
            case ServerboundCustomPayloadPacket cp -> customPayload(r, cp, used);
            case ServerboundContainerClosePacket c -> {
                Finding f = finding(r, "out container_close " + c.getContainerId());
                if (c.getContainerId() != current().id) {
                    f.add(Cat.NOT_OPEN, "closes " + c.getContainerId() + " while " + current().label() + " is open");
                }
                roundTrip(f, r, p, used);
                open = null;
                reportQuiet(f);
            }
            case ServerboundContainerButtonClickPacket b -> {
                Finding f = finding(r, "out container_button_click " + b.containerId() + " button " + b.buttonId());
                if (b.containerId() != current().id) f.add(Cat.NOT_OPEN, "container " + b.containerId() + " is not open");
                roundTrip(f, r, p, used);
                report(f);
            }
            case ServerboundSetCarriedItemPacket s -> {
                Finding f = finding(r, "out set_carried_item " + s.getSlot());
                if (s.getSlot() < 0 || s.getSlot() > 8) f.add(Cat.OUT_OF_RANGE, "hotbar slot " + s.getSlot());
                roundTrip(f, r, p, used);
                reportQuiet(f);
            }
            default -> {
                Finding f = finding(r, "out " + r.name);
                roundTrip(f, r, p, used);
                reportQuiet(f);
            }
        }
    }

    /** Counted and printed only when something is off, even with --all. */
    void reportQuiet(Finding f) {
        if (f.lines.isEmpty()) {
            f.add(Cat.OK, "");
            findings.add(f);
            counts.merge(Cat.OK, 1, Integer::sum);
            return;
        }
        report(f);
    }

    void roundTrip(Finding f, Rec r, Packet<?> p, StreamCodec codec) {
        byte[] again = encode(codec, p);
        if (!Arrays.equals(again, r.raw)) {
            f.add(Cat.BYTES, "bot " + hex(r.raw) + "\n          vanilla re-encodes " + hex(again));
        }
    }

    void knownPacksReply(Rec r, ServerboundSelectKnownPacks k, StreamCodec codec) {
        Finding f = finding(r, "out select_known_packs " + k.knownPacks());
        if (vanillaReply == null) {
            f.add(Cat.LOGIN, "sent without a select_known_packs request in this configuration phase");
        } else {
            byte[] vanilla = encode(codec, new ServerboundSelectKnownPacks(vanillaReply));
            if (!Arrays.equals(vanilla, r.raw)) {
                f.add(Cat.LOGIN, "server offered " + serverPacks + "; vanilla 26.3 replies " + vanillaReply + "; bot replied " + k.knownPacks());
            }
        }
        System.out.println("   login: server offered " + serverPacks + ", bot replied " + k.knownPacks()
                + (vanillaReply == null ? "" : ", vanilla replies " + vanillaReply));
        report(f);
    }

    /** Options.buildPlayerInformation() on a fresh 64-bit install. */
    static final ClientInformation OPTIONS_DEFAULTS = new ClientInformation(
            "en_us", 12, ChatVisiblity.FULL, true, 0x7f, HumanoidArm.RIGHT, false, true, ParticleStatus.ALL);

    void clientInformation(Rec r, ServerboundClientInformationPacket p, StreamCodec codec) {
        ClientInformation bot = p.information();
        boolean configState = codec == ConfigurationProtocols.SERVERBOUND.codec();
        // A fresh login has no registries yet; a reconfiguration keeps them.
        boolean reconfiguring = reconfigurePending || (inConfig && registries != null);
        Finding f = finding(r, "out client_information (" + (configState ? "configuration" : "game") + ")");
        roundTrip(f, r, p, codec);
        if (configState && !reconfiguring) {
            // ClientHandshakePacketListenerImpl: the one packet vanilla sends per login.
            System.out.println("   login: client_information " + bot);
            if (!bot.equals(OPTIONS_DEFAULTS)) {
                System.out.println("          differs from fresh vanilla Options: " + infoDiff(OPTIONS_DEFAULTS, bot));
            }
        } else if (configState) {
            f.add(Cat.LOGIN, "sent during reconfiguration. Vanilla sends client_information in configuration only once, when login finishes.");
        } else if (lastInfo != null && bot.equals(lastInfo)) {
            f.add(Cat.LOGIN, "identical to the one sent " + (r.micros - lastInfoMicros) / 1000
                    + " ms earlier. ClientPacketListener.broadcastClientInformation skips unchanged information, so vanilla never sends this.");
        } else if (lastInfo != null) {
            f.add(Cat.LOGIN, "options changed " + (r.micros - lastInfoMicros) / 1000 + " ms after the previous client_information: "
                    + infoDiff(lastInfo, bot) + ". Vanilla sends this only after the player edits Options.");
        }
        lastInfo = bot;
        lastInfoMicros = r.micros;
        report(f);
    }

    static String infoDiff(ClientInformation a, ClientInformation b) {
        List<String> d = new ArrayList<>();
        if (!a.language().equals(b.language())) d.add("language " + a.language() + " -> " + b.language());
        if (a.viewDistance() != b.viewDistance()) d.add("view distance " + a.viewDistance() + " -> " + b.viewDistance());
        if (a.chatVisibility() != b.chatVisibility()) d.add("chat " + a.chatVisibility() + " -> " + b.chatVisibility());
        if (a.chatColors() != b.chatColors()) d.add("chat colors " + a.chatColors() + " -> " + b.chatColors());
        if (a.modelCustomisation() != b.modelCustomisation()) d.add(String.format("skin parts 0x%02x -> 0x%02x", a.modelCustomisation(), b.modelCustomisation()));
        if (a.mainHand() != b.mainHand()) d.add("main hand " + a.mainHand() + " -> " + b.mainHand());
        if (a.textFilteringEnabled() != b.textFilteringEnabled()) d.add("text filtering " + a.textFilteringEnabled() + " -> " + b.textFilteringEnabled());
        if (a.allowsListing() != b.allowsListing()) d.add("server listing " + a.allowsListing() + " -> " + b.allowsListing());
        if (a.particleStatus() != b.particleStatus()) d.add("particles " + a.particleStatus() + " -> " + b.particleStatus());
        return String.join(", ", d);
    }

    void customPayload(Rec r, ServerboundCustomPayloadPacket p, StreamCodec codec) {
        Finding f = finding(r, "out custom_payload " + p.payload().type().id());
        if (p.payload() instanceof BrandPayload brand) {
            System.out.println("   login: brand \"" + brand.brand() + "\" (vanilla client sends \"vanilla\")");
            if (!brand.brand().equals("vanilla")) f.add(Cat.LOGIN, "brand \"" + brand.brand() + "\", vanilla sends \"vanilla\"");
        } else {
            System.out.println("   custom_payload " + p.payload().type().id() + " (" + r.raw.length + " bytes)");
        }
        roundTrip(f, r, p, codec);
        report(f);
    }

    // ---------------------------------------------------------------- clicks

    void click(Rec r, ServerboundContainerClickPacket p) {
        clicks++;
        Menu m = current();
        int slot = p.slotNum();
        String onItem = (p.containerId() == m.id && m.size() >= 0 && slot >= 0 && slot < m.size())
                ? " on " + describe(new View(m, inv).get(slot)) : "";
        Finding f = finding(r, "click container " + p.containerId() + " slot " + slot + " " + p.containerInput()
                + " button " + p.buttonNum() + " state " + p.stateId() + onItem + "  [open: " + m.label() + "]");

        if (p.containerId() != m.id) {
            f.add(Cat.NOT_OPEN, "container " + p.containerId() + " is not open (open: " + m.label()
                    + "). MultiPlayerGameMode.handleContainerInput logs \"Ignoring click in mismatching container\" and sends nothing." + openedLater(p.containerId(), r.micros));
            report(f);
            return;
        }
        if (m.size() < 0) {
            f.add(Cat.UNSIMULATED, "no content received for this menu yet, its size is unknown");
            report(f);
            return;
        }
        if (slot >= m.size() || (slot < 0 && slot != -999 && slot != -1)) {
            f.add(Cat.OUT_OF_RANGE, "slot " + slot + " in a " + m.size() + "-slot menu: AbstractContainerMenu.clicked throws before anything is sent");
            report(f);
            return;
        }
        if (p.stateId() != m.stateId) {
            f.add(Cat.STATE_ID, "bot sent " + p.stateId() + ", the menu's latest is " + m.stateId);
        }

        View before = new View(m, inv);
        View after = new View(m, inv);
        String unsupported = simulate(after, m, slot, p.buttonNum(), p.containerInput());
        if (unsupported != null) {
            f.add(Cat.UNSIMULATED, unsupported);
            candidateCheck(f, before, p);
            report(f);
            return;
        }

        Int2ObjectMap<HashedStack> changed = new Int2ObjectOpenHashMap<>();
        Map<Integer, St> changedSt = new HashMap<>();
        for (int i = 0; i < m.size(); i++) {
            if (!ItemStack.matches(before.get(i).stack(), after.get(i).stack())) {
                changed.put(i, HashedStack.create(after.get(i).stack(), hasher));
                changedSt.put(i, after.get(i));
            }
        }
        HashedStack carried = HashedStack.create(after.carried.stack(), hasher);
        ServerboundContainerClickPacket vanilla = new ServerboundContainerClickPacket(
                m.id, m.stateId, (short) slot, p.buttonNum(), p.containerInput(), changed, carried);
        byte[] vanillaBytes = encode(gameOut.codec(), vanilla);

        if (!Arrays.equals(vanillaBytes, r.raw)) {
            int before0 = f.lines.size();
            Set<Integer> botKeys = new HashSet<>(p.changedSlots().keySet());
            for (int k : botKeys) {
                if (!changed.containsKey(k)) {
                    f.add(Cat.SLOTS, "bot reports slot " + k + " changed to " + describeHash(p.changedSlots().get(k))
                            + "; vanilla's click leaves it as " + describe(before.get(k)));
                }
            }
            for (int k : changed.keySet()) {
                if (!botKeys.contains(k)) {
                    f.add(Cat.SLOTS, "vanilla reports slot " + k + " changed to " + describe(changedSt.get(k)) + "; bot left it out");
                } else {
                    diffStack(f, "slot " + k, p.changedSlots().get(k), changedSt.get(k));
                }
            }
            diffStack(f, "carried", p.carriedItem(), after.carried);
            boolean sameContent = f.lines.size() == before0;
            if (sameContent && !f.lines.containsKey(Cat.STATE_ID)) {
                List<Integer> botOrder = encodedOrder(r.raw);
                List<Integer> vanillaOrder = new ArrayList<>(changed.keySet());
                if (botOrder != null && !botOrder.equals(vanillaOrder) && new HashSet<>(botOrder).equals(new HashSet<>(vanillaOrder))) {
                    f.add(Cat.ORDER, "bot " + botOrder + ", vanilla (Int2ObjectOpenHashMap) " + vanillaOrder);
                } else {
                    f.add(Cat.BYTES, "bot " + hex(r.raw) + "\n          vanilla " + hex(vanillaBytes));
                }
            }
        }
        // The vanilla client applies its own prediction; server packets correct it afterwards.
        after.commit(inv);
        report(f);
    }

    String openedLater(int containerId, long micros) {
        for (int i = at + 1; i < recs.size() && i < at + 400; i++) {
            Rec next = recs.get(i);
            if (next.kind == IN_GAME && next.name.equals("open_screen")) {
                try {
                    ClientboundOpenScreenPacket o = (ClientboundOpenScreenPacket) gameIn.codec().decode(Unpooled.wrappedBuffer(next.raw));
                    if (o.getContainerId() == containerId) {
                        return " That menu opened " + (next.micros - micros) / 1000 + " ms later (\"" + o.getTitle().getString() + "\").";
                    }
                } catch (RuntimeException ignored) {
                    return "";
                }
            }
        }
        return "";
    }

    List<Integer> encodedOrder(byte[] raw) {
        try {
            RegistryFriendlyByteBuf b = new RegistryFriendlyByteBuf(Unpooled.wrappedBuffer(raw), registries);
            b.readVarInt();
            b.readContainerId();
            b.readVarInt();
            b.readShort();
            b.readByte();
            ContainerInput.STREAM_CODEC.decode(b);
            StreamCodec<RegistryFriendlyByteBuf, Int2ObjectLinkedOpenHashMap<HashedStack>> codec = ByteBufCodecs.map(
                    Int2ObjectLinkedOpenHashMap::new, ByteBufCodecs.SHORT.map(Short::intValue, Integer::shortValue), HashedStack.STREAM_CODEC, 128);
            return new ArrayList<>(codec.decode(b).keySet());
        } catch (RuntimeException e) {
            return null;
        }
    }

    /**
     * AbstractContainerMenu.doClick for menus made of plain Slot (chests, generic
     * menus, hopper) and the main player inventory. Returns why it cannot, or null.
     */
    String simulate(View v, Menu m, int slot, int button, ContainerInput input) {
        boolean plain = m.id == 0 ? (slot == -999 || slot == -1 || slot >= 9) : plainMenu(m.type);
        if (!plain) return "menu type " + m.type + " / slot " + slot + " has special slots";
        if (input == ContainerInput.PICKUP || input == ContainerInput.QUICK_MOVE) {
            if (button != 0 && button != 1) return "button " + button + " is not a container click button; vanilla does nothing";
            if (slot == -999) {
                if (!v.carried.isEmpty()) v.carried = button == 0 ? St.EMPTY : v.carried.withCount(v.carried.count() - 1);
                return null;
            }
            if (slot < 0) return null;
            if (input == ContainerInput.QUICK_MOVE) {
                if (m.id == 0) return "shift-click in the player inventory (InventoryMenu.quickMoveStack) is not simulated";
                St clicked = quickMove(v, m, slot);
                while (!clicked.isEmpty() && ItemStack.isSameItem(v.get(slot).stack(), clicked.stack())) {
                    clicked = quickMove(v, m, slot);
                }
                return null;
            }
            St clicked = v.get(slot);
            St carried = v.carried;
            if (clicked.isEmpty()) {
                if (!carried.isEmpty()) {
                    int amount = button == 0 ? carried.count() : 1;
                    insert(v, slot, amount);
                }
            } else if (carried.isEmpty()) {
                int amount = button == 0 ? clicked.count() : (clicked.count() + 1) / 2;
                v.carried = clicked.withCount(amount);
                v.set(slot, clicked.withCount(clicked.count() - amount));
            } else if (ItemStack.isSameItemSameComponents(clicked.stack(), carried.stack())) {
                int amount = button == 0 ? carried.count() : 1;
                insert(v, slot, amount);
            } else if (carried.count() <= slotMax(carried)) {
                v.set(slot, carried);
                v.carried = clicked;
            }
            return null;
        }
        if (input == ContainerInput.CLONE) return null; // survival: no infinite materials, nothing changes
        if (input == ContainerInput.THROW) {
            if (!v.carried.isEmpty() || slot < 0) return null;
            St item = v.get(slot);
            if (item.isEmpty()) return null;
            if (button == 0) {
                v.set(slot, item.withCount(item.count() - 1));
            } else {
                v.set(slot, St.EMPTY);
            }
            return null;
        }
        if (input == ContainerInput.SWAP) {
            if (!((button >= 0 && button < 9) || button == 40)) return null;
            St source = v.inv[button];
            St target = v.get(slot);
            if (source.isEmpty() && target.isEmpty()) return null;
            if (source.isEmpty()) {
                v.inv[button] = target;
                v.set(slot, St.EMPTY);
            } else if (target.isEmpty()) {
                int max = slotMax(source);
                if (source.count() > max) {
                    v.set(slot, source.withCount(max));
                    v.inv[button] = source.withCount(source.count() - max);
                } else {
                    v.inv[button] = St.EMPTY;
                    v.set(slot, source);
                }
            } else {
                if (source.count() > slotMax(source)) return "swap that overflows into Inventory.add is not simulated";
                v.inv[button] = target;
                v.set(slot, source);
            }
            return null;
        }
        return input + " is not simulated";
    }

    /** Slot.getMaxStackSize(stack): the container's 99 capped by the item's own max. */
    static int slotMax(St st) {
        return Math.min(99, st.stack().getMaxStackSize());
    }

    /** Slot.safeInsert(carried, amount) for a plain slot. */
    static void insert(View v, int slot, int amount) {
        St carried = v.carried;
        St current = v.get(slot);
        int n = Math.min(Math.min(amount, carried.count()), slotMax(carried) - current.count());
        if (n <= 0) return;
        if (current.isEmpty()) {
            v.set(slot, carried.withCount(n));
            v.carried = carried.withCount(carried.count() - n);
        } else if (ItemStack.isSameItemSameComponents(current.stack(), carried.stack())) {
            v.set(slot, current.withCount(current.count() + n));
            v.carried = carried.withCount(carried.count() - n);
        }
    }

    /** ChestMenu.quickMoveStack (same shape for DispenserMenu and HopperMenu). */
    static St quickMove(View v, Menu m, int slot) {
        St st = v.get(slot);
        if (st.isEmpty()) return St.EMPTY;
        int n = m.containerSlots;
        ItemStack work = st.stack().copy();
        boolean moved = slot < n ? moveTo(v, work, st.wire(), n, m.size(), true) : moveTo(v, work, st.wire(), 0, n, false);
        if (!moved) return St.EMPTY;
        v.set(slot, work.isEmpty() ? St.EMPTY : new St(work, st.wire()));
        return st;
    }

    /** AbstractContainerMenu.moveItemStackTo over plain slots. */
    static boolean moveTo(View v, ItemStack item, DataComponentPatch wire, int start, int end, boolean backwards) {
        boolean changed = false;
        int dest = backwards ? end - 1 : start;
        if (item.isStackable()) {
            while (!item.isEmpty() && (backwards ? dest >= start : dest < end)) {
                St target = v.get(dest);
                if (!target.isEmpty() && ItemStack.isSameItemSameComponents(item, target.stack())) {
                    int total = target.count() + item.getCount();
                    int max = slotMax(target);
                    if (total <= max) {
                        item.setCount(0);
                        v.set(dest, target.withCount(total));
                        changed = true;
                    } else if (target.count() < max) {
                        item.shrink(max - target.count());
                        v.set(dest, target.withCount(max));
                        changed = true;
                    }
                }
                dest += backwards ? -1 : 1;
            }
        }
        if (!item.isEmpty()) {
            dest = backwards ? end - 1 : start;
            while (backwards ? dest >= start : dest < end) {
                if (v.get(dest).isEmpty()) {
                    int max = Math.min(99, item.getMaxStackSize());
                    v.set(dest, new St(item.split(Math.min(item.getCount(), max)), wire));
                    changed = true;
                    break;
                }
                dest += backwards ? -1 : 1;
            }
        }
        return changed;
    }

    /** Unsimulated click: every item hash the bot sent must match some item of that type in the menu. */
    void candidateCheck(Finding f, View v, ServerboundContainerClickPacket p) {
        List<St> candidates = new ArrayList<>();
        for (int i = 0; i < v.menu.size(); i++) if (!v.get(i).isEmpty()) candidates.add(v.get(i));
        if (!v.carried.isEmpty()) candidates.add(v.carried);
        Map<String, HashedStack> sent = new LinkedHashMap<>();
        p.changedSlots().forEach((k, h) -> sent.put("slot " + k, h));
        sent.put("carried", p.carriedItem());
        sent.forEach((where, h) -> {
            if (!(h instanceof HashedStack.ActualItem a)) return;
            St sameItem = null;
            for (St c : candidates) {
                if (!c.stack().typeHolder().equals(a.item())) continue;
                if (HashedPatchMap.create(c.stack().getComponentsPatch(), hasher).equals(a.components())) return;
                if (sameItem == null) sameItem = c;
            }
            if (sameItem == null) {
                f.add(Cat.HASH, where + ": " + describeHash(h) + " matches no item in the menu");
            } else {
                diffStack(f, where, h, sameItem.withCount(a.count()));
            }
        });
    }

    void diffStack(Finding f, String where, HashedStack bot, St expect) {
        HashedStack vanilla = HashedStack.create(expect.stack(), hasher);
        if (bot.equals(vanilla)) return;
        if (!(bot instanceof HashedStack.ActualItem b)) {
            f.add(Cat.HASH, where + ": bot sent empty, vanilla sends " + describe(expect));
            return;
        }
        if (!(vanilla instanceof HashedStack.ActualItem v)) {
            f.add(Cat.HASH, where + ": bot sent " + describeHash(bot) + ", vanilla sends empty");
            return;
        }
        String item = describe(expect);
        if (!b.item().equals(v.item())) {
            f.add(Cat.HASH, where + ": item " + b.item().getRegisteredName() + ", vanilla " + item);
            return;
        }
        if (b.count() != v.count()) {
            f.add(Cat.HASH, where + ": count " + b.count() + ", vanilla " + v.count() + " of " + item);
        }
        if (b.components().equals(v.components())) return;

        HashedPatchMap wireHash = HashedPatchMap.create(expect.wire(), hasher);
        boolean rawStyle = b.components().equals(wireHash);
        Cat cat = rawStyle ? Cat.RAW_PATCH : Cat.HASH;
        Map<DataComponentType<?>, TypedDataComponent<?>> onWire = index(expect.wire().split().added());
        Set<DataComponentType<?>> removedOnWire = expect.wire().split().removed();
        Map<DataComponentType<?>, TypedDataComponent<?>> normalized = index(expect.stack().getComponentsPatch().split().added());

        List<String> parts = new ArrayList<>();
        Map<DataComponentType<?>, Integer> ba = b.components().addedComponents();
        Map<DataComponentType<?>, Integer> va = v.components().addedComponents();
        for (DataComponentType<?> t : sorted(ba.keySet())) {
            if (va.containsKey(t)) continue;
            TypedDataComponent<?> w = onWire.get(t);
            parts.add("+" + name(t) + (w != null
                    ? " (Hypixel sent " + json(w) + ", equal to the item default, so vanilla drops it)"
                    : " (not on the received item)"));
        }
        for (DataComponentType<?> t : sorted(va.keySet())) {
            if (!ba.containsKey(t)) parts.add("-" + name(t) + " (vanilla hashes " + json(normalized.get(t)) + ", bot left it out)");
        }
        for (DataComponentType<?> t : sorted(ba.keySet())) {
            if (va.containsKey(t) && !ba.get(t).equals(va.get(t))) {
                parts.add("~" + name(t) + String.format(" hash %08x, vanilla %08x for %s", ba.get(t), va.get(t), json(normalized.get(t))));
            }
        }
        Set<DataComponentType<?>> br = b.components().removedComponents();
        Set<DataComponentType<?>> vr = v.components().removedComponents();
        for (DataComponentType<?> t : sorted(br)) {
            if (!vr.contains(t)) {
                parts.add("+!" + name(t) + (removedOnWire.contains(t)
                        ? " (Hypixel sent this removal; the item has no such default, so vanilla drops it)"
                        : " (removal not on the received item)"));
            }
        }
        for (DataComponentType<?> t : sorted(vr)) {
            if (!br.contains(t)) parts.add("-!" + name(t) + " (vanilla hashes this removal, bot left it out)");
        }
        f.add(cat, where + " " + item + ": " + String.join("; ", parts));
    }

    static Map<DataComponentType<?>, TypedDataComponent<?>> index(DataComponentMap map) {
        Map<DataComponentType<?>, TypedDataComponent<?>> out = new HashMap<>();
        for (TypedDataComponent<?> t : map) out.put(t.type(), t);
        return out;
    }

    static List<DataComponentType<?>> sorted(Set<DataComponentType<?>> types) {
        List<DataComponentType<?>> out = new ArrayList<>(types);
        out.sort((a, b) -> name(a).compareTo(name(b)));
        return out;
    }

    static String name(DataComponentType<?> t) {
        return String.valueOf(BuiltInRegistries.DATA_COMPONENT_TYPE.getKey(t));
    }

    String json(TypedDataComponent<?> t) {
        if (t == null) return "?";
        String s = t.encodeValue(jsonOps).result().map(JsonElement::toString).orElse("<unencodable>");
        return s.length() > 160 ? s.substring(0, 157) + "..." : s;
    }

    static String describe(St st) {
        if (st.isEmpty()) return "empty";
        ItemStack s = st.stack();
        Component custom = s.get(DataComponents.CUSTOM_NAME);
        String label = custom != null ? " \"" + custom.getString() + "\"" : "";
        return s.typeHolder().getRegisteredName() + " x" + s.getCount() + label;
    }

    static String describeHash(HashedStack h) {
        if (h instanceof HashedStack.ActualItem a) {
            return a.item().getRegisteredName() + " x" + a.count() + " (" + a.components().addedComponents().size() + " components)";
        }
        return "empty";
    }

    // ---------------------------------------------------------------- codec helpers

    Packet<?> decode(StreamCodec codec, Rec r, String dir) {
        ByteBuf b = Unpooled.wrappedBuffer(r.raw);
        try {
            Packet<?> p = (Packet<?>) codec.decode(b);
            if (b.readableBytes() != 0) {
                Finding f = finding(r, dir + " " + r.name);
                f.add(Cat.DECODE, b.readableBytes() + " bytes left after vanilla decoded it");
                report(f);
            }
            return p;
        } catch (RuntimeException e) {
            Finding f = finding(r, dir + " " + r.name);
            f.add(Cat.DECODE, "vanilla decode failed: " + e);
            report(f);
            return null;
        }
    }

    static byte[] encode(StreamCodec codec, Object packet) {
        ByteBuf b = Unpooled.buffer();
        codec.encode(b, packet);
        return ByteBufUtil.getBytes(b);
    }

    static String hex(byte[] bytes) {
        String h = ByteBufUtil.hexDump(bytes);
        return h.length() > 240 ? h.substring(0, 240) + "... (" + bytes.length + " bytes)" : h;
    }

    static final DateTimeFormatter TIME = DateTimeFormatter.ofPattern("yyyy-MM-dd HH:mm:ss.SSS").withZone(ZoneOffset.UTC);

    static String time(long micros) {
        return TIME.format(Instant.ofEpochSecond(micros / 1_000_000, (micros % 1_000_000) * 1000));
    }

    void summary() {
        System.out.println();
        System.out.println("== summary: " + recs.size() + " records, " + sessions + " session(s), " + clicks + " container clicks");
        int bad = 0;
        for (Cat c : Cat.values()) {
            int n = counts.getOrDefault(c, 0);
            if (n == 0) continue;
            if (c != Cat.OK) bad += n;
            System.out.printf("   %-12s %6d  %s%n", c, n, c.meaning);
        }
        System.out.println(bad == 0 ? "   every checked packet is what vanilla 26.3 sends" : "   see the lines above for each difference");
    }

    // ---------------------------------------------------------------- main

    static void bootstrap() {
        SharedConstants.tryDetectVersion();
        Bootstrap.bootStrap();
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 0) {
            System.err.println("usage: ClickOracle <dump.bin> [--all] | --selftest [out.bin] | --core-registries <out.bin> [dump.bin]");
            System.exit(64);
        }
        java.io.PrintStream stdout = System.out;
        bootstrap();
        System.setOut(stdout); // Bootstrap routes System.out through log4j
        if (args[0].equals("--selftest")) {
            System.exit(SelfTest.run(args.length > 1 ? Path.of(args[1]) : null));
        }
        if (args[0].equals("--core-registries")) {
            if (args.length < 2) {
                System.err.println("usage: ClickOracle --core-registries <out.bin> [dump.bin to compare with]");
                System.exit(64);
            }
            System.exit(CoreRegistries.run(Path.of(args[1]), args.length > 2 ? Path.of(args[2]) : null));
        }
        boolean all = Arrays.asList(args).contains("--all");
        List<Rec> recs = readDump(Files.readAllBytes(Path.of(args[0])));
        ClickOracle oracle = new ClickOracle(all);
        oracle.run(recs);
        oracle.summary();
        System.exit(0); // registry loading leaves executor threads behind
    }

    // ---------------------------------------------------------------- core registries

    /**
     * The registry contents a vanilla 26.3 server sends to a client that knows
     * no data packs: {@code RegistrySynchronization.packRegistries} with an
     * empty known set, over the registries a vanilla client loads from its
     * built-in {@code minecraft:core} pack.
     *
     * <p>A client that answers {@code select_known_packs} with
     * {@code [minecraft:core:26.3]}, like vanilla does, receives these entries
     * without data and has to supply them itself. azalea-client embeds this
     * file for exactly that.
     *
     * <p>File: repeated {@code u32 LE length | ClientboundRegistryDataPacket body}
     * (registry id, then the entry list, no packet id).
     */
    static final class CoreRegistries {
        static RegistryAccess.Frozen load(KnownPacksManager km) throws Exception {
            km.trySelectingPacks(List.of(KnownPack.vanilla("core")));
            RegistryDataCollector collector = new RegistryDataCollector();
            try (CloseableResourceManager rm = km.createResourceManager()) {
                Map<ResourceKey<?>, Map<Identifier, Integer>> indexes = new HashMap<>();
                for (RegistryDataLoader.RegistryData<?> data : RegistryDataLoader.SYNCHRONIZED_REGISTRIES) {
                    ResourceKey<? extends Registry<?>> key = (ResourceKey) data.key();
                    List<Identifier> ids = SelfTest.listIds(rm, key);
                    Map<Identifier, Integer> index = new HashMap<>();
                    List<RegistrySynchronization.PackedRegistryEntry> entries = new ArrayList<>();
                    for (Identifier id : ids) {
                        index.put(id, entries.size());
                        entries.add(new RegistrySynchronization.PackedRegistryEntry(id, Optional.empty()));
                    }
                    indexes.put(key, index);
                    collector.appendContents(key, entries);
                }
                collector.appendTags(SelfTest.buildTags(rm, indexes));
                return collector.collectGameRegistries(rm, ClientRegistryLayer.createRegistryAccess().compositeAccess(), false);
            }
        }

        static int run(Path out, Path dump) throws Exception {
            KnownPacksManager km = new KnownPacksManager();
            java.lang.reflect.Field f = KnownPacksManager.class.getDeclaredField("knownPackToId");
            f.setAccessible(true);
            System.out.println("vanilla client knows packs: " + ((Map<?, ?>) f.get(km)).keySet());
            RegistryAccess.Frozen regs = load(km);
            com.mojang.serialization.DynamicOps<net.minecraft.nbt.Tag> ops = regs.createSerializationContext(net.minecraft.nbt.NbtOps.INSTANCE);
            Map<ResourceKey<? extends Registry<?>>, List<RegistrySynchronization.PackedRegistryEntry>> packed = new LinkedHashMap<>();
            RegistrySynchronization.packRegistries(ops, regs, Set.of(), packed::put);

            ByteArrayOutputStream file = new ByteArrayOutputStream();
            int total = 0;
            for (var e : packed.entrySet()) {
                ByteBuf buf = Unpooled.buffer();
                ClientboundRegistryDataPacket.STREAM_CODEC.encode(new net.minecraft.network.FriendlyByteBuf(buf),
                        new ClientboundRegistryDataPacket(e.getKey(), e.getValue()));
                byte[] bytes = new byte[buf.readableBytes()];
                buf.readBytes(bytes);
                file.write(ByteBuffer.allocate(4).order(java.nio.ByteOrder.LITTLE_ENDIAN).putInt(bytes.length).array());
                file.write(bytes);
                total += e.getValue().size();
                System.out.println("  " + e.getKey().identifier() + ": " + e.getValue().size() + " entries, " + bytes.length + " bytes");
            }
            Files.write(out, file.toByteArray());
            System.out.println("wrote " + out + " (" + packed.size() + " registries, " + total + " entries, " + file.size() + " bytes)");
            return dump == null ? 0 : compare(packed, dump);
        }

        /** Every entry the server sent with data must equal ours byte for byte. */
        static int compare(Map<ResourceKey<? extends Registry<?>>, List<RegistrySynchronization.PackedRegistryEntry>> packed, Path dump) throws Exception {
            Map<String, Map<Identifier, Optional<net.minecraft.nbt.Tag>>> ours = new HashMap<>();
            packed.forEach((key, entries) -> {
                Map<Identifier, Optional<net.minecraft.nbt.Tag>> m = new HashMap<>();
                for (var en : entries) m.put(en.id(), en.data());
                ours.put(key.identifier().toString(), m);
            });
            int same = 0, differ = 0, omitted = 0, serverOnly = 0;
            Map<String, Integer> differByRegistry = new TreeMap<>();
            for (Rec r : readDump(Files.readAllBytes(dump))) {
                if (r.kind != IN_CONFIG || !r.name.equals("registry_data")) continue;
                ByteBuf b = Unpooled.wrappedBuffer(r.raw);
                ClientboundRegistryDataPacket d = (ClientboundRegistryDataPacket) ConfigurationProtocols.CLIENTBOUND.codec().decode(b);
                String reg = d.registry().identifier().toString();
                Map<Identifier, Optional<net.minecraft.nbt.Tag>> m = ours.get(reg);
                for (var en : d.entries()) {
                    Optional<net.minecraft.nbt.Tag> v = m == null ? null : m.get(en.id());
                    if (v == null) {
                        serverOnly++;
                        System.out.println("  server-only entry " + reg + " " + en.id() + (en.data().isPresent() ? " (with data)" : " (NO DATA, a client cannot fill it)"));
                    } else if (en.data().isEmpty()) {
                        omitted++;
                    } else if (en.data().equals(v)) {
                        same++;
                    } else {
                        differ++;
                        differByRegistry.merge(reg, 1, Integer::sum);
                        String show = System.getenv("ORACLE_SHOW_REGISTRY");
                        if (show != null ? reg.equals(show) : differ <= 10) System.out.println("  DIFFERS " + reg + " " + en.id() + "\n    server: " + en.data().get() + "\n    ours:   " + v.get());
                    }
                }
            }
            differByRegistry.forEach((reg, n) -> System.out.println("  " + reg + ": " + n + " entries encoded differently by the server"));
            System.out.println("compared with " + dump + ": " + same + " identical, " + differ + " different, "
                    + omitted + " sent without data, " + serverOnly + " server-only");
            return differ == 0 && same + omitted > 0 ? 0 : 1;
        }
    }

    // ---------------------------------------------------------------- self test

    /**
     * Builds a session the way a vanilla server would send it (known packs,
     * every synchronized registry, tags), a Bazaar-like menu with
     * Hypixel-style items, and clicks: vanilla-correct ones and each kind of
     * deviation. The checker must flag exactly the deviations.
     */
    static final class SelfTest {
        final ByteArrayOutputStream file = new ByteArrayOutputStream();
        final List<Rec> recs = new ArrayList<>();
        final ClickOracle gen = new ClickOracle(false);
        long clock = 1_794_600_000_000_000L;
        final List<Cat> expected = new ArrayList<>();
        final List<Integer> expectedAt = new ArrayList<>();

        void add(int kind, String name, byte[] raw) {
            clock += 20_000;
            Rec r = new Rec(kind, clock, name, raw);
            recs.add(r);
            writeRec(file, r);
            gen.recs = recs;
            gen.at = recs.size() - 1;
            switch (kind) {
                case IN_CONFIG -> gen.configIn(r);
                case IN_GAME -> gen.gameIn(r);
                default -> {}
            }
        }

        void expect(Cat cat) {
            expected.add(cat);
            expectedAt.add(recs.size() - 1);
        }

        static int run(Path out) throws Exception {
            SelfTest t = new SelfTest();
            t.build();
            byte[] bytes = t.fileBytes();
            if (out != null) {
                Files.write(out, bytes);
                System.out.println("wrote " + out + " (" + bytes.length + " bytes)");
            }
            System.out.println("---- checker output on the synthetic session ----");
            ClickOracle oracle = new ClickOracle(true);
            oracle.run(readDump(bytes));
            oracle.summary();
            System.out.println("---- expectations ----");
            int failures = 0;
            for (int i = 0; i < t.expected.size(); i++) {
                int record = t.expectedAt.get(i) + 1; // +1: the session_start record comes first in the file
                Cat want = t.expected.get(i);
                Finding got = null;
                for (Finding f : oracle.findings) if (f.record == record) got = f;
                boolean pass = got != null && (want == Cat.OK ? got.ok() : got.lines.containsKey(want)
                        && got.lines.keySet().stream().allMatch(c -> c == want || c == Cat.OK));
                if (!pass) failures++;
                System.out.println((pass ? "PASS " : "FAIL ") + "#" + record + " expect " + want
                        + (got == null ? " (no finding)" : " got " + got.lines.keySet()));
            }
            int otherBad = 0;
            for (Finding f : oracle.findings) {
                boolean expectedRecord = t.expectedAt.contains(f.record - 1);
                if (!expectedRecord && !f.ok()) otherBad++;
            }
            if (otherBad > 0) {
                failures++;
                System.out.println("FAIL " + otherBad + " unexpected finding(s) on records with no expectation");
            }
            System.out.println(failures == 0 ? "SELFTEST PASSED" : "SELFTEST FAILED (" + failures + ")");
            return failures == 0 ? 0 : 1;
        }

        byte[] fileBytes() {
            ByteArrayOutputStream all = new ByteArrayOutputStream();
            all.writeBytes(MAGIC);
            writeRec(all, new Rec(SESSION, 1_794_600_000_000_000L, "session_start", new byte[0]));
            all.writeBytes(file.toByteArray());
            return all.toByteArray();
        }

        byte[] configIn(Packet<?> p) {
            return encode(ConfigurationProtocols.CLIENTBOUND.codec(), p);
        }

        byte[] configOut(Packet<?> p) {
            return encode(ConfigurationProtocols.SERVERBOUND.codec(), p);
        }

        byte[] gameInBytes(Packet<?> p) {
            return encode(gen.gameIn.codec(), p);
        }

        byte[] gameOutBytes(Packet<?> p) {
            return encode(gen.gameOut.codec(), p);
        }

        void build() throws Exception {
            gen.startSessionQuiet();

            // Configuration. The client speaks first: options and brand go out as
            // soon as login finishes, before any configuration packet arrives.
            add(OUT, "client_information", configOut(new ServerboundClientInformationPacket(OPTIONS_DEFAULTS)));
            expect(Cat.OK);
            add(OUT, "custom_payload", configOut(new ServerboundCustomPayloadPacket(new BrandPayload("vanilla"))));
            expect(Cat.OK);
            List<KnownPack> offered = List.of(KnownPack.vanilla("core"));
            add(IN_CONFIG, "select_known_packs", configIn(new ClientboundSelectKnownPacks(offered)));
            KnownPacksManager km = new KnownPacksManager();
            List<KnownPack> selected = km.trySelectingPacks(offered);
            add(OUT, "select_known_packs", configOut(new ServerboundSelectKnownPacks(selected)));
            expect(Cat.OK);
            try (CloseableResourceManager rm = km.createResourceManager()) {
                Map<ResourceKey<?>, Map<Identifier, Integer>> indexes = new HashMap<>();
                for (RegistryDataLoader.RegistryData<?> data : RegistryDataLoader.SYNCHRONIZED_REGISTRIES) {
                    ResourceKey<? extends Registry<?>> key = (ResourceKey) data.key();
                    List<Identifier> ids = listIds(rm, key);
                    Map<Identifier, Integer> index = new HashMap<>();
                    List<RegistrySynchronization.PackedRegistryEntry> entries = new ArrayList<>();
                    for (Identifier id : ids) {
                        index.put(id, entries.size());
                        entries.add(new RegistrySynchronization.PackedRegistryEntry(id, Optional.empty()));
                    }
                    indexes.put(key, index);
                    add(IN_CONFIG, "registry_data", configIn(new ClientboundRegistryDataPacket(key, entries)));
                }
                add(IN_CONFIG, "update_tags", configIn(new ClientboundUpdateTagsPacket(buildTags(rm, indexes))));
            }
            add(IN_CONFIG, "finish_configuration", configIn(ClientboundFinishConfigurationPacket.INSTANCE));
            add(OUT, "finish_configuration", configOut(ServerboundFinishConfigurationPacket.INSTANCE));
            if (gen.registries == null) throw new IllegalStateException("configuration replay did not produce registries");

            // BAF's Event::Login resend: identical values in game state. Vanilla drops it.
            add(OUT, "client_information", gameOutBytes(new ServerboundClientInformationPacket(OPTIONS_DEFAULTS)));
            expect(Cat.LOGIN);
            ClientInformation changed = new ClientInformation("en_us", 8, ChatVisiblity.FULL, true, 0x7f, HumanoidArm.RIGHT, false, false, ParticleStatus.ALL);
            add(OUT, "client_information", gameOutBytes(new ServerboundClientInformationPacket(changed)));
            expect(Cat.LOGIN);

            // A Bazaar-like menu with Hypixel-style items.
            int win = 7;
            add(IN_GAME, "open_screen", gameInBytes(new ClientboundOpenScreenPacket(win, MenuType.GENERIC_9x6, Component.literal("Bazaar ➜ Farming"))));
            St skull = st(item("""
                    {"id":"minecraft:player_head","count":1,"components":{
                     "minecraft:custom_name":{"text":"Enchanted Hay Bale","color":"green","italic":false},
                     "minecraft:lore":[{"text":"","italic":false,"extra":[{"text":"Buy price: ","color":"gray"},{"text":"1,234.5 coins","color":"gold"}]},
                                       {"text":"Click to view!","color":"yellow","italic":false}],
                     "minecraft:profile":{"id":[1158386813,-1012970291,-1474196713,1043531146],
                       "properties":[{"name":"textures","value":"ewogICJ0aW1lc3RhbXAiIDogMTYxNzA1NjU5MDQ0OSwKICAidGV4dHVyZXMiIDogewogICAgIlNLSU4iIDogeyJ1cmwiIDogImh0dHA6Ly90ZXh0dXJlcy5taW5lY3JhZnQubmV0L3RleHR1cmUvYWJjIn0KICB9Cn0="}]},
                     "minecraft:tooltip_display":{"hidden_components":["minecraft:attribute_modifiers"]},
                     "minecraft:custom_data":{"id":"ENCHANTED_HAY_BLOCK","timestamp":1763100000000}}}
                    """));
            St carrot = st(item("""
                    {"id":"minecraft:carrot","count":64,"components":{
                     "minecraft:custom_name":{"text":"Enchanted Carrot","color":"green","italic":false},
                     "minecraft:enchantment_glint_override":true,
                     "minecraft:enchantments":{"minecraft:unbreaking":1},
                     "minecraft:lore":[{"text":"Sell price: 512.3 coins","color":"gray","italic":false}]}}
                    """));
            St boots = st(item("""
                    {"id":"minecraft:leather_boots","count":1,"components":{
                     "minecraft:dyed_color":16750848,
                     "minecraft:attribute_modifiers":[{"type":"minecraft:armor","id":"minecraft:armor.boots","amount":1.0,"operation":"add_value","slot":"feet"}],
                     "minecraft:unbreakable":{},
                     "minecraft:custom_name":{"text":"Farmer Boots","color":"gold","italic":false}}}
                    """));
            St potion = st(item("""
                    {"id":"minecraft:potion","count":1,"components":{
                     "minecraft:potion_contents":{"potion":"minecraft:swiftness","custom_color":3381504},
                     "minecraft:custom_name":{"text":"Speed Potion","color":"aqua","italic":false}}}
                    """));
            St star = st(item("""
                    {"id":"minecraft:nether_star","count":1,"components":{
                     "minecraft:custom_name":{"text":"SkyBlock Menu (Click)","color":"green","italic":false}}}
                    """));
            St close = st(item("""
                    {"id":"minecraft:barrier","count":1,"components":{"minecraft:custom_name":{"text":"Close","color":"red","italic":false}}}
                    """));
            // Wheat as Hypixel may put it on the wire: the real components plus
            // entries a vanilla client normalizes away (default-equal values and a
            // removal of a component wheat does not have).
            St plainWheat = st(item("""
                    {"id":"minecraft:wheat","count":3,"components":{
                     "minecraft:custom_name":{"text":"Wheat","color":"white","italic":false},
                     "minecraft:lore":[{"text":"Buy price: 2.4 coins","color":"gray","italic":false}]}}
                    """));
            DataComponentPatch.Builder redundant = DataComponentPatch.builder();
            for (TypedDataComponent<?> c : plainWheat.stack().getComponentsPatch().split().added()) copy(redundant, c);
            redundant.set(DataComponents.MAX_STACK_SIZE, 64);
            redundant.set(DataComponents.REPAIR_COST, 0);
            redundant.set(DataComponents.RARITY, Rarity.COMMON);
            var defaultEnchantments = plainWheat.stack().getPrototype().get(DataComponents.ENCHANTMENTS);
            if (defaultEnchantments != null) redundant.set(DataComponents.ENCHANTMENTS, defaultEnchantments);
            redundant.remove(DataComponents.FOOD);
            DataComponentPatch wheatWire = redundant.build();
            St wheat = new St(new ItemStack(plainWheat.stack().typeHolder(), 3, wheatWire), wheatWire);
            if (wheat.stack().getComponentsPatch().size() != plainWheat.stack().getComponentsPatch().size()) {
                throw new IllegalStateException("vanilla did not normalize the redundant wheat patch");
            }

            St[] slots = new St[90];
            Arrays.fill(slots, St.EMPTY);
            slots[10] = skull;
            slots[11] = carrot;
            slots[12] = boots;
            slots[13] = wheat;
            slots[14] = potion;
            slots[49] = close;
            slots[54 + 27] = star; // hotbar 0, where Hypixel keeps the SkyBlock menu
            int state = 1;

            // The manual wire encoder must produce exactly vanilla's bytes when nothing is redundant.
            St[] plain = slots.clone();
            plain[13] = plainWheat;
            byte[] manual = rawContent(win, state, plain, St.EMPTY);
            byte[] codec = gameInBytes(new ClientboundContainerSetContentPacket(win, state, stacks(plain), ItemStack.EMPTY));
            if (!Arrays.equals(manual, codec)) throw new IllegalStateException("manual set_content encoder disagrees with vanilla");

            add(IN_GAME, "container_set_content", rawContent(win, state, slots, St.EMPTY));
            HashedPatchMap.HashGenerator h = gen.hasher;

            // A. vanilla pickup of the skull button
            add(OUT, "container_click", click(win, state, 10, 0, ContainerInput.PICKUP, map(10, HashedStack.EMPTY), HashedStack.create(skull.stack(), h)));
            expect(Cat.OK);
            state = restore(win, state, slots);

            // B. pickup of the wheat with the hash built from the wire patch (azalea before #312)
            HashedStack wheatAzalea = new HashedStack.ActualItem(wheat.stack().typeHolder(), 3, HashedPatchMap.create(wheatWire, h));
            add(OUT, "container_click", click(win, state, 13, 0, ContainerInput.PICKUP, map(13, HashedStack.EMPTY), wheatAzalea));
            expect(Cat.RAW_PATCH);
            state = restore(win, state, slots);

            // C. the same pickup hashed after vanilla normalization (BAF after #312)
            add(OUT, "container_click", click(win, state, 13, 0, ContainerInput.PICKUP, map(13, HashedStack.EMPTY), HashedStack.create(wheat.stack(), h)));
            expect(Cat.OK);
            state = restore(win, state, slots);

            // D. a click carrying the previous state id
            add(OUT, "container_click", click(win, state - 1, 14, 0, ContainerInput.PICKUP, map(14, HashedStack.EMPTY), HashedStack.create(potion.stack(), h)));
            expect(Cat.STATE_ID);
            state = restore(win, state, slots);

            // E. one component hashed differently (a CRC port that disagrees with Mojang's)
            HashedStack.ActualItem good = (HashedStack.ActualItem) HashedStack.create(boots.stack(), h);
            Map<DataComponentType<?>, Integer> added = new HashMap<>(good.components().addedComponents());
            added.put(DataComponents.DYED_COLOR, added.get(DataComponents.DYED_COLOR) ^ 0x5a5a5a5a);
            HashedStack badBoots = new HashedStack.ActualItem(good.item(), good.count(), new HashedPatchMap(added, good.components().removedComponents()));
            add(OUT, "container_click", click(win, state, 12, 0, ContainerInput.PICKUP, map(12, HashedStack.EMPTY), badBoots));
            expect(Cat.HASH);
            state = restore(win, state, slots);

            // F. right click takes half of the carrots
            add(OUT, "container_click", click(win, state, 11, 1, ContainerInput.PICKUP,
                    map(11, HashedStack.create(carrot.stack().copyWithCount(32), h)), HashedStack.create(carrot.stack().copyWithCount(32), h)));
            expect(Cat.OK);
            state = restore(win, state, slots);

            // G. a click for the next window before it opened (pre-click), then that window opens
            int next = win + 1;
            add(OUT, "container_click", click(next, 0, 13, 0, ContainerInput.PICKUP, map(13, HashedStack.EMPTY), HashedStack.create(wheat.stack(), h)));
            expect(Cat.NOT_OPEN);
            add(IN_GAME, "open_screen", gameInBytes(new ClientboundOpenScreenPacket(next, MenuType.GENERIC_9x6, Component.literal("Farming ➜ Carrot"))));
            state = 1;
            add(IN_GAME, "container_set_content", rawContent(next, state, slots, St.EMPTY));

            // H. vanilla shift-click: the carrots go to the last free player slot (hotbar 8 = slot 89)
            Int2ObjectMap<HashedStack> moved = new Int2ObjectOpenHashMap<>();
            moved.put(11, HashedStack.EMPTY);
            moved.put(89, HashedStack.create(carrot.stack(), h));
            add(OUT, "container_click", click(next, state, 11, 0, ContainerInput.QUICK_MOVE, moved, HashedStack.EMPTY));
            expect(Cat.OK);
            state = restore(next, state, slots);

            // I. the same shift-click with the changed-slot entries in the other order
            Int2ObjectLinkedOpenHashMap<HashedStack> reversed = new Int2ObjectLinkedOpenHashMap<>();
            List<Integer> order = new ArrayList<>(moved.keySet());
            java.util.Collections.reverse(order);
            for (int k : order) reversed.put(k, moved.get(k));
            add(OUT, "container_click", click(next, state, 11, 0, ContainerInput.QUICK_MOVE, reversed, HashedStack.EMPTY));
            expect(Cat.ORDER);
            state = restore(next, state, slots);

            // J. a click on a slot past the end of the menu
            add(OUT, "container_click", click(next, state, 95, 0, ContainerInput.PICKUP, new Int2ObjectOpenHashMap<>(), HashedStack.EMPTY));
            expect(Cat.OUT_OF_RANGE);

            // K. closing the window
            add(OUT, "container_close", gameOutBytes(new ServerboundContainerClosePacket(next)));
            expect(Cat.OK);
        }

        static <T> void copy(DataComponentPatch.Builder b, TypedDataComponent<T> c) {
            b.set(c.type(), c.value());
        }

        int restore(int win, int state, St[] slots) {
            int next = state + 1;
            add(IN_GAME, "container_set_content", rawContent(win, next, slots, St.EMPTY));
            add(IN_GAME, "set_cursor_item", gameInBytes(new ClientboundSetCursorItemPacket(ItemStack.EMPTY)));
            return next;
        }

        static Int2ObjectMap<HashedStack> map(int slot, HashedStack h) {
            Int2ObjectMap<HashedStack> m = new Int2ObjectOpenHashMap<>();
            m.put(slot, h);
            return m;
        }

        byte[] click(int win, int state, int slot, int button, ContainerInput input, Int2ObjectMap<HashedStack> changed, HashedStack carried) {
            return gameOutBytes(new ServerboundContainerClickPacket(win, state, (short) slot, (byte) button, input, changed, carried));
        }

        static List<ItemStack> stacks(St[] slots) {
            List<ItemStack> out = new ArrayList<>();
            for (St s : slots) out.add(s.stack());
            return out;
        }

        ItemStack item(String json) {
            return ItemStack.CODEC.parse(gen.jsonOps, JsonParser.parseString(json)).getOrThrow();
        }

        static St st(ItemStack s) {
            return new St(s, s.getComponentsPatch());
        }

        /** container_set_content with each item's wire patch written verbatim. */
        byte[] rawContent(int win, int state, St[] slots, St carried) {
            byte[] probe = gameInBytes(new ClientboundContainerSetContentPacket(0, 0, List.of(), ItemStack.EMPTY));
            int packetId = new RegistryFriendlyByteBuf(Unpooled.wrappedBuffer(probe), gen.registries).readVarInt();
            RegistryFriendlyByteBuf b = new RegistryFriendlyByteBuf(Unpooled.buffer(), gen.registries);
            b.writeVarInt(packetId);
            b.writeContainerId(win);
            b.writeVarInt(state);
            b.writeVarInt(slots.length);
            for (St s : slots) writeWireItem(b, s);
            writeWireItem(b, carried);
            return ByteBufUtil.getBytes(b);
        }

        static void writeWireItem(RegistryFriendlyByteBuf b, St s) {
            if (s.isEmpty()) {
                b.writeVarInt(0);
                return;
            }
            b.writeVarInt(s.count());
            Item.STREAM_CODEC.encode(b, s.stack().typeHolder());
            DataComponentPatch.STREAM_CODEC.encode(b, s.wire());
        }

        static List<Identifier> listIds(ResourceManager rm, ResourceKey<? extends Registry<?>> key) {
            String dir = Registries.elementsDirPath(key);
            TreeMap<String, Identifier> sorted = new TreeMap<>();
            rm.listResources(dir, id -> id.getPath().endsWith(".json")).keySet().forEach(file -> {
                String path = file.getPath();
                Identifier id = Identifier.fromNamespaceAndPath(file.getNamespace(), path.substring(dir.length() + 1, path.length() - ".json".length()));
                sorted.put(id.toString(), id);
            });
            return new ArrayList<>(sorted.values());
        }

        static Map<ResourceKey<? extends Registry<?>>, TagNetworkSerialization.NetworkPayload> buildTags(
                ResourceManager rm, Map<ResourceKey<?>, Map<Identifier, Integer>> synced) {
            Map<ResourceKey<? extends Registry<?>>, TagNetworkSerialization.NetworkPayload> out = new HashMap<>();
            synced.forEach((key, index) -> {
                TagNetworkSerialization.NetworkPayload p = payload(rm, (ResourceKey) key, id -> Optional.ofNullable(index.get(id)));
                if (p != null) out.put((ResourceKey) key, p);
            });
            for (Registry<?> registry : BuiltInRegistries.REGISTRY) {
                TagNetworkSerialization.NetworkPayload p = payload(rm, (ResourceKey) registry.key(), id -> staticId(registry, id));
                if (p != null) out.put((ResourceKey) registry.key(), p);
            }
            return out;
        }

        static <T> Optional<Integer> staticId(Registry<T> registry, Identifier id) {
            return registry.getOptional(id).map(registry::getId);
        }

        static TagNetworkSerialization.NetworkPayload payload(ResourceManager rm, ResourceKey<? extends Registry<?>> key, Function<Identifier, Optional<Integer>> ids) {
            TagLoader<Integer> loader = new TagLoader<>((id, required) -> ids.apply(id), Registries.tagsDirPath(key));
            Map<Identifier, List<Integer>> built = loader.build(loader.load(rm));
            if (built.isEmpty()) return null;
            Map<Identifier, IntList> tags = new HashMap<>();
            built.forEach((tag, values) -> tags.put(tag, new IntArrayList(values)));
            return new TagNetworkSerialization.NetworkPayload(tags);
        }
    }

    void startSessionQuiet() {
        inConfig = false;
        registries = null;
    }
}
