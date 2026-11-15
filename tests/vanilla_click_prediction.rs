//! Clicks on a menu the server has not answered yet report what vanilla's own
//! click prediction shows.
//!
//! On 2026-11-15 at 16:55:08 the bot clicked a grace-period bed in BIN Auction
//! View slot 31 three times, 26 ms apart, with no server packet in between.
//! Every click said "slot 31 emptied, bed on the cursor". A vanilla client
//! applies each click to its own menu before sending it, so its second click
//! puts the bed back (slot 31 = bed, cursor empty). The vanilla oracle flagged
//! that second click as HASH. These cases pin the vanilla sequence.

use azalea::entity::inventory::Inventory;
use azalea::registry::builtin::{ItemKind, MenuKind};
use azalea_client::inventory::{predict_click, ClickPrediction, PredictedClick};
use azalea_inventory::operations::{ClickOperation, PickupClick};
use azalea_inventory::{ItemStack, Menu};

const BUY_SLOT: u16 = 31;
const CONFIRM_SLOT: u16 = 11;

fn menu_with(kind: MenuKind, slot: u16, item: ItemKind, id: i32, state_id: u32) -> Inventory {
    let mut menu = Menu::from_kind(kind);
    *menu.slot_mut(slot as usize).unwrap() = ItemStack::from(item);
    Inventory {
        id,
        container_menu: Some(menu),
        state_id,
        ..Default::default()
    }
}

/// BIN Auction View as the server last sent it, cursor empty.
fn bin_auction_view(slot_31: ItemKind) -> Inventory {
    menu_with(MenuKind::Generic9x6, BUY_SLOT, slot_31, 21, 58)
}

fn left_click(server: &Inventory, prediction: &mut ClickPrediction, slot: u16) -> PredictedClick {
    predict_click(
        server,
        prediction,
        &ClickOperation::Pickup(PickupClick::Left { slot: Some(slot) }),
        &Default::default(),
    )
}

fn bed() -> ItemStack {
    ItemStack::from(ItemKind::RedBed)
}

#[test]
fn repeated_bed_clicks_alternate_pickup_and_place() {
    let server = bin_auction_view(ItemKind::RedBed);
    let mut prediction = ClickPrediction::default();

    let first = left_click(&server, &mut prediction, BUY_SLOT);
    assert_eq!(first.state_id, 58);
    assert_eq!(first.changed_slots, vec![(BUY_SLOT, ItemStack::Empty)]);
    assert_eq!(first.carried, bed());

    // The click the oracle flagged: vanilla puts the bed back.
    let second = left_click(&server, &mut prediction, BUY_SLOT);
    assert_eq!(second.state_id, 58);
    assert_eq!(second.changed_slots, vec![(BUY_SLOT, bed())]);
    assert_eq!(second.carried, ItemStack::Empty);

    let third = left_click(&server, &mut prediction, BUY_SLOT);
    assert_eq!(third.changed_slots, vec![(BUY_SLOT, ItemStack::Empty)]);
    assert_eq!(third.carried, bed());
}

#[test]
fn gold_nugget_with_the_bed_on_the_cursor_swaps() {
    let mut server = bin_auction_view(ItemKind::RedBed);
    let mut prediction = ClickPrediction::default();
    left_click(&server, &mut prediction, BUY_SLOT);

    // Grace ends: the server sets slot 31 and leaves the cursor alone.
    *server.menu_mut().slot_mut(BUY_SLOT as usize).unwrap() = ItemStack::from(ItemKind::GoldNugget);
    server.state_id = 59;
    prediction.server_set_slot(BUY_SLOT);

    let buy = left_click(&server, &mut prediction, BUY_SLOT);
    assert_eq!(buy.state_id, 59);
    assert_eq!(buy.changed_slots, vec![(BUY_SLOT, bed())]);
    assert_eq!(buy.carried, ItemStack::from(ItemKind::GoldNugget));
}

#[test]
fn a_server_slot_write_elsewhere_keeps_the_guess() {
    let mut server = bin_auction_view(ItemKind::RedBed);
    let mut prediction = ClickPrediction::default();
    left_click(&server, &mut prediction, BUY_SLOT);

    // The server rewrites the item preview in slot 13, not slot 31.
    *server.menu_mut().slot_mut(13).unwrap() = ItemStack::from(ItemKind::Diamond);
    server.state_id = 59;
    prediction.server_set_slot(13);

    let second = left_click(&server, &mut prediction, BUY_SLOT);
    assert_eq!(second.state_id, 59);
    assert_eq!(second.changed_slots, vec![(BUY_SLOT, bed())]);
    assert_eq!(second.carried, ItemStack::Empty);
}

#[test]
fn a_full_resync_replaces_the_guess() {
    let server = bin_auction_view(ItemKind::RedBed);
    let mut prediction = ClickPrediction::default();
    left_click(&server, &mut prediction, BUY_SLOT);

    // container_set_content: every slot and the cursor are the server's again.
    prediction.reset(None);

    let again = left_click(&server, &mut prediction, BUY_SLOT);
    assert_eq!(again.changed_slots, vec![(BUY_SLOT, ItemStack::Empty)]);
    assert_eq!(again.carried, bed());
}

#[test]
fn a_new_menu_starts_with_an_empty_cursor() {
    let auction = bin_auction_view(ItemKind::RedBed);
    let mut prediction = ClickPrediction::default();
    left_click(&auction, &mut prediction, BUY_SLOT);

    // Confirm Purchase opens while the bed is still on vanilla's cursor.
    // azalea's open-screen handler resets the guesses to a fresh menu.
    let confirm = menu_with(
        MenuKind::Generic9x3,
        CONFIRM_SLOT,
        ItemKind::GreenTerracotta,
        22,
        13,
    );
    prediction.reset(Some(ItemStack::Empty));

    let click = left_click(&confirm, &mut prediction, CONFIRM_SLOT);
    assert_eq!(click.state_id, 13);
    assert_eq!(click.changed_slots, vec![(CONFIRM_SLOT, ItemStack::Empty)]);
    assert_eq!(click.carried, ItemStack::from(ItemKind::GreenTerracotta));
}
