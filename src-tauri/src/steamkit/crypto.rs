//! Crypto helpers: AES-256 (ECB/CBC) for depot chunks & filenames,
//! RSA for password encryption.

use aes::cipher::{BlockDecrypt, KeyInit};
use aes::Aes256;
use anyhow::{anyhow, bail, Context};
use base64::Engine;

type Aes256CbcDec = cbc::Decryptor<Aes256>;

/// Decrypts a single 16-byte block with AES-256-ECB (no padding).
pub fn aes256_ecb_decrypt_block(key: &[u8], block: &[u8; 16]) -> [u8; 16] {
    let cipher = Aes256::new_from_slice(key).expect("AES-256 key must be 32 bytes");
    let mut buf = aes::cipher::generic_array::GenericArray::clone_from_slice(block);
    cipher.decrypt_block(&mut buf);
    let mut out = [0u8; 16];
    out.copy_from_slice(&buf);
    out
}

/// Decrypts data with AES-256-CBC + PKCS7 padding.
pub fn aes256_cbc_decrypt(key: &[u8], iv: &[u8; 16], data: &[u8]) -> anyhow::Result<Vec<u8>> {
    use aes::cipher::{BlockDecryptMut, KeyIvInit};
    let dec = Aes256CbcDec::new_from_slices(key, iv).map_err(|e| anyhow!("CBC init: {e}"))?;
    let mut buf = data.to_vec();
    let out = dec
        .decrypt_padded_mut::<aes::cipher::block_padding::Pkcs7>(&mut buf)
        .map_err(|e| anyhow!("CBC decrypt: {e}"))?;
    Ok(out.to_vec())
}

/// Decrypts a Steam symmetric (depot) encrypted name: base64 of
/// [ECB-encrypted IV | CBC-encrypted data], UTF-8 with trailing NULs.
/// Steam wraps the base64 payload with newlines; strip whitespace first.
pub fn decrypt_symmetric_name(encoded: &str, key: &[u8]) -> anyhow::Result<String> {
    let cleaned: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
    let data = base64::engine::general_purpose::STANDARD
        .decode(&cleaned)
        .context("base64 decode of encrypted name")?;
    if data.len() < 16 || data.len() % 16 != 0 {
        bail!("encrypted name has invalid length {}", data.len());
    }
    let iv = aes256_ecb_decrypt_block(key, data[..16].try_into().unwrap());
    let plain = aes256_cbc_decrypt(key, &iv, &data[16..])?;
    let trimmed: Vec<u8> = plain
        .into_iter()
        .take_while(|b| *b != 0)
        .collect();
    Ok(String::from_utf8_lossy(&trimmed).into_owned())
}

/// RSA PKCS#1 v1.5 encryption of the password for credentials auth.
pub fn rsa_encrypt_password(modulus_hex: &str, exponent_hex: &str, password: &str) -> anyhow::Result<String> {
    use rsa::BigUint;
    let n = BigUint::from_bytes_be(&hex::decode(modulus_hex).context("bad modulus hex")?);
    let e = BigUint::from_bytes_be(&hex::decode(exponent_hex).context("bad exponent hex")?);
    let key = rsa::RsaPublicKey::new(n, e).context("invalid RSA public key")?;
    let mut rng = rand::thread_rng();
    let encrypted = key
        .encrypt(&mut rng, rsa::Pkcs1v15Encrypt, password.as_bytes())
        .context("RSA encrypt failed")?;
    Ok(base64::engine::general_purpose::STANDARD.encode(encrypted))
}

/// Adler-32 with zero seed (as used by depot chunk checksums).
pub fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 0;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}
