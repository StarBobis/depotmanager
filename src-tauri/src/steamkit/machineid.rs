//! Machine ID generation (binary KeyValues blob sent in ClientLogon).
//!
//! Steam requires a "well-formed" machine MessageObject for logon.
//! We derive stable pseudo-random IDs persisted per installation.

use sha1::{Digest, Sha1};

fn sha1_hex(data: &[u8]) -> String {
    let digest = Sha1::digest(data);
    hex::encode_upper(digest)
}

/// Builds the binary KeyValues machine-id blob:
/// MessageObject { "BB3" = hex(sha1(seed)), "FF2" = ..., "3B3" = ... }
pub fn build_machine_id(seed: &[u8]) -> Vec<u8> {
    let bb3 = sha1_hex(&[seed, b":bb3"].concat());
    let ff2 = sha1_hex(&[seed, b":ff2"].concat());
    let b3b3 = sha1_hex(&[seed, b":3b3"].concat());

    let mut out = Vec::with_capacity(160);
    // root node: type None (0x00), name "MessageObject"
    out.push(0x00);
    out.extend_from_slice(b"MessageObject\0");
    for (name, value) in [("BB3", bb3), ("FF2", ff2), ("3B3", b3b3)] {
        // string entry: type String (0x01)
        out.push(0x01);
        out.extend_from_slice(name.as_bytes());
        out.push(0x00);
        out.extend_from_slice(value.as_bytes());
        out.push(0x00);
    }
    out.push(0x08); // end of root children
    out.push(0x08); // extra terminator (matches SteamKit RecursiveSaveBinaryToStream)
    out
}

/// Generates a fresh random seed to persist in settings.
pub fn random_seed() -> Vec<u8> {
    use rand::RngCore;
    let mut seed = vec![0u8; 32];
    rand::thread_rng().fill_bytes(&mut seed);
    seed
}
