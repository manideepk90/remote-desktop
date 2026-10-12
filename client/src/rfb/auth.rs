//! Security types: VNC password (DES challenge), Apple Remote Desktop
//! (Diffie-Hellman + AES, username and password) and VeNCrypt Plain.

use aes::Aes128;
use des::Des;
use des::cipher::{BlockCipherEncrypt, KeyInit};
use md5::{Digest, Md5};
use num_bigint::BigUint;

pub const NONE: u8 = 1;
pub const VNC: u8 = 2;
pub const VENCRYPT: u8 = 19;
pub const ARD: u8 = 30;

pub const VENCRYPT_PLAIN: u32 = 256;

/// Picks a security type from what the server offers. A username means the
/// person expects an account login (macOS Screen Sharing), so ARD goes first.
/// VeNCrypt comes last because only its unencrypted Plain subtype is supported.
pub fn choose(offered: &[u8], have_username: bool) -> Option<u8> {
    let order: &[u8] = if have_username { &[ARD, VNC, NONE, VENCRYPT] } else { &[NONE, VNC, ARD, VENCRYPT] };
    order.iter().copied().find(|t| offered.contains(t))
}

pub fn name(t: u8) -> String {
    match t {
        NONE => "None".into(),
        VNC => "VNC password".into(),
        5 | 6 => "RealVNC RA2".into(),
        16 => "Tight".into(),
        VENCRYPT => "VeNCrypt".into(),
        ARD => "Apple Remote Desktop".into(),
        113 => "UltraVNC MS-Logon".into(),
        t => format!("type {t}"),
    }
}

/// VNC authentication: DES-encrypt the challenge with the password (first 8
/// bytes, each bit-reversed) as the key.
pub fn vnc_response(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (k, b) in key.iter_mut().zip(password.bytes()) {
        *k = b.reverse_bits();
    }
    let des = Des::new_from_slice(&key).expect("8-byte key");
    let mut out = *challenge;
    for block in out.as_chunks_mut::<8>().0 {
        des.encrypt_block(block.into());
    }
    out
}

/// Apple Remote Desktop authentication. Given the server's generator, prime
/// and public key, returns what the client sends back: the encrypted
/// credentials followed by the client's public key.
pub fn ard_response(generator: &[u8], prime: &[u8], server_key: &[u8], username: &str, password: &str) -> Vec<u8> {
    let len = prime.len();
    let mut private = vec![0u8; len];
    rand::fill(&mut private[..]);
    ard_response_with(generator, prime, server_key, &private, username, password)
}

fn ard_response_with(
    generator: &[u8],
    prime: &[u8],
    server_key: &[u8],
    private: &[u8],
    username: &str,
    password: &str,
) -> Vec<u8> {
    let len = prime.len();
    let p = BigUint::from_bytes_be(prime);
    let g = BigUint::from_bytes_be(generator);
    let x = BigUint::from_bytes_be(private);
    let public = g.modpow(&x, &p);
    let shared = BigUint::from_bytes_be(server_key).modpow(&x, &p);
    let key = Md5::digest(pad(&shared.to_bytes_be(), len));

    // { username[64], password[64] }, each NUL-terminated, the rest random.
    let mut creds = [0u8; 128];
    rand::fill(&mut creds[..]);
    for (field, value) in [(0, username), (64, password)] {
        let bytes = &value.as_bytes()[..value.len().min(63)];
        creds[field..field + bytes.len()].copy_from_slice(bytes);
        creds[field + bytes.len()] = 0;
    }
    let aes = Aes128::new_from_slice(&key).expect("16-byte key");
    for block in creds.as_chunks_mut::<16>().0 {
        aes.encrypt_block(block.into());
    }
    let mut out = creds.to_vec();
    out.extend(pad(&public.to_bytes_be(), len));
    out
}

/// Left-pads a big-endian number to `len` bytes.
fn pad(bytes: &[u8], len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len.saturating_sub(bytes.len())];
    out.extend_from_slice(&bytes[bytes.len().saturating_sub(len)..]);
    out
}

/// VeNCrypt Plain: lengths then the username and password in the clear.
pub fn vencrypt_plain(username: &str, password: &str) -> Vec<u8> {
    let mut out = (username.len() as u32).to_be_bytes().to_vec();
    out.extend((password.len() as u32).to_be_bytes());
    out.extend(username.as_bytes());
    out.extend(password.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockCipherDecrypt;

    #[test]
    fn vnc_response_is_des_ecb_with_reversed_key() {
        let r = vnc_response("password", &[0u8; 16]);
        assert_eq!(&r[..8], &r[8..]);
        let key: Vec<u8> = b"password".iter().map(|b| b.reverse_bits()).collect();
        let des = Des::new_from_slice(&key).unwrap();
        let mut b = [0u8; 8];
        des.encrypt_block((&mut b).into());
        assert_eq!(&r[..8], &b);
    }

    #[test]
    fn vnc_password_is_truncated_to_eight_bytes() {
        let c = [7u8; 16];
        assert_eq!(vnc_response("12345678", &c), vnc_response("123456789", &c));
        assert_ne!(vnc_response("1234567", &c), vnc_response("12345678", &c));
    }

    #[test]
    fn ard_server_can_decrypt_credentials() {
        // A toy 64-bit group is enough to check the arithmetic and packing.
        let prime = 0xffff_ffff_ffff_ffc5u64.to_be_bytes(); // largest 64-bit prime
        let generator = [0u8, 2];
        let server_private = BigUint::from(0x1234_5678_9abc_def0u64);
        let p = BigUint::from_bytes_be(&prime);
        let server_public = pad(&BigUint::from(2u8).modpow(&server_private, &p).to_bytes_be(), 8);

        let client_private = 0x0fed_cba9_8765_4321u64.to_be_bytes();
        let out = ard_response_with(&generator, &prime, &server_public, &client_private, "alice", "s3cret");
        assert_eq!(out.len(), 128 + 8);

        // Server side: shared = client_public ^ server_private.
        let client_public = BigUint::from_bytes_be(&out[128..]);
        let shared = client_public.modpow(&server_private, &p);
        let key = Md5::digest(pad(&shared.to_bytes_be(), 8));
        let aes = Aes128::new_from_slice(&key).unwrap();
        let mut plain = out[..128].to_vec();
        for block in plain.as_chunks_mut::<16>().0 {
            aes.decrypt_block(block.into());
        }
        assert_eq!(&plain[..6], b"alice\0");
        assert_eq!(&plain[64..71], b"s3cret\0");
    }

    #[test]
    fn choose_prefers_account_login_with_a_username() {
        assert_eq!(choose(&[VNC, ARD], true), Some(ARD));
        assert_eq!(choose(&[VNC, ARD], false), Some(VNC));
        assert_eq!(choose(&[ARD], false), Some(ARD));
        assert_eq!(choose(&[VENCRYPT, VNC], true), Some(VNC));
        assert_eq!(choose(&[VENCRYPT], false), Some(VENCRYPT));
        assert_eq!(choose(&[5, 6], false), None);
    }

    #[test]
    fn pad_left_pads_and_truncates() {
        assert_eq!(pad(&[1, 2], 4), vec![0, 0, 1, 2]);
        assert_eq!(pad(&[1, 2, 3], 2), vec![2, 3]);
    }
}
