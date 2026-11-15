//! A player head's `minecraft:profile` component hashes like vanilla.
//!
//! Vanilla's `UUIDUtil.CODEC` writes a UUID as four ints and `HashedStack`
//! hashes the profile through it. azalea used to hash the `uuid` crate's
//! string form, so every click carrying or changing a player head with a
//! profile id sent a hash no vanilla client produces. The expected values
//! come from Mojang's 26.3 code (`ResolvableProfile.CODEC` encoded with
//! `HashOps.CRC32C_INSTANCE`), and the partial profile is the Green Jerry
//! Talisman exactly as Hypixel sent it, where the vanilla oracle saw
//! vanilla 838bb343 and the bot 975257cc.

use std::io::Cursor;

use azalea::buf::AzBuf;
use azalea::core::registry_holder::RegistryHolder;
use azalea_inventory::components::{EncodableDataComponent, Profile};

const UUID: [u8; 16] = [
    0x68, 0x10, 0xa0, 0x21, 0xfe, 0x95, 0x34, 0x2f, 0xbb, 0x83, 0xe0, 0xc1, 0x2e, 0x8e, 0xbb, 0x53,
];

const TEXTURES_VALUE: &[&str] = &[
    "ewogICJ0aW1lc3RhbXAiIDogMTYxMDU4NTE1MzgwNywKICAicHJvZmlsZUlkIiA6ICI0ZmFkNjk2NTIxNGI0NGQ4Yj",
    "AxYzlhZTVjZDQ0MDVjOCIsCiAgInByb2ZpbGVOYW1lIiA6ICJoaXBfYXNpYW4iLAogICJzaWduYXR1cmVSZXF1aXJl",
    "ZCIgOiB0cnVlLAogICJ0ZXh0dXJlcyIgOiB7CiAgICAiU0tJTiIgOiB7CiAgICAgICJ1cmwiIDogImh0dHA6Ly90ZX",
    "h0dXJlcy5taW5lY3JhZnQubmV0L3RleHR1cmUvMjU2NzI1MWQ3MWI2ZjNhNGRjMDNmYTAxZTQ0MWRlN2VkZjkzNTQ3",
    "OTRlNzRhZWMxNmMwNGYxNDFkZGYzMWM3MCIKICAgIH0KICB9Cn0=",
];

const TEXTURES_SIGNATURE: &[&str] = &[
    "nbnDi/h7h2NAkXHbRe2k9f1BARfgKHAz0oiEF5ckHjSWQHrYzxr4VBwfCGcTzBCbQv244HXEE/RZOSQVGNHHZgP6CY",
    "sI0xO40jNa9pa1vwb/3Xq4xN5HyU9Hca/9hZYtD+myFqnJw5dHEVs0+33006acXdCRCHn6+i/Lca1RWgRS2Svwaw5w",
    "2axrLZW6x916SVX9Pxirr9y4enP+qjsuTYyaXM4CA4pR4l4cyP92gAtR93Y229hes3Ofo1NM+/KGYN4xzNqbZbxsB+",
    "7iQhk48GVb8M1bcm50ArBtT2endrpNIePlMQVmKczlDjQJKOinliXPMe/gTDkcR+8/ww7u+h2PQoKBYfWP7df6EHu3",
    "AwRPtjWVTBGvYn3NXhyVZiIY/TJAYQLyA20n+aG+yf4y4n/wryoEHfTpMsovJoYxfEtp5pPq1JM7OpRXkzj/4L7lvv",
    "6pBbNZP7lIFtklLaPn1iNh+hwMgfiPv5gXAcrVuu+xUlPPp/J68GFJU25z9wwcFB0+gaUQrDbJgIVi2MvxEmGiP2fF",
    "3NhOHIl2a0LiJ770z+D8nQhHE+QETsg2/H6d6WqfDaZj4zmspmwt45VDTyV6TMs/9Cr3mLVkWrTZhaAl7PB3IbSvQY",
    "UQ3I6ZmINjuJCZvCn/wWWucd59wMYs41ulXgC7ltW2huakKUrzey0=",
];

fn var_int(buf: &mut Vec<u8>, mut value: u32) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

fn string(buf: &mut Vec<u8>, s: &str) {
    var_int(buf, s.len() as u32);
    buf.extend_from_slice(s.as_bytes());
}

/// Wire form of the textures property map: one entry, with its signature.
fn textures(buf: &mut Vec<u8>) {
    var_int(buf, 1);
    string(buf, "textures");
    string(buf, &TEXTURES_VALUE.concat());
    buf.push(1);
    string(buf, &TEXTURES_SIGNATURE.concat());
}

/// Decodes a profile the way the bot reads it off the wire, then hashes it
/// the way the bot hashes it for a click.
fn hash(wire: &[u8]) -> u32 {
    let mut cursor = Cursor::new(wire);
    let profile = Profile::azalea_read(&mut cursor).expect("profile decodes");
    assert_eq!(cursor.position() as usize, wire.len(), "trailing bytes");
    profile.crc_hash(&RegistryHolder::default()).0
}

/// Skin patch: no texture, cape, elytra or model override.
const NO_SKIN_PATCH: [u8; 4] = [0, 0, 0, 0];

#[test]
fn partial_profile_like_hypixel_sends_hashes_like_vanilla() {
    // partial, no name, id, textures with signature
    let mut wire = vec![0, 0, 1];
    wire.extend_from_slice(&UUID);
    textures(&mut wire);
    wire.extend_from_slice(&NO_SKIN_PATCH);
    assert_eq!(format!("{:08x}", hash(&wire)), "838bb343");
}

#[test]
fn partial_profile_with_only_an_id_hashes_like_vanilla() {
    let mut wire = vec![0, 0, 1];
    wire.extend_from_slice(&UUID);
    wire.push(0); // no properties
    wire.extend_from_slice(&NO_SKIN_PATCH);
    assert_eq!(format!("{:08x}", hash(&wire)), "dd1e19ae");
}

#[test]
fn full_profile_hashes_like_vanilla() {
    let mut with_textures = vec![1];
    with_textures.extend_from_slice(&UUID);
    string(&mut with_textures, "hip_asian");
    textures(&mut with_textures);
    with_textures.extend_from_slice(&NO_SKIN_PATCH);
    assert_eq!(format!("{:08x}", hash(&with_textures)), "41d136b6");

    let mut bare = vec![1];
    bare.extend_from_slice(&UUID);
    string(&mut bare, "hip_asian");
    bare.push(0); // no properties: vanilla leaves the field out
    bare.extend_from_slice(&NO_SKIN_PATCH);
    assert_eq!(format!("{:08x}", hash(&bare)), "726666e1");
}
