//! Hash item stacks for `ServerboundContainerClick` the way a vanilla client does.
//!
//! A vanilla client never keeps an item's component patch as it came off the
//! wire. Decoding a stack builds `PatchedDataComponentMap.fromPatch(prototype,
//! patch)`, and `applyPatch` drops two kinds of entry:
//!
//! * an added component whose value equals the item's prototype default,
//! * a removal of a component the prototype does not have.
//!
//! `HashedStack.create` later hashes that normalized patch, so neither kind of
//! entry ever reaches the server in a click.
//!
//! azalea stores the raw wire patch (its `DataComponentPatch` docs note it does
//! not implement prototype defaults), and `HashedStack::from_item_stack` hashes
//! every entry. Whenever the server's item encoder puts a redundant entry on the
//! wire, every click that predicts that item onto the cursor reports a component
//! set no vanilla client can produce. The server's `HashedPatchMap.matches` fails
//! on the size check before it compares a single hash. Menus whose button items
//! carry such entries (Bazaar more than AH) flag on those buttons only.

use std::sync::atomic::{AtomicU64, Ordering};

use azalea::core::registry_holder::RegistryHolder;
use azalea::registry::builtin::{DataComponentKind, ItemKind};
use azalea_inventory::{
    components::{self, EncodableDataComponent},
    default_components::get_default_component,
    ItemStack, ItemStackData,
};
use azalea_protocol::packets::game::s_container_click::{
    HashedActualItem, HashedPatchMap, HashedStack,
};
use tracing::debug;

/// Hashed stacks whose wire patch held entries a vanilla client drops.
static STACKS_NORMALIZED: AtomicU64 = AtomicU64::new(0);

/// How many hashed stacks needed normalizing this session. Non-zero means the
/// old `HashedStack::from_item_stack` path would have sent a non-vanilla patch.
pub(crate) fn stacks_normalized() -> u64 {
    STACKS_NORMALIZED.load(Ordering::Relaxed)
}

#[derive(Debug, PartialEq, Eq)]
enum Prototype {
    /// The item has no default for this component.
    Absent,
    /// The patch value equals the item's default.
    Same,
    /// The item has a default and the patch value differs, or removes it.
    Differs,
}

/// Exhaustive over every component azalea knows. A component added by an
/// azalea bump fails to compile here instead of silently hashing raw.
macro_rules! compare_with_prototype {
    ($kind:expr, $item:expr, $value:expr; $($t:ident),* $(,)?) => {
        match $kind {
            $(DataComponentKind::$t => match get_default_component::<components::$t>($item) {
                None => Prototype::Absent,
                Some(default) => match $value {
                    Some(value) if EncodableDataComponent::eq(value, &default) => Prototype::Same,
                    _ => Prototype::Differs,
                },
            },)*
        }
    };
}

fn prototype(
    kind: DataComponentKind,
    item: ItemKind,
    value: Option<&dyn EncodableDataComponent>,
) -> Prototype {
    compare_with_prototype!(kind, item, value;
        CustomData, MaxStackSize, MaxDamage, Damage, Unbreakable, CustomName, ItemName,
        ItemModel, Lore, Rarity, Enchantments, CanPlaceOn, CanBreak, AttributeModifiers,
        CustomModelData, TooltipDisplay, RepairCost, CreativeSlotLock,
        EnchantmentGlintOverride, IntangibleProjectile, Food, Consumable, UseRemainder,
        UseCooldown, DamageResistant, Tool, Weapon, Enchantable, Equippable, Repairable,
        Glider, TooltipStyle, DeathProtection, BlocksAttacks, StoredEnchantments,
        DyedColor, MapId, MapDecorations, MapPostProcessing, ChargedProjectiles,
        BundleContents, PotionContents, PotionDurationScale, SuspiciousStewEffects,
        WritableBookContent, WrittenBookContent, Trim, DebugStickState, EntityData,
        BucketEntityData, BlockEntityData, Instrument, ProvidesTrimMaterial,
        OminousBottleAmplifier, JukeboxPlayable, ProvidesBannerPatterns, Recipes,
        LodestoneTracker, FireworkExplosion, Fireworks, Profile, NoteBlockSound,
        BannerPatterns, BaseColor, PotDecorations, Container, BlockState, Bees, Lock,
        ContainerLoot, BreakSound, VillagerVariant, WolfVariant, WolfSoundVariant,
        WolfCollar, FoxVariant, SalmonSize, ParrotVariant, TropicalFishPattern,
        TropicalFishBaseColor, TropicalFishPatternColor, MooshroomVariant, RabbitVariant,
        PigVariant, CowVariant, ChickenVariant, FrogVariant, HorseVariant, PaintingVariant,
        LlamaVariant, AxolotlVariant, CatVariant, CatCollar, SheepColor, ShulkerColor,
        UseEffects, MinimumAttackCharge, DamageType, PiercingWeapon, KineticWeapon,
        ZombieNautilusVariant, AttackRange, AdditionalTradeCost, Dye, PigSoundVariant,
        CowSoundVariant, ChickenSoundVariant, CatSoundVariant, SulfurCubeContent,
        BlockTransformer, ProvidesPotteryPattern, AttackAnimation, InteractAnimation,
        VillagerFood, Compostable, CookingFuel, BrewingFuel, MobVisibility, SignTextFront,
        SignTextBack, Waxed, CushionColor,
    )
}

/// The patch a vanilla client holds for this stack after decoding it.
struct VanillaPatch<'a> {
    added: Vec<(DataComponentKind, &'a dyn EncodableDataComponent)>,
    removed: Vec<DataComponentKind>,
    /// Wire entries `PatchedDataComponentMap.applyPatch` would have dropped.
    stripped: Vec<DataComponentKind>,
}

fn vanilla_patch(item: &ItemStackData) -> VanillaPatch<'_> {
    let mut patch = VanillaPatch {
        added: Vec::new(),
        removed: Vec::new(),
        stripped: Vec::new(),
    };
    for (kind, value) in item.component_patch.iter() {
        match (value, prototype(kind, item.kind, value)) {
            (Some(_), Prototype::Same) | (None, Prototype::Absent) => patch.stripped.push(kind),
            (Some(value), _) => patch.added.push((kind, value)),
            (None, _) => patch.removed.push(kind),
        }
    }
    patch
}

/// Drop-in replacement for `HashedStack::from_item_stack` that hashes the
/// normalized patch a vanilla client would hash.
pub(crate) fn hashed_stack(item: &ItemStack, registries: &RegistryHolder) -> HashedStack {
    let ItemStack::Present(item) = item else {
        return HashedStack(None);
    };
    let patch = vanilla_patch(item);
    if !patch.stripped.is_empty() {
        STACKS_NORMALIZED.fetch_add(1, Ordering::Relaxed);
        debug!(
            "Hashed {:?} without {:?}: vanilla drops patch entries equal to the item default",
            item.kind, patch.stripped
        );
    }
    HashedStack(Some(HashedActualItem {
        kind: item.kind,
        count: item.count,
        components: HashedPatchMap {
            added_components: patch
                .added
                .into_iter()
                .map(|(kind, value)| (kind, value.crc_hash(registries)))
                .collect(),
            removed_components: patch.removed,
        },
    }))
}

#[cfg(test)]
mod tests {
    use azalea::FormattedText;
    use azalea_inventory::components::{
        CustomName, DataComponentUnion, EnchantmentGlintOverride, Lore, MaxStackSize,
    };

    use super::*;

    fn stack(
        kind: ItemKind,
        entries: Vec<(DataComponentKind, Option<DataComponentUnion>)>,
    ) -> ItemStackData {
        let mut data = ItemStackData::new(kind, 1);
        for (component, value) in entries {
            // SAFETY: every value below is built from the component type its kind names.
            unsafe {
                data.component_patch
                    .unchecked_insert_component(component, value)
            };
        }
        data
    }

    fn kinds(patch: &VanillaPatch<'_>) -> (Vec<DataComponentKind>, Vec<DataComponentKind>) {
        (
            patch.added.iter().map(|(k, _)| *k).collect(),
            patch.removed.clone(),
        )
    }

    #[test]
    fn added_components_equal_to_the_default_are_stripped() {
        let item = stack(
            ItemKind::Stone,
            vec![
                (DataComponentKind::Lore, Some(Lore { lines: vec![] }.into())),
                (
                    DataComponentKind::MaxStackSize,
                    Some(MaxStackSize { count: 64 }.into()),
                ),
            ],
        );
        let patch = vanilla_patch(&item);
        assert_eq!(kinds(&patch), (vec![], vec![]));
        assert_eq!(
            patch.stripped,
            vec![DataComponentKind::Lore, DataComponentKind::MaxStackSize]
        );
    }

    #[test]
    fn added_components_that_differ_from_the_default_are_kept() {
        let item = stack(
            ItemKind::Stone,
            vec![
                (
                    DataComponentKind::MaxStackSize,
                    Some(MaxStackSize { count: 1 }.into()),
                ),
                (
                    DataComponentKind::CustomName,
                    Some(
                        CustomName {
                            name: FormattedText::from("Buy Order"),
                        }
                        .into(),
                    ),
                ),
                (
                    DataComponentKind::Lore,
                    Some(
                        Lore {
                            lines: vec![FormattedText::from("Click to buy")],
                        }
                        .into(),
                    ),
                ),
            ],
        );
        let patch = vanilla_patch(&item);
        assert!(patch.stripped.is_empty());
        assert_eq!(
            kinds(&patch).0,
            vec![
                DataComponentKind::MaxStackSize,
                DataComponentKind::CustomName,
                DataComponentKind::Lore
            ]
        );
    }

    #[test]
    fn glint_override_is_stripped_only_where_the_item_already_glints() {
        let glint = || Some(EnchantmentGlintOverride { show_glint: true }.into());
        let book_stack = stack(
            ItemKind::EnchantedBook,
            vec![(DataComponentKind::EnchantmentGlintOverride, glint())],
        );
        let book = vanilla_patch(&book_stack);
        assert_eq!(
            book.stripped,
            vec![DataComponentKind::EnchantmentGlintOverride]
        );

        let diamond_stack = stack(
            ItemKind::Diamond,
            vec![(DataComponentKind::EnchantmentGlintOverride, glint())],
        );
        let diamond = vanilla_patch(&diamond_stack);
        assert!(diamond.stripped.is_empty());
        assert_eq!(
            kinds(&diamond).0,
            vec![DataComponentKind::EnchantmentGlintOverride]
        );
    }

    #[test]
    fn removals_survive_only_when_the_prototype_has_the_component() {
        let item = stack(
            ItemKind::Stone,
            vec![
                // Stone has no default custom_data: vanilla drops the removal.
                (DataComponentKind::CustomData, None),
                // Every item has a default lore: removing it is a real patch entry.
                (DataComponentKind::Lore, None),
            ],
        );
        let patch = vanilla_patch(&item);
        assert_eq!(patch.stripped, vec![DataComponentKind::CustomData]);
        assert_eq!(kinds(&patch), (vec![], vec![DataComponentKind::Lore]));
    }

    #[test]
    fn hashed_stack_omits_stripped_entries() {
        let registries = RegistryHolder::default();
        let item = ItemStack::Present(stack(
            ItemKind::Stone,
            vec![
                (DataComponentKind::Lore, Some(Lore { lines: vec![] }.into())),
                (DataComponentKind::CustomData, None),
                (
                    DataComponentKind::MaxStackSize,
                    Some(MaxStackSize { count: 16 }.into()),
                ),
            ],
        ));
        let HashedStack(Some(hashed)) = hashed_stack(&item, &registries) else {
            panic!("present stack hashed to empty");
        };
        let added: Vec<_> = hashed
            .components
            .added_components
            .iter()
            .map(|(k, _)| *k)
            .collect();
        assert_eq!(added, vec![DataComponentKind::MaxStackSize]);
        assert!(hashed.components.removed_components.is_empty());

        // Same stack without the redundant entries hashes identically to azalea's
        // raw path, which is what a vanilla client would have sent.
        let clean = ItemStack::Present(stack(
            ItemKind::Stone,
            vec![(
                DataComponentKind::MaxStackSize,
                Some(MaxStackSize { count: 16 }.into()),
            )],
        ));
        assert_eq!(
            HashedStack(Some(hashed)),
            HashedStack::from_item_stack(&clean, &registries)
        );
        assert_eq!(
            hashed_stack(&ItemStack::Empty, &registries),
            HashedStack(None)
        );
    }
}
