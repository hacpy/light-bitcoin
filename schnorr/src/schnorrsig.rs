#![allow(non_snake_case)]

use core::ops::Neg;

use crate::{
    taggedhash::{HashAdd, Tagged},
    xonly::XOnly,
};
use digest::Digest;
use secp256k1::{
    curve::{Affine, Scalar},
    Message, PublicKey, SecretKey, Signature,
};

/// Construct schnorr sig challenge
/// hash(R_x|P_x|msg)
pub fn schnorrsig_challenge(rx: &XOnly, pkx: &XOnly, msg: &Message) -> Scalar {
    let mut bytes = [0u8; 32];
    let hash = sha2::Sha256::default().tagged(b"BIP0340/challenge");
    let tagged = hash.add(rx).add(pkx).add(&msg.0).finalize();

    bytes.copy_from_slice(tagged.as_slice());
    let mut scalar = Scalar::default();
    let _ = scalar.set_b32(&bytes);
    scalar
}

/// Generate nonce k and nonce point R
pub fn nonce_function_bip340(
    bip340_sk: &SecretKey,
    bip340_pkx: &XOnly,
    msg: &Message,
    aux: &Message,
) -> (Scalar, Affine) {
    let aux_hash = sha2::Sha256::default().tagged(b"BIP0340/aux");
    let aux_tagged = aux_hash.add(&aux.0).finalize();
    let sec_bytes: [u8; 32] = bip340_sk.serialize();
    let mut aux_bytes = [0u8; 32];
    aux_bytes.copy_from_slice(&aux_tagged);

    // bitwise xor the hashed randomness with secret
    for (i, byte) in aux_bytes.iter_mut().enumerate() {
        *byte ^= sec_bytes[i]
    }

    let nonce_hash = sha2::Sha256::default().tagged(b"BIP0340/nonce");
    let nonce_tagged = nonce_hash
        .add(&aux_bytes)
        .add(bip340_pkx)
        .add(&msg.0)
        .finalize();

    let mut nonce_bytes = [0u8; 32];
    nonce_bytes.copy_from_slice(nonce_tagged.as_slice());
    let mut scalar = Scalar::default();
    let _ = scalar.set_b32(&nonce_bytes);
    let k = SecretKey::parse(&scalar.b32()).unwrap();
    let R = PublicKey::from_secret_key(&k);
    (k.into(), R.into())
}

/// Sign a message using the secret key
pub fn sign(msg: Message, aux: Message, seckey: SecretKey, pubkey: PublicKey) -> Signature {
    let mut pk: Affine = pubkey.into();

    let pkx = XOnly::from_field(&mut pk.x).unwrap();

    // Get nonce k and nonce point R
    let (k, mut R) = nonce_function_bip340(&seckey, &pkx, &msg, &aux);
    R.y.normalize();
    R.x.normalize();
    let k_even = if R.y.is_odd() { k.neg() } else { k };

    // Generate s = k + tagged_hash("BIP0340/challenge", R_x|P_x|msg) * d
    let rx = XOnly::from_bytes(R.x.b32()).unwrap();
    let h = schnorrsig_challenge(&rx, &pkx, &msg);
    let s = k_even + h * seckey.into();

    // Generate sig = R_x|s
    Signature { r: rx.into(), s }
}

#[cfg(test)]
mod tests {
    use sha2::Sha256;

    use super::*;

    /// Check if the function is available
    #[test]
    fn test_sign() {
        let msg = Sha256::digest(b"message");
        let aux = Sha256::digest(b"random auxiliary data");

        let m = Message::parse_slice(msg.as_slice()).unwrap();
        let a = Message::parse_slice(aux.as_slice()).unwrap();

        let seckey = SecretKey::parse_slice(&Scalar::from_int(1).b32()).unwrap();
        let pubkey = PublicKey::from_secret_key(&seckey);

        let sig = sign(m, a, seckey, pubkey);
        println!("{:?}", sig.serialize());
    }
}