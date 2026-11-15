//! Vanilla's known-packs handshake.
//!
//! During configuration the server offers the data packs it shares with the
//! client. A vanilla 26.3 client (`KnownPacksManager.trySelectingPacks`)
//! answers with every offered pack it ships, in the offered order, and the
//! server then sends the registry entries from those packs without data; the
//! client loads that data from its own jar. Hypixel offers
//! `minecraft:core:26.3`, which every vanilla 26.3 client selects.
//!
//! Upstream azalea answered with an empty list, a reply no vanilla client
//! sends. To answer like vanilla we need the data vanilla would load, so it is
//! embedded here: `vanilla_core_26_3_registries.bin` is the output of Mojang's
//! own `RegistrySynchronization.packRegistries` over the 26.3 `minecraft:core`
//! pack, generated with `tools/vanilla-oracle/run.sh --core-registries`.
//!
//! File format: repeated `u32 LE length | ClientboundRegistryData body`
//! (registry id, then the entry list, no packet id), every entry with data.

use std::{io::Cursor, sync::LazyLock};

use azalea_buf::AzBuf;
use azalea_protocol::packets::config::{ClientboundRegistryData, s_select_known_packs::KnownPack};
use azalea_registry::identifier::Identifier;
use indexmap::IndexMap;
use simdnbt::owned::{NbtList, NbtTag};
use tracing::error;

const CORE_REGISTRIES: &[u8] = include_bytes!("vanilla_core_26_3_registries.bin");

/// The `minecraft:core` version a vanilla 26.3 client ships.
pub const CORE_VERSION: &str = "26.3";

/// Registry id to entry id to entry data, in file order.
pub type Registries = IndexMap<Identifier, IndexMap<Identifier, NbtTag>>;

static CORE: LazyLock<Option<Registries>> = LazyLock::new(|| match decode(CORE_REGISTRIES) {
    Ok(registries) => Some(registries),
    Err(err) => {
        error!("Embedded vanilla core registries do not decode, known packs disabled: {err}");
        None
    }
});

/// The embedded vanilla `minecraft:core` registry data, `None` if it failed to
/// decode (then [`select`] answers with no packs, like upstream azalea).
pub fn core() -> Option<&'static Registries> {
    CORE.as_ref()
}

fn is_core(pack: &KnownPack) -> bool {
    pack.namespace == "minecraft" && pack.id == "core" && pack.version == CORE_VERSION
}

/// The reply to `select_known_packs`: the offered packs we can fill, in the
/// offered order.
///
/// Vanilla also ships `minecart_improvements`, `redstone_experiments` and
/// `trade_rebalance`. Hypixel never offers them; if a server did, we leave
/// them out because we have no data to fill their entries with.
pub fn select(offered: &[KnownPack]) -> Vec<KnownPack> {
    if core().is_none() {
        return Vec::new();
    }
    offered
        .iter()
        .filter(|pack| is_core(pack))
        .cloned()
        .collect()
}

/// Fills every entry the server sent without data with the vanilla core data,
/// like the vanilla client loading it from its jar. Entries are never
/// reordered, so protocol ids stay what the server meant.
///
/// Returns the ids that came without data and are not in the core pack.
pub fn fill_from_core(
    registry: &Identifier,
    entries: &mut [(Identifier, Option<NbtTag>)],
) -> Vec<Identifier> {
    let known = core().and_then(|core| core.get(registry));
    let mut missing = Vec::new();
    for (id, data) in entries.iter_mut() {
        if data.is_some() {
            continue;
        }
        match known.and_then(|known| known.get(id)) {
            Some(tag) => *data = Some(tag.clone()),
            None => missing.push(id.clone()),
        }
    }
    missing
}

/// Decodes the embedded file format.
pub fn decode(mut bytes: &[u8]) -> Result<Registries, String> {
    let mut registries = Registries::new();
    while !bytes.is_empty() {
        let Some((len, rest)) = bytes.split_first_chunk::<4>() else {
            return Err(format!("truncated length, {} bytes left", bytes.len()));
        };
        let len = u32::from_le_bytes(*len) as usize;
        if rest.len() < len {
            return Err(format!("record of {len} bytes, only {} left", rest.len()));
        }
        let (record, rest) = rest.split_at(len);
        bytes = rest;

        let mut cursor = Cursor::new(record);
        let packet = ClientboundRegistryData::azalea_read(&mut cursor)
            .map_err(|err| format!("registry record does not decode: {err:?}"))?;
        if cursor.position() as usize != record.len() {
            return Err(format!(
                "{} has {} trailing bytes",
                packet.registry_id,
                record.len() - cursor.position() as usize
            ));
        }

        let mut entries = IndexMap::with_capacity(packet.entries.len());
        for (id, data) in packet.entries {
            let Some(mut data) = data else {
                return Err(format!("{} {id} has no data", packet.registry_id));
            };
            int_arrays_to_lists(&mut data);
            entries.insert(id, data);
        }
        registries.insert(packet.registry_id, entries);
    }
    Ok(registries)
}

/// Vanilla's NBT codec writes `Vec3i` offsets (frost walker, block
/// transformers) as int arrays, Hypixel writes them as int lists, and
/// azalea's `Vec3i` only reads lists. An enchantment that fails to parse is
/// dropped by `RegistryType::append_nbt`, which would shift every later
/// enchantment's protocol id, so convert to the form azalea reads.
fn int_arrays_to_lists(tag: &mut NbtTag) {
    match tag {
        NbtTag::IntArray(values) => {
            let values = std::mem::take(values);
            *tag = NbtTag::List(NbtList::Int(values));
        }
        NbtTag::Compound(compound) => compound.values_mut().for_each(int_arrays_to_lists),
        NbtTag::List(list) => list_int_arrays_to_lists(list),
        _ => {}
    }
}

fn list_int_arrays_to_lists(list: &mut NbtList) {
    match list {
        NbtList::IntArray(arrays) => {
            let arrays = std::mem::take(arrays);
            *list = NbtList::List(arrays.into_iter().map(NbtList::Int).collect());
        }
        NbtList::Compound(compounds) => {
            for compound in compounds {
                compound.values_mut().for_each(int_arrays_to_lists);
            }
        }
        NbtList::List(lists) => lists.iter_mut().for_each(list_int_arrays_to_lists),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(namespace: &str, id: &str, version: &str) -> KnownPack {
        KnownPack {
            namespace: namespace.to_owned(),
            id: id.to_owned(),
            version: version.to_owned(),
        }
    }

    #[test]
    fn embedded_core_decodes() {
        let core = core().expect("embedded core decodes");
        assert_eq!(core.len(), 32);
        assert_eq!(core.values().map(IndexMap::len).sum::<usize>(), 432);
    }

    #[test]
    fn selects_only_core_at_our_version() {
        let offered = [
            pack("minecraft", "core", "26.3"),
            pack("minecraft", "core", "26.2"),
            pack("minecraft", "trade_rebalance", "26.3"),
            pack("hypixel", "core", "26.3"),
        ];
        assert_eq!(select(&offered), vec![pack("minecraft", "core", "26.3")]);
        assert!(select(&[]).is_empty());
    }

    #[test]
    fn fills_only_entries_without_data() {
        let registry = Identifier::new("minecraft:dimension_type");
        let own = NbtTag::Int(7);
        let mut entries = vec![
            (Identifier::new("minecraft:overworld"), None),
            (Identifier::new("hypixel:custom"), None),
            (Identifier::new("minecraft:the_nether"), Some(own.clone())),
        ];
        let missing = fill_from_core(&registry, &mut entries);
        assert_eq!(missing, vec![Identifier::new("hypixel:custom")]);
        assert!(matches!(entries[0].1, Some(NbtTag::Compound(_))));
        assert_eq!(entries[1].1, None);
        assert_eq!(entries[2].1, Some(own));
    }

    #[test]
    fn int_arrays_become_int_lists() {
        let mut tag = NbtTag::List(NbtList::IntArray(vec![vec![0, -1, 0]]));
        int_arrays_to_lists(&mut tag);
        assert_eq!(
            tag,
            NbtTag::List(NbtList::List(vec![NbtList::Int(vec![0, -1, 0])]))
        );
    }
}
