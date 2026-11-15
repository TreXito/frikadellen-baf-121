//! The vendored azalea-client answers `select_known_packs` with
//! `minecraft:core:26.3` and fills the entries Hypixel then sends without data
//! from embedded vanilla data. Every filled entry has to parse in azalea:
//! `RegistryType::append_nbt` drops an entry that fails and every later
//! protocol id of that registry shifts. CI only runs this crate's tests, so the
//! check lives here rather than in the vendored crate.

use azalea::{core::registry_holder::RegistryHolder, Identifier};
use azalea_client::known_packs;
use azalea_protocol::simdnbt::owned::NbtTag;

#[test]
fn every_embedded_core_entry_parses_in_azalea() {
    let core = known_packs::core().expect("embedded vanilla core registries decode");
    assert_eq!(core.len(), 32);

    let mut holder = RegistryHolder::default();
    for (registry, entries) in core {
        // what Hypixel sends once we select the core pack: ids, no data
        let mut sent: Vec<(Identifier, Option<NbtTag>)> =
            entries.keys().map(|id| (id.clone(), None)).collect();
        let missing = known_packs::fill_from_core(registry, &mut sent);
        assert!(missing.is_empty(), "{registry}: {missing:?}");
        holder.append(
            registry.clone(),
            sent.into_iter()
                .map(|(id, data)| (id, data.map(Into::into)))
                .collect(),
        );
    }

    let enchantments = &core[&Identifier::new("minecraft:enchantment")];
    assert_eq!(holder.enchantment.map.len(), enchantments.len());
    // protocol ids follow the order the entries were sent in
    for (index, id) in enchantments.keys().enumerate() {
        assert_eq!(
            holder.enchantment.map.get_index(index).map(|(k, _)| k),
            Some(id)
        );
    }

    let dimensions = &core[&Identifier::new("minecraft:dimension_type")];
    assert_eq!(holder.dimension_type.map.len(), dimensions.len());
    let overworld = &holder.dimension_type.map[&Identifier::new("minecraft:overworld")];
    assert_eq!(overworld.min_y, -64);
    assert_eq!(overworld.height, 384);
}

#[test]
fn only_core_at_the_client_version_is_selected() {
    use azalea_protocol::packets::config::s_select_known_packs::KnownPack;
    let pack = |id: &str, version: &str| KnownPack {
        namespace: "minecraft".to_owned(),
        id: id.to_owned(),
        version: version.to_owned(),
    };
    let selected = known_packs::select(&[pack("core", "26.3"), pack("core", "26.2")]);
    assert_eq!(selected, vec![pack("core", "26.3")]);
}
