#![no_std]

use core::{net::SocketAddr, ops::ControlFlow};

use prim::{hash, Encrypted, LABEL_COOKIE, LABEL_MAC1};
pub use prim::{mac, DecryptionKey, EncryptionKey, HandshakeState, Key, Mac};
pub use x25519_dalek::{PublicKey, StaticSecret};

use rand_core::{CryptoRng, RngCore};
use rustyguard_types::{
    Cookie, EncryptedCookie, EncryptedEmpty, EncryptedPublicKey, EncryptedTimestamp, HandshakeInit,
    HandshakeResp, Tag, MSG_FIRST, MSG_SECOND,
};

use tai64::Tai64N;
use zerocopy::{little_endian, transmute_mut, AsBytes, FromBytes, FromZeroes};
use zeroize::{Zeroize, ZeroizeOnDrop};

#[cfg(any(test, rustyguard_unsafe_logging))]
extern crate std;

macro_rules! unsafe_log {
    ($($t:tt)*) => {
        match core::format_args!($($t)*) {
            #[cfg(any(test, rustyguard_unsafe_logging))]
            args => std::eprintln!("{args}"),
            #[cfg(not(any(test, rustyguard_unsafe_logging)))]
            _ => {}
        };
    }
}

mod prim;

#[derive(Debug)]
pub enum CryptoError {
    DecryptionError,
    Rejected,
}

pub fn decrypt_cookie<'c>(
    cookie: &'c mut EncryptedCookie,
    key: &Key,
    nonce: &[u8; 24],
    aad: &[u8],
) -> Result<&'c mut Cookie, CryptoError> {
    use chacha20poly1305::{AeadInPlace, KeyInit, XChaCha20Poly1305};

    XChaCha20Poly1305::new(key)
        .decrypt_in_place_detached(nonce.into(), aad, &mut cookie.msg.0, (&cookie.tag.0).into())
        .map_err(|_| CryptoError::DecryptionError)?;

    Ok(&mut cookie.msg)
}

pub fn encrypt_cookie(
    mut cookie: Cookie,
    key: &Key,
    nonce: &[u8; 24],
    aad: &[u8],
) -> EncryptedCookie {
    use chacha20poly1305::{AeadInPlace, KeyInit, XChaCha20Poly1305};

    let tag = XChaCha20Poly1305::new(key)
        .encrypt_in_place_detached(nonce.into(), aad, &mut cookie.0)
        .expect("cookie message should not be larger than max message size");

    EncryptedCookie {
        msg: cookie,
        tag: Tag(tag.into()),
    }
}

pub fn mac1_key(spk: &PublicKey) -> Key {
    hash([&LABEL_MAC1, spk.as_bytes()]).into()
}
pub fn cookie_key(spk: &PublicKey) -> Key {
    hash([&LABEL_COOKIE, spk.as_bytes()]).into()
}

#[derive(Zeroize, ZeroizeOnDrop, Default)]
pub struct CookieState {
    key: Key,
}

impl CookieState {
    pub fn generate(&mut self, rng: &mut (impl CryptoRng + RngCore)) {
        rng.fill_bytes(&mut self.key);
    }

    pub fn new_cookie(&self, addr: SocketAddr) -> Cookie {
        // there's no specified encoding here - it just needs to contain the IP address and port :shrug:
        let mut a = [0; 20];
        match addr.ip() {
            core::net::IpAddr::V4(ipv4) => a[..4].copy_from_slice(&ipv4.octets()[..]),
            core::net::IpAddr::V6(ipv6) => a[..16].copy_from_slice(&ipv6.octets()[..]),
        }
        a[16..].copy_from_slice(&addr.port().to_le_bytes()[..]);
        Cookie(mac(&self.key, &a))
    }
}

/// Both handshake messages are protected via MACs which can quickly be used
/// to rule out invalid messages.
///
/// The first MAC verifies that the message is even valid - to not waste time.
/// The second MAC is only checked if the server is overloaded. If the server is
/// overloaded and second MAC is invalid, a CookieReply is sent to the client,
/// which contains an encrypted key that can be used to re-sign the handshake later.
pub trait HasMac: FromBytes + AsBytes + Sized {
    fn verify<'m>(
        &'m mut self,
        config: &StaticInitiatorConfig,
        overload: bool,
        cookie: &CookieState,
        addr: SocketAddr,
    ) -> Result<ControlFlow<Cookie, &'m mut Self>, CryptoError> {
        // verify the mac1. this should be very fast.
        // takes 450ns on my M2 Max.
        // Thus, can handle ~2 million handshakes per second.
        // This is currently single threaded.
        // TODO(conrad): need to make the rejection multi-threaded.
        self.verify_mac1(&config.mac1_key)?;

        if overload {
            // Will be roughly twice as slow as verify_mac1. 750ns on my M2 Max.
            // If the server is overloaded, this is good for rejecting DDoS attacks
            // as it requires a round trip from the sender.
            let cookie = cookie.new_cookie(addr);
            if self.verify_mac2(&cookie).is_err() {
                return Ok(ControlFlow::Break(cookie));
            }
        }

        Ok(ControlFlow::Continue(self))
    }

    fn verify_mac1(&self, mac1_key: &Key) -> Result<(), CryptoError> {
        let actual_mac1 = self.compute_mac1(mac1_key);
        if &actual_mac1 != self.get_mac1() {
            unsafe_log!("invalid mac1");
            Err(CryptoError::Rejected)
        } else {
            unsafe_log!("valid mac1");
            Ok(())
        }
    }

    fn verify_mac2(&self, cookie: &Cookie) -> Result<(), CryptoError> {
        let actual_mac2 = self.compute_mac2(cookie);
        if &actual_mac2 != self.get_mac2() {
            unsafe_log!("invalid mac2");
            Err(CryptoError::Rejected)
        } else {
            unsafe_log!("valid mac2");
            Ok(())
        }
    }

    fn compute_mac1(&self, mac1_key: &Key) -> Mac;
    fn compute_mac2(&self, cookie: &Cookie) -> Mac;
    fn get_mac1(&self) -> &Mac;
    fn get_mac2(&self) -> &Mac;
}

macro_rules! mac_protected {
    ($i:ident, $t:ident) => {
        impl HasMac for $i {
            fn compute_mac1(&self, mac1_key: &chacha20poly1305::Key) -> Mac {
                let offset = core::mem::offset_of!($i, mac1);
                let bytes = self.as_bytes();
                prim::mac(mac1_key, &bytes[..offset])
            }

            fn compute_mac2(&self, cookie: &Cookie) -> Mac {
                let offset = core::mem::offset_of!($i, mac2);
                let bytes = self.as_bytes();
                prim::mac(&cookie.0, &bytes[..offset])
            }

            fn get_mac1(&self) -> &Mac {
                &self.mac1
            }

            fn get_mac2(&self) -> &Mac {
                &self.mac2
            }
        }
    };
}

mac_protected!(HandshakeInit, MSG_FIRST);
mac_protected!(HandshakeResp, MSG_SECOND);

// ---- Noise IKpsk2 ---- //
// Wireguard makes use of a slightly modified Noise IKpsk2 handshake.
// Read the noise specification: https://noiseprotocol.org/noise.html
//
// The IKpsk2 pattern is as follows:
// <- s
// -> e, es, s, ss
// <- e, ee, se, psk
//
// The initiator is expected to know the responder's static public key prior to the handshake.
// The initiator sends an ephemeral public key and their encrypted static public key.
// The responder sends an ephemeral public key.

pub struct StaticPeerConfig {
    /// Peer's public key.
    pub key: PublicKey,
    /// Peer's preshared key.
    pub preshared_key: Key,
    /// Cached mac1_key: calculated using `mac1_key(&self.key)`
    pub mac1_key: Key,
    /// Cached cookie_key: calculated using `cookie_key(&self.key)`
    pub cookie_key: Key,
}

pub struct StaticInitiatorConfig {
    /// Our private key
    pub private_key: StaticSecret,
    /// Cached public key, derived from the above private key
    pub public_key: PublicKey,
    /// Cached mac1_key: calculated using `mac1_key(&self.public_key)`
    pub mac1_key: Key,
    /// Cached cookie_key: calculated using `cookie_key(&self.public_key)`
    pub cookie_key: Key,
}

impl StaticPeerConfig {
    pub fn new(key: PublicKey, preshared_key: Option<Key>) -> Self {
        Self {
            mac1_key: mac1_key(&key),
            cookie_key: cookie_key(&key),
            key,
            preshared_key: preshared_key.unwrap_or_default(),
        }
    }
}

impl StaticInitiatorConfig {
    pub fn new(key: StaticSecret) -> Self {
        let public_key = PublicKey::from(&key);
        Self {
            mac1_key: mac1_key(&public_key),
            cookie_key: cookie_key(&public_key),
            public_key,
            private_key: key,
        }
    }
}

#[derive(Clone, Copy, FromBytes, FromZeroes, AsBytes)]
#[repr(transparent)]
pub struct DecryptedHandshakeInit(HandshakeInit);

impl DecryptedHandshakeInit {
    #[inline(always)]
    pub fn static_key(&self) -> PublicKey {
        PublicKey::from(self.0.static_key.msg)
    }
    #[inline(always)]
    pub fn timestamp(&self) -> &[u8; 12] {
        &self.0.timestamp.msg
    }
}

pub fn encrypt_handshake_init(
    hs: &mut HandshakeState,
    initiator: &StaticInitiatorConfig,
    peer: &StaticPeerConfig,
    esk_i: &StaticSecret,
    now: Tai64N,
    sender: u32,
    cookie: Option<&Cookie>,
) -> HandshakeInit {
    // let ph = &mut peer.handshake;
    // let hs = &mut ph.state;

    // IKpsk2:
    // <- s
    // -> e, es, s, ss

    // <- s:
    let epk_i = PublicKey::from(esk_i);
    hs.mix_hash(peer.key.as_bytes());

    // -> e: ephemeral keypair generated by caller
    // wireguard goes off-spec here with mix-chain.
    hs.mix_chain(epk_i.as_bytes());
    hs.mix_hash(epk_i.as_bytes());

    // -> es:
    let k = hs.mix_key_dh(esk_i, &peer.key);

    // -> s:
    let static_key = EncryptedPublicKey::encrypt_and_hash(initiator.public_key.to_bytes(), hs, &k);

    // -> ss:
    let k = hs.mix_key_dh(&initiator.private_key, &peer.key);

    // payload:
    let timestamp = EncryptedTimestamp::encrypt_and_hash(now.to_bytes(), hs, &k);

    // build the message and protect with the MACs
    let mut msg = HandshakeInit {
        _type: little_endian::U32::new(MSG_FIRST),
        sender: little_endian::U32::new(sender),
        ephemeral_key: epk_i.to_bytes(),
        static_key,
        timestamp,
        mac1: [0; 16],
        mac2: [0; 16],
    };
    msg.mac1 = msg.compute_mac1(&peer.mac1_key);
    if let Some(cookie) = cookie {
        msg.mac2 = msg.compute_mac2(cookie);
    }

    msg
}

pub fn decrypt_handshake_init<'m>(
    init: &'m mut HandshakeInit,
    hs: &mut HandshakeState,
    receiver: &StaticInitiatorConfig,
) -> Result<&'m mut DecryptedHandshakeInit, CryptoError> {
    // IKpsk2:
    // <- s
    // -> e, es, s, ss

    // <- s:
    hs.mix_hash(receiver.public_key.as_bytes());

    // -> e:
    // wireguard goes off-spec here with mix-chain.
    hs.mix_chain(&init.ephemeral_key);
    hs.mix_hash(&init.ephemeral_key);

    // -> es:
    let epk_i = PublicKey::from(init.ephemeral_key);
    let k = hs.mix_key_dh(&receiver.private_key, &epk_i);

    unsafe_log!("decrypting static key");
    // -> s:
    let spk_i = init.static_key.decrypt_and_hash(hs, &k)?;
    let spk_i = PublicKey::from(*spk_i);
    unsafe_log!("decrypted public key {spk_i:?}");

    // -> ss:
    let k = hs.mix_key_dh(&receiver.private_key, &spk_i);

    unsafe_log!("decrypting payload");
    // payload:
    let _timestamp = *init.timestamp.decrypt_and_hash(hs, &k)?;

    Ok(transmute_mut!(init))
}

pub fn encrypt_handshake_resp(
    hs: &mut HandshakeState,
    data: &DecryptedHandshakeInit,
    esk_r: &StaticSecret,
    peer: &StaticPeerConfig,
    sender: u32,
    cookie: Option<&Cookie>,
) -> HandshakeResp {
    // IKpsk2:
    // <- e, ee, se, psk

    // <- e: ephemeral keypair generated by caller
    // wireguard goes off-spec here with mix-chain.
    let epk_r = PublicKey::from(esk_r);
    hs.mix_chain(epk_r.as_bytes());
    hs.mix_hash(epk_r.as_bytes());

    // <- ee
    let epk_i = PublicKey::from(data.0.ephemeral_key);
    hs.mix_dh(esk_r, &epk_i);

    // <- se
    let spk_i = PublicKey::from(data.0.static_key.msg);
    hs.mix_dh(esk_r, &spk_i);

    // <- psk
    let k = hs.mix_key_and_hash(&peer.preshared_key);

    // payload:
    let empty = EncryptedEmpty::encrypt_and_hash([], hs, &k);

    // build the message and protect with the MACs
    let mut msg = HandshakeResp {
        _type: little_endian::U32::new(MSG_SECOND),
        sender: little_endian::U32::new(sender),
        receiver: data.0.sender,
        ephemeral_key: epk_r.to_bytes(),
        empty,
        mac1: [0; 16],
        mac2: [0; 16],
    };
    msg.mac1 = msg.compute_mac1(&peer.mac1_key);
    if let Some(cookie) = cookie {
        msg.mac2 = msg.compute_mac2(cookie);
    }

    msg
}

pub fn decrypt_handshake_resp(
    resp: &mut HandshakeResp,
    hs: &mut HandshakeState,
    initiator: &StaticInitiatorConfig,
    peer: &StaticPeerConfig,
    esk_i: &StaticSecret,
) -> Result<(), CryptoError> {
    // IKpsk2:
    // <- e, ee, se, psk

    // <- e:
    // wireguard goes off-spec here with mix-chain.
    let epk_r = PublicKey::from(resp.ephemeral_key);
    hs.mix_chain(epk_r.as_bytes());
    hs.mix_hash(epk_r.as_bytes());

    // <- ee:
    hs.mix_dh(esk_i, &epk_r);

    // <- se:
    hs.mix_dh(&initiator.private_key, &epk_r);

    // <- psk:
    let k = hs.mix_key_and_hash(&peer.preshared_key);

    unsafe_log!("decrypting payload");
    // payload:
    resp.empty.decrypt_and_hash(hs, &k)?;

    Ok(())
}
