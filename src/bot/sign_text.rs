//! Sign editor lines the way a vanilla client sends them.
//!
//! Hypixel's price, amount and duration prompts are client-side signs: a
//! `BlockUpdate` places the sign, `BlockEntityData` fills its text (the
//! "^^^^^^^^^^^^^^^" hint and the prompt lines), then `OpenSignEditor` opens it.
//!
//! Vanilla's `SignEditScreen` loads all four existing lines with
//! `Component.getString()`, the player types on line 1, and closing the screen
//! sends all four lines back. Lines 2 to 4 therefore always echo whatever the
//! server pre-filled. If the client never received sign text for that position,
//! vanilla edits a fresh sign and sends empty lines.
//!
//! BAF used to send hard-coded lines instead: `[value, "", "", ""]` for AH
//! price and duration, `[value, "^^^^^^^^^^^^^^^", "", ""]` for Bazaar amount
//! and price. Whenever Hypixel pre-fills more than that, every listing and
//! every order sent sign text no vanilla client produces without the player
//! deleting the prompt by hand.

use azalea::BlockPos;
use azalea_protocol::common::sign::SignTextSlot;
use azalea_protocol::simdnbt::owned::{Nbt, NbtCompound, NbtList, NbtTag};
use parking_lot::Mutex;

/// Plain text of both sides of a sign, as `Component.getString()` renders it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SignLines {
    pub front: [String; 4],
    pub back: [String; 4],
}

impl SignLines {
    fn side(&self, slot: SignTextSlot) -> &[String; 4] {
        match slot {
            SignTextSlot::Front => &self.front,
            SignTextSlot::Back => &self.back,
        }
    }
}

/// The sign block entity the client currently holds. Hypixel shows one prompt
/// sign at a time, so the latest one is all a vanilla client would have.
static CURRENT_SIGN: Mutex<Option<(BlockPos, SignLines)>> = Mutex::new(None);

/// `BlockEntityData`: keep the text when it is a sign, drop a stale sign the
/// packet replaces.
pub(crate) fn on_block_entity_data(pos: BlockPos, tag: &Nbt) {
    let mut current = CURRENT_SIGN.lock();
    match parse_sign_lines(tag) {
        Some(lines) => *current = Some((pos, lines)),
        None if current.as_ref().is_some_and(|(p, _)| *p == pos) => *current = None,
        None => {}
    }
}

/// `BlockUpdate`: a block change at the sign's position removes its block
/// entity in vanilla. The text that follows comes in a fresh `BlockEntityData`.
pub(crate) fn on_block_update(pos: BlockPos) {
    let mut current = CURRENT_SIGN.lock();
    if current.as_ref().is_some_and(|(p, _)| *p == pos) {
        *current = None;
    }
}

/// The four lines a vanilla client sends after typing `typed` into the sign
/// opened at `pos`: the typed value on line 1, the pre-filled lines 2 to 4
/// unchanged, or empty lines if no sign text was received for `pos`.
pub(crate) fn update_lines(pos: BlockPos, slot: SignTextSlot, typed: String) -> [String; 4] {
    let current = CURRENT_SIGN.lock();
    let prefilled = current
        .as_ref()
        .filter(|(p, _)| *p == pos)
        .map(|(_, lines)| lines.side(slot));
    vanilla_lines(typed, prefilled)
}

fn vanilla_lines(typed: String, prefilled: Option<&[String; 4]>) -> [String; 4] {
    let mut lines = prefilled.cloned().unwrap_or_default();
    lines[0] = typed;
    lines
}

/// Sign text from a sign block entity's NBT (`front_text` / `back_text`, each
/// with four `messages`). `None` when the tag is not a sign.
pub(crate) fn parse_sign_lines(nbt: &Nbt) -> Option<SignLines> {
    let Nbt::Some(base) = nbt else {
        return None;
    };
    let front = side_lines(base.compound("front_text"));
    let back = side_lines(base.compound("back_text"));
    if front.is_none() && back.is_none() {
        return None;
    }
    Some(SignLines {
        front: front.unwrap_or_default(),
        back: back.unwrap_or_default(),
    })
}

fn side_lines(side: Option<&NbtCompound>) -> Option<[String; 4]> {
    let messages = side?.list("messages")?;
    let mut lines: [String; 4] = Default::default();
    for (line, text) in lines.iter_mut().zip(list_texts(messages)) {
        *line = text;
    }
    Some(lines)
}

/// One plain string per list element.
fn list_texts(list: &NbtList) -> Vec<String> {
    match list {
        NbtList::String(strings) => strings.iter().map(|s| s.to_str().into_owned()).collect(),
        NbtList::Compound(compounds) => compounds.iter().map(compound_text).collect(),
        NbtList::List(lists) => lists.iter().map(|l| list_texts(l).concat()).collect(),
        _ => Vec::new(),
    }
}

fn tag_text(tag: &NbtTag) -> String {
    match tag {
        NbtTag::String(s) => s.to_str().into_owned(),
        NbtTag::Compound(c) => compound_text(c),
        NbtTag::List(l) => list_texts(l).concat(),
        _ => String::new(),
    }
}

/// `Component.getString()` of an NBT text component: its own text followed by
/// every `extra` sibling, styling dropped.
fn compound_text(c: &NbtCompound) -> String {
    // Mixed-type NBT lists wrap each element as {"": value}.
    if let Some(inner) = c.get("") {
        return tag_text(inner);
    }
    let mut out = match (c.string("text"), c.string("translate")) {
        (Some(text), _) => text.to_str().into_owned(),
        (None, Some(key)) => c.string("fallback").unwrap_or(key).to_str().into_owned(),
        _ => String::new(),
    };
    if let Some(extra) = c.list("extra") {
        out.push_str(&list_texts(extra).concat());
    }
    out
}

#[cfg(test)]
mod tests {
    use azalea_protocol::simdnbt::owned::BaseNbt;

    use super::*;

    fn compound(values: Vec<(&str, NbtTag)>) -> NbtCompound {
        NbtCompound::from_values(values.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    fn strings(lines: &[&str]) -> NbtList {
        NbtList::String(lines.iter().map(|s| (*s).into()).collect())
    }

    fn sign(front: NbtList, back: NbtList) -> Nbt {
        Nbt::Some(BaseNbt::new(
            "",
            compound(vec![
                (
                    "front_text",
                    NbtTag::Compound(compound(vec![("messages", NbtTag::List(front))])),
                ),
                (
                    "back_text",
                    NbtTag::Compound(compound(vec![("messages", NbtTag::List(back))])),
                ),
                ("is_waxed", NbtTag::Byte(0)),
            ]),
        ))
    }

    fn lines(l: [&str; 4]) -> [String; 4] {
        l.map(String::from)
    }

    #[test]
    fn plain_string_messages_parse_per_side() {
        let nbt = sign(
            strings(&["", "^^^^^^^^^^^^^^^", "Your auction", "starting bid"]),
            strings(&["", "", "", ""]),
        );
        let parsed = parse_sign_lines(&nbt).expect("sign");
        assert_eq!(
            parsed.front,
            lines(["", "^^^^^^^^^^^^^^^", "Your auction", "starting bid"])
        );
        assert_eq!(parsed.back, lines(["", "", "", ""]));
    }

    #[test]
    fn styled_compound_messages_flatten_like_get_string() {
        let styled = compound(vec![
            ("text", NbtTag::String("Enter ".into())),
            ("color", NbtTag::String("gray".into())),
            (
                "extra",
                NbtTag::List(NbtList::Compound(vec![compound(vec![
                    ("text", NbtTag::String("amount".into())),
                    ("bold", NbtTag::Byte(1)),
                ])])),
            ),
        ]);
        let front = NbtList::Compound(vec![
            compound(vec![("", NbtTag::String("".into()))]),
            compound(vec![("", NbtTag::String("^^^^^^^^^^^^^^^".into()))]),
            styled,
            compound(vec![("text", NbtTag::String("to order".into()))]),
        ]);
        let parsed = parse_sign_lines(&sign(front, strings(&["", "", "", ""]))).expect("sign");
        assert_eq!(
            parsed.front,
            lines(["", "^^^^^^^^^^^^^^^", "Enter amount", "to order"])
        );
    }

    #[test]
    fn non_sign_block_entities_are_ignored() {
        let chest = Nbt::Some(BaseNbt::new(
            "",
            compound(vec![("Items", NbtTag::List(NbtList::Empty))]),
        ));
        assert_eq!(parse_sign_lines(&chest), None);
        assert_eq!(parse_sign_lines(&Nbt::None), None);
    }

    #[test]
    fn typed_value_replaces_line_one_and_prompt_lines_are_echoed() {
        let prefilled = lines(["", "^^^^^^^^^^^^^^^", "Your auction", "starting bid"]);
        assert_eq!(
            vanilla_lines("50000000".into(), Some(&prefilled)),
            lines([
                "50000000",
                "^^^^^^^^^^^^^^^",
                "Your auction",
                "starting bid"
            ])
        );
        assert_eq!(vanilla_lines("6".into(), None), lines(["6", "", "", ""]));
    }

    #[test]
    fn tracked_sign_follows_block_updates() {
        let pos = BlockPos::new(10, 60, -3);
        let other = BlockPos::new(11, 60, -3);
        let nbt = sign(
            strings(&["", "^^^^^^^^^^^^^^^", "Enter amount", "to order"]),
            strings(&["", "", "", ""]),
        );

        on_block_entity_data(pos, &nbt);
        assert_eq!(
            update_lines(pos, SignTextSlot::Front, "64".into()),
            lines(["64", "^^^^^^^^^^^^^^^", "Enter amount", "to order"])
        );
        // An editor opened somewhere else never sees this sign's text.
        assert_eq!(
            update_lines(other, SignTextSlot::Front, "64".into()),
            lines(["64", "", "", ""])
        );
        // Back side is its own text.
        assert_eq!(
            update_lines(pos, SignTextSlot::Back, "64".into()),
            lines(["64", "", "", ""])
        );

        // A block change elsewhere keeps it, at the sign's position removes it.
        on_block_update(other);
        assert_eq!(
            update_lines(pos, SignTextSlot::Front, "1".into())[2],
            "Enter amount"
        );
        on_block_update(pos);
        assert_eq!(
            update_lines(pos, SignTextSlot::Front, "1".into()),
            lines(["1", "", "", ""])
        );
    }
}
