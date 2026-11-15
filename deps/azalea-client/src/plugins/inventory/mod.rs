pub mod equipment_effects;

use azalea_chat::FormattedText;
use azalea_core::tick::GameTick;
use azalea_entity::{PlayerAbilities, inventory::Inventory as Inv};
use azalea_inventory::operations::ClickOperation;
pub use azalea_inventory::*;
use azalea_protocol::packets::game::{
    s_container_click::{HashedStack, ServerboundContainerClick},
    s_container_close::ServerboundContainerClose,
    s_set_carried_item::ServerboundSetCarriedItem,
};
use azalea_registry::builtin::MenuKind;
use azalea_world::{WorldName, Worlds};
use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;
use indexmap::IndexMap;
use tracing::{error, warn};

use crate::{
    inventory::equipment_effects::{collect_equipment_changes, handle_equipment_changes},
    packet::game::SendGamePacketEvent,
};

// TODO: when this is removed, remove the Inv alias above (which just exists to
// avoid conflicting with this pub deprecated type)
#[doc(hidden)]
#[deprecated = "moved to `azalea_entity::inventory::Inventory`."]
pub type Inventory = azalea_entity::inventory::Inventory;

/// The click results a vanilla client is still showing for the open menu.
///
/// Vanilla applies a container click to its own menu before it sends the
/// packet, and keeps that guess until the server overwrites the slot
/// (`container_set_slot`, `container_set_content`) or the cursor
/// (`set_cursor_item`, `container_set_content`), or a new menu replaces it.
/// [`Inv`] holds only what the server sent. This holds the guesses on top of
/// it, so a second click before the server answers reports what vanilla would
/// report, and nothing else in the client sees items that have not moved.
#[derive(Clone, Component, Debug, Default)]
pub struct ClickPrediction {
    /// Predicted contents of open-menu slots, by menu index, that the server
    /// has not overwritten since.
    pub slots: std::collections::HashMap<u16, ItemStack>,
    /// Predicted cursor. `None` means the server's [`Inv::carried`] stands.
    pub carried: Option<ItemStack>,
}

impl ClickPrediction {
    /// The menu and cursor a vanilla client would show: the server's state
    /// with the pending guesses on top.
    pub fn view(&self, inventory: &Inv) -> Inv {
        let mut view = inventory.clone();
        let menu = view.menu_mut();
        for (&slot, item) in &self.slots {
            if let Some(slot_mut) = menu.slot_mut(slot as usize) {
                *slot_mut = item.clone();
            }
        }
        if let Some(carried) = &self.carried {
            view.carried = carried.clone();
        }
        view
    }

    /// Drop every guess. `carried` is the cursor of the menu that is now
    /// active: `Some(ItemStack::Empty)` for a menu vanilla just created,
    /// `None` when the server's cursor is the truth again.
    pub fn reset(&mut self, carried: Option<ItemStack>) {
        self.slots.clear();
        self.carried = carried;
    }

    /// The server set one slot of the open menu.
    pub fn server_set_slot(&mut self, slot: u16) {
        self.slots.remove(&slot);
    }

    /// The server set the cursor.
    pub fn server_set_carried(&mut self) {
        self.carried = None;
    }

    /// The server changed the player's own inventory, which the open menu
    /// shows in its player section. Drop the guesses there; [`Inv`] does not
    /// map those updates slot by slot either.
    pub fn server_set_player_slots(&mut self, player_slots: std::ops::RangeInclusive<usize>) {
        self.slots
            .retain(|&slot, _| !player_slots.contains(&(slot as usize)));
    }
}

/// What a click a vanilla client predicted puts in the packet.
#[derive(Clone, Debug)]
pub struct PredictedClick {
    pub state_id: u32,
    /// Slots whose content changed, with the content after the click, in menu
    /// order.
    pub changed_slots: Vec<(u16, ItemStack)>,
    /// The cursor after the click.
    pub carried: ItemStack,
}

/// Apply a click the way vanilla's `MultiPlayerGameMode.handleContainerInput`
/// does before it sends the packet: simulate it on what the client currently
/// shows (server state plus pending guesses), keep the result as the new
/// guess, and return the changed slots and the cursor.
pub fn predict_click(
    inventory: &Inv,
    prediction: &mut ClickPrediction,
    operation: &ClickOperation,
    player_abilities: &PlayerAbilities,
) -> PredictedClick {
    let mut view = prediction.view(inventory);
    let before = view.menu().slots();
    view.simulate_click(operation, player_abilities);
    let after = view.menu().slots();

    let mut changed_slots = Vec::new();
    for (index, (old, new)) in before.iter().zip(after.iter()).enumerate() {
        if old != new {
            changed_slots.push((index as u16, new.clone()));
            prediction.slots.insert(index as u16, new.clone());
        }
    }
    prediction.carried = Some(view.carried.clone());

    PredictedClick {
        state_id: inventory.state_id,
        changed_slots,
        carried: view.carried,
    }
}

pub struct InventoryPlugin;
impl Plugin for InventoryPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            GameTick,
            (
                ensure_has_sent_carried_item.after(super::mining::handle_mining_queued),
                collect_equipment_changes
                    .after(super::interact::handle_start_use_item_queued)
                    .before(azalea_physics::ai_step),
            ),
        )
        .add_observer(handle_client_side_close_container_trigger)
        .add_observer(handle_menu_opened_trigger)
        .add_observer(handle_container_close_event)
        .add_observer(handle_set_container_content_trigger)
        .add_observer(handle_container_click_event)
        // number keys are checked on tick but scrolling can happen outside of ticks, therefore
        // this is fine
        .add_observer(handle_set_selected_hotbar_slot_event)
        .add_observer(handle_equipment_changes);
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, SystemSet)]
pub struct InventorySystems;

/// A Bevy trigger that's fired when our client should show a new screen (like a
/// chest or crafting table).
///
/// To watch for the menu being closed, you could use
/// [`ClientsideCloseContainerEvent`]. To close it manually, use
/// [`CloseContainerEvent`].
#[derive(Clone, Debug, EntityEvent)]
pub struct MenuOpenedEvent {
    pub entity: Entity,
    pub window_id: i32,
    pub menu_type: MenuKind,
    pub title: FormattedText,
}
fn handle_menu_opened_trigger(
    event: On<MenuOpenedEvent>,
    mut query: Query<(&mut Inv, Option<&mut ClickPrediction>)>,
) {
    let (mut inventory, prediction) = query.get_mut(event.entity).unwrap();
    // vanilla builds a fresh menu, and a fresh menu's cursor is empty
    if let Some(mut prediction) = prediction {
        prediction.reset(Some(ItemStack::Empty));
    }
    inventory.id = event.window_id;
    // a new vanilla menu starts at state id 0; the server's set_slot and
    // set_content packets move it from there
    inventory.state_id = 0;
    inventory.container_menu = Some(Menu::from_kind(event.menu_type));
    inventory.container_menu_title = Some(event.title.clone());
}

/// Tell the server that we want to close a container.
///
/// Note that this is also sent when the client closes its own inventory, even
/// though there is no packet for opening its inventory.
#[derive(EntityEvent)]
pub struct CloseContainerEvent {
    pub entity: Entity,
    /// The ID of the container to close. 0 for the player's inventory.
    ///
    /// If this is not the same as the currently open inventory, nothing will
    /// happen.
    pub id: i32,
}
fn handle_container_close_event(
    close_container: On<CloseContainerEvent>,
    mut commands: Commands,
    query: Query<(Entity, &Inv)>,
) {
    let (entity, inventory) = query.get(close_container.entity).unwrap();
    if close_container.id != inventory.id {
        warn!(
            "Tried to close container with ID {}, but the current container ID is {}",
            close_container.id, inventory.id
        );
        return;
    }

    commands.trigger(SendGamePacketEvent::new(
        entity,
        ServerboundContainerClose {
            container_id: inventory.id,
        },
    ));
    commands.trigger(ClientsideCloseContainerEvent {
        entity: close_container.entity,
    });
}

/// A Bevy event that's fired when our client closed a container.
///
/// This can also be triggered directly to close a container silently without
/// sending any packets to the server. You probably don't want that though, and
/// should instead use [`CloseContainerEvent`].
///
/// If you want to watch for a container being opened, you should use
/// [`MenuOpenedEvent`].
#[derive(Clone, EntityEvent)]
pub struct ClientsideCloseContainerEvent {
    pub entity: Entity,
}
pub fn handle_client_side_close_container_trigger(
    event: On<ClientsideCloseContainerEvent>,
    mut query: Query<(&mut Inv, Option<&mut ClickPrediction>)>,
) {
    let (mut inventory, prediction) = query.get_mut(event.entity).unwrap();
    // vanilla switches back to the inventory menu, whose cursor is empty
    if let Some(mut prediction) = prediction {
        prediction.reset(Some(ItemStack::Empty));
    }

    // copy the Player part of the container_menu to the inventory_menu
    if let Some(inventory_menu) = inventory.container_menu.take() {
        // this isn't the same as what vanilla does. i believe vanilla synchronizes the
        // slots between inventoryMenu and containerMenu by just having the player slots
        // point to the same ItemStack in memory, but emulating this in rust would
        // require us to wrap our `ItemStack`s as `Arc<Mutex<ItemStack>>` which would
        // have kinda terrible ergonomics.

        // the simpler solution i chose to go with here is to only copy the player slots
        // when the container is closed. this is perfectly fine for vanilla, but it
        // might cause issues if a server modifies id 0 while we have a container
        // open...

        // if we do encounter this issue in the wild then the simplest solution would
        // probably be to just add logic for updating the container_menu when the server
        // tries to modify id 0 for slots within `inventory`. not implemented for now
        // because i'm not sure if that's worth worrying about.

        let new_inventory = inventory_menu.slots()[inventory_menu.player_slots_range()].to_vec();
        let new_inventory = <[ItemStack; 36]>::try_from(new_inventory).unwrap();
        *inventory.inventory_menu.as_player_mut().inventory = new_inventory;
    }

    inventory.id = 0;
    inventory.container_menu_title = None;
}

#[derive(Debug, EntityEvent)]
pub struct ContainerClickEvent {
    pub entity: Entity,
    pub window_id: i32,
    pub operation: ClickOperation,
}
pub fn handle_container_click_event(
    container_click: On<ContainerClickEvent>,
    mut commands: Commands,
    mut query: Query<(Entity, &mut Inv, Option<&PlayerAbilities>, &WorldName)>,
    worlds: Res<Worlds>,
) {
    let (entity, mut inventory, player_abilities, world_name) =
        query.get_mut(container_click.entity).unwrap();
    if inventory.id != container_click.window_id {
        error!(
            "Tried to click container with ID {}, but the current container ID is {}. Click packet won't be sent.",
            container_click.window_id, inventory.id
        );
        return;
    }

    let Some(world) = worlds.get(world_name) else {
        return;
    };

    let old_slots = inventory.menu().slots();
    inventory.simulate_click(
        &container_click.operation,
        player_abilities.unwrap_or(&PlayerAbilities::default()),
    );
    let new_slots = inventory.menu().slots();

    let registry_holder = &world.read().registries;

    // see which slots changed after clicking and put them in the map the server
    // uses this to check if we desynced
    let mut changed_slots: IndexMap<u16, HashedStack> = IndexMap::new();
    for (slot_index, old_slot) in old_slots.iter().enumerate() {
        let new_slot = &new_slots[slot_index];
        if old_slot != new_slot {
            changed_slots.insert(
                slot_index as u16,
                HashedStack::from_item_stack(new_slot, registry_holder),
            );
        }
    }

    commands.trigger(SendGamePacketEvent::new(
        entity,
        ServerboundContainerClick {
            container_id: container_click.window_id,
            state_id: inventory.state_id,
            slot_num: container_click
                .operation
                .slot_num()
                .map(|n| n as i16)
                .unwrap_or(-999),
            button_num: container_click.operation.button_num(),
            click_type: container_click.operation.click_type(),
            changed_slots,
            carried_item: HashedStack::from_item_stack(&inventory.carried, registry_holder),
        },
    ));
}

/// Sent from the server when the contents of a container are replaced.
///
/// Usually triggered by the `ContainerSetContent` packet.
#[derive(EntityEvent)]
pub struct SetContainerContentEvent {
    pub entity: Entity,
    pub slots: Vec<ItemStack>,
    pub container_id: i32,
}
pub fn handle_set_container_content_trigger(
    set_container_content: On<SetContainerContentEvent>,
    mut query: Query<&mut Inv>,
) {
    let mut inventory = query.get_mut(set_container_content.entity).unwrap();

    if set_container_content.container_id != inventory.id {
        warn!(
            "Got SetContainerContentEvent for container with ID {}, but the current container ID is {}",
            set_container_content.container_id, inventory.id
        );
        return;
    }

    let menu = inventory.menu_mut();
    for (i, slot) in set_container_content.slots.iter().enumerate() {
        if let Some(slot_mut) = menu.slot_mut(i) {
            *slot_mut = slot.clone();
        }
    }
}

/// An ECS message to switch our hand to a different hotbar slot.
///
/// This is equivalent to using the scroll wheel or number keys in Minecraft.
#[derive(EntityEvent)]
pub struct SetSelectedHotbarSlotEvent {
    pub entity: Entity,
    /// The hotbar slot to select. This should be in the range 0..=8.
    pub slot: u8,
}
pub fn handle_set_selected_hotbar_slot_event(
    set_selected_hotbar_slot: On<SetSelectedHotbarSlotEvent>,
    mut query: Query<&mut Inv>,
) {
    let mut inventory = query.get_mut(set_selected_hotbar_slot.entity).unwrap();
    inventory.selected_hotbar_slot = set_selected_hotbar_slot.slot;
}

/// The item slot that the server thinks we have selected.
///
/// See [`ensure_has_sent_carried_item`].
#[derive(Component)]
pub struct LastSentSelectedHotbarSlot {
    pub slot: u8,
}
/// A system that makes sure that [`LastSentSelectedHotbarSlot`] is in sync with
/// [`Inv::selected_hotbar_slot`].
///
/// This is necessary to make sure that [`ServerboundSetCarriedItem`] is sent in
/// the right order, since it's not allowed to happen outside of a tick.
pub fn ensure_has_sent_carried_item(
    mut commands: Commands,
    query: Query<(Entity, &Inv, Option<&LastSentSelectedHotbarSlot>)>,
) {
    for (entity, inventory, last_sent) in query.iter() {
        if let Some(last_sent) = last_sent {
            if last_sent.slot == inventory.selected_hotbar_slot {
                continue;
            }

            commands.trigger(SendGamePacketEvent::new(
                entity,
                ServerboundSetCarriedItem {
                    slot: inventory.selected_hotbar_slot as u16,
                },
            ));
        }

        commands.entity(entity).insert(LastSentSelectedHotbarSlot {
            slot: inventory.selected_hotbar_slot,
        });
    }
}
