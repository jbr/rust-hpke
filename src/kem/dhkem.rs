use crate::{
    dhkex::{DhKeyExchange, MAX_PUBKEY_SIZE},
    kdf::{extract_and_expand, Kdf as KdfTrait},
    kem::{Kem as KemTrait, SharedSecret},
    util::kem_suite_id,
    Deserializable, HpkeError, Serializable,
};

use digest::FixedOutput;
use generic_array::GenericArray;
use paste::paste;
use rand_core::{CryptoRng, RngCore};

/// Defines DHKEM(G, K) given a Diffie-Hellman group G and KDF K
macro_rules! impl_dhkem {
    // Top-level case. Creates the ident for the encapped key type and calls to the base case
    (
        $kem_name:ident,
        $dhkex:ty,
        $kdf:ty,
        $kem_id:literal,
        $doc_str:expr
    ) => {
        paste! {
            impl_dhkem!(
                $kem_name,
                [<$kem_name EncappedKey>],
                $dhkex,
                $kdf,
                $kem_id,
                $doc_str
            );
        }
    };

    // Base case. Implements the given KEM with the given KDF type, encapped key type, DHKEX types,
    // etc.
    (
        $kem_name:ident,
        $encapped_key:ident,
        $dhkex:ty,
        $kdf:ty,
        $kem_id:literal,
        $doc_str:expr
    ) => {
        /// Holds the content of an encapsulated secret. This is what the receiver uses to derive
        /// the shared secret. This just wraps a pubkey, because that's all an encapsulated key is
        /// in a DH-KEM
        #[doc(hidden)]
        #[derive(Clone)]
        pub struct $encapped_key(pub(crate) <$dhkex as DhKeyExchange>::PublicKey);

        // EncappedKeys need to be serializable, since they're gonna be sent over the wire.
        // Underlyingly, they're just DH pubkeys, so we just serialize them the same way
        impl Serializable for $encapped_key {
            type OutputSize = <<$dhkex as DhKeyExchange>::PublicKey as Serializable>::OutputSize;

            // Pass to underlying to_bytes() impl
            fn to_bytes(&self) -> GenericArray<u8, Self::OutputSize> {
                self.0.to_bytes()
            }
        }

        impl Deserializable for $encapped_key {
            // Pass to underlying from_bytes() impl
            fn from_bytes(encoded: &[u8]) -> Result<Self, HpkeError> {
                let pubkey =
                    <<$dhkex as DhKeyExchange>::PublicKey as Deserializable>::from_bytes(encoded)?;
                Ok($encapped_key(pubkey))
            }
        }

        // Define the KEM struct
        #[doc = $doc_str]
        pub struct $kem_name;

        impl KemTrait for $kem_name {
            // draft12 §4.1
            // For the variants of DHKEM defined in this document, the size "Nsecret" of the KEM
            // shared secret is equal to the output length of the hash function underlying the KDF

            /// The size of the shared secret at the end of the key exchange process
            #[doc(hidden)]
            type NSecret = <<$kdf as KdfTrait>::HashImpl as FixedOutput>::OutputSize;

            // draft12 §4.1
            // The function parameters "pkR" and "pkS" are deserialized public keys, and
            // "enc" is a serialized public key.  Since encapsulated keys are Diffie-Hellman public
            // keys in this KEM algorithm, we use "SerializePublicKey()" and
            // "DeserializePublicKey()" to encode and decode them, respectively.  "Npk" equals
            // "Nenc". "GenerateKeyPair()" produces a key pair for the Diffie-Hellman group in use.
            // Section 7.1.3 contains the "DeriveKeyPair()" function specification for DHKEMs
            // defined in this document.
            type PublicKey = <$dhkex as DhKeyExchange>::PublicKey;
            type PrivateKey = <$dhkex as DhKeyExchange>::PrivateKey;
            type EncappedKey = $encapped_key;

            const KEM_ID: u16 = $kem_id;

            /// Deterministically derives a keypair from the given input keying material
            ///
            /// Requirements
            /// ============
            /// This keying material SHOULD have as many bits of entropy as the bit length of a
            /// secret key, i.e., `8 * Self::PrivateKey::size()`. For X25519 and P-256, this is
            /// 256 bits of entropy.
            fn derive_keypair(ikm: &[u8]) -> (Self::PrivateKey, Self::PublicKey) {
                let suite_id = kem_suite_id::<Self>();
                <$dhkex as DhKeyExchange>::derive_keypair::<$kdf>(&suite_id, ikm)
            }

            /// Generates a random keypair using the given RNG
            fn gen_keypair<R: CryptoRng + RngCore>(
                csprng: &mut R,
            ) -> (Self::PrivateKey, Self::PublicKey) {
                // Make some keying material that's the size of a private key
                let mut ikm: GenericArray<u8, <Self::PrivateKey as Serializable>::OutputSize> =
                    GenericArray::default();
                // Fill it with randomness
                csprng.fill_bytes(&mut ikm);
                // Run derive_keypair using the KEM's KDF
                Self::derive_keypair(&ikm)
            }

            // draft12 §4.1
            // def Encap(pkR):
            //   skE, pkE = GenerateKeyPair()
            //   dh = DH(skE, pkR)
            //   enc = SerializePublicKey(pkE)
            //
            //   pkRm = SerializePublicKey(pkR)
            //   kem_context = concat(enc, pkRm)
            //
            // def AuthEncap(pkR, skS):
            //   skE, pkE = GenerateKeyPair()
            //   dh = concat(DH(skE, pkR), DH(skS, pkR))
            //   enc = SerializePublicKey(pkE)
            //
            //   pkRm = SerializePublicKey(pkR)
            //   pkSm = SerializePublicKey(pk(skS))
            //   kem_context = concat(enc, pkRm, pkSm)
            //
            //   shared_secret = ExtractAndExpand(dh, kem_context)
            //   return shared_secret, enc

            /// Derives a shared secret that the owner of the recipient's pubkey can use to derive
            /// the same shared secret. If `sk_sender_id` is given, the sender's identity will be
            /// tied to the shared secret.
            ///
            /// Return Value
            /// ============
            /// Returns a shared secret and encapped key on success. If an error happened during
            /// key exchange, returns `Err(HpkeError::EncapError)`.
            #[doc(hidden)]
            fn encap_with_eph(
                pk_recip: &Self::PublicKey,
                sender_id_keypair: Option<&(Self::PrivateKey, Self::PublicKey)>,
                sk_eph: Self::PrivateKey,
            ) -> Result<(SharedSecret<Self>, Self::EncappedKey), HpkeError> {
                // Put together the binding context used for all KDF operations
                let suite_id = kem_suite_id::<Self>();

                // Compute the shared secret from the ephemeral inputs
                let kex_res_eph = <$dhkex as DhKeyExchange>::dh(&sk_eph, pk_recip)
                    .map_err(|_| HpkeError::EncapError)?;

                // The encapped key is the ephemeral pubkey
                let encapped_key = {
                    let pk_eph = <$dhkex as DhKeyExchange>::sk_to_pk(&sk_eph);
                    $encapped_key(pk_eph)
                };

                // The shared secret is either gonna be kex_res_eph, or that along with another
                // shared secret that's tied to the sender's identity.
                let shared_secret = if let Some((sk_sender_id, pk_sender_id)) = sender_id_keypair {
                    // kem_context = encapped_key || pk_recip || pk_sender_id
                    // We concat without allocation by making a buffer of the maximum possible
                    // size, then taking the appropriately sized slice.
                    let (kem_context_buf, kem_context_size) = concat_with_known_maxlen!(
                        MAX_PUBKEY_SIZE,
                        &encapped_key.to_bytes(),
                        &pk_recip.to_bytes(),
                        &pk_sender_id.to_bytes()
                    );
                    let kem_context = &kem_context_buf[..kem_context_size];

                    // We want to do an authed encap. Do a DH exchange between the sender identity
                    // secret key and the recipient's pubkey
                    let kex_res_identity = <$dhkex as DhKeyExchange>::dh(sk_sender_id, pk_recip)
                        .map_err(|_| HpkeError::EncapError)?;

                    // concatted_secrets = kex_res_eph || kex_res_identity
                    // Same no-alloc concat trick as above
                    let (concatted_secrets_buf, concatted_secret_size) = concat_with_known_maxlen!(
                        MAX_PUBKEY_SIZE,
                        &kex_res_eph.to_bytes(),
                        &kex_res_identity.to_bytes()
                    );
                    let concatted_secrets = &concatted_secrets_buf[..concatted_secret_size];

                    // The "authed shared secret" is derived from the KEX of the ephemeral input
                    // with the recipient pubkey, and the KEX of the identity input with the
                    // recipient pubkey. The HKDF-Expand call only errors if the output values are
                    // 255x the digest size of the hash function. Since these values are fixed at
                    // compile time, we don't worry about it.
                    let mut buf = <SharedSecret<Self> as Default>::default();
                    extract_and_expand::<$kdf>(
                        concatted_secrets,
                        &suite_id,
                        kem_context,
                        &mut buf.0,
                    )
                    .expect("shared secret is way too big");
                    buf
                } else {
                    // kem_context = encapped_key || pk_recip
                    // We concat without allocation by making a buffer of the maximum possible
                    // size, then taking the appropriately sized slice.
                    let (kem_context_buf, kem_context_size) = concat_with_known_maxlen!(
                        MAX_PUBKEY_SIZE,
                        &encapped_key.to_bytes(),
                        &pk_recip.to_bytes()
                    );
                    let kem_context = &kem_context_buf[..kem_context_size];

                    // The "unauthed shared secret" is derived from just the KEX of the ephemeral
                    // input with the recipient pubkey. The HKDF-Expand call only errors if the
                    // output values are 255x the digest size of the hash function. Since these
                    // values are fixed at compile time, we don't worry about it.
                    let mut buf = <SharedSecret<Self> as Default>::default();
                    extract_and_expand::<$kdf>(
                        &kex_res_eph.to_bytes(),
                        &suite_id,
                        kem_context,
                        &mut buf.0,
                    )
                    .expect("shared secret is way too big");
                    buf
                };

                Ok((shared_secret, encapped_key))
            }

            // draft11 §4.1
            // def Decap(enc, skR):
            //   pkE = DeserializePublicKey(enc)
            //   dh = DH(skR, pkE)
            //
            //   pkRm = SerializePublicKey(pk(skR))
            //   kem_context = concat(enc, pkRm)
            //
            //   shared_secret = ExtractAndExpand(dh, kem_context)
            //   return shared_secret
            //
            // def AuthDecap(enc, skR, pkS):
            //   pkE = DeserializePublicKey(enc)
            //   dh = concat(DH(skR, pkE), DH(skR, pkS))
            //
            //   pkRm = SerializePublicKey(pk(skR))
            //   pkSm = SerializePublicKey(pkS)
            //   kem_context = concat(enc, pkRm, pkSm)
            //
            //   shared_secret = ExtractAndExpand(dh, kem_context)
            //   return shared_secret

            /// Derives a shared secret given the encapsulated key and the recipients secret key.
            /// If `pk_sender_id` is given, the sender's identity will be tied to the shared
            /// secret.
            ///
            /// Return Value
            /// ============
            /// Returns a shared secret on success. If an error happened during key exchange,
            /// returns `Err(HpkeError::DecapError)`.
            #[doc(hidden)]
            fn decap(
                sk_recip: &Self::PrivateKey,
                pk_sender_id: Option<&Self::PublicKey>,
                encapped_key: &Self::EncappedKey,
            ) -> Result<SharedSecret<Self>, HpkeError> {
                // Put together the binding context used for all KDF operations
                let suite_id = kem_suite_id::<Self>();

                // Compute the shared secret from the ephemeral inputs
                let kex_res_eph = <$dhkex as DhKeyExchange>::dh(sk_recip, &encapped_key.0)
                    .map_err(|_| HpkeError::DecapError)?;

                // Compute the sender's pubkey from their privkey
                let pk_recip = <$dhkex as DhKeyExchange>::sk_to_pk(sk_recip);

                // The shared secret is either gonna be kex_res_eph, or that along with another
                // shared secret that's tied to the sender's identity.
                if let Some(pk_sender_id) = pk_sender_id {
                    // kem_context = encapped_key || pk_recip || pk_sender_id We concat without
                    // allocation by making a buffer of the maximum possible size, then taking the
                    // appropriately sized slice.
                    let (kem_context_buf, kem_context_size) = concat_with_known_maxlen!(
                        MAX_PUBKEY_SIZE,
                        &encapped_key.to_bytes(),
                        &pk_recip.to_bytes(),
                        &pk_sender_id.to_bytes()
                    );
                    let kem_context = &kem_context_buf[..kem_context_size];

                    // We want to do an authed encap. Do a DH exchange between the sender identity
                    // secret key and the recipient's pubkey
                    let kex_res_identity = <$dhkex as DhKeyExchange>::dh(sk_recip, pk_sender_id)
                        .map_err(|_| HpkeError::DecapError)?;

                    // concatted_secrets = kex_res_eph || kex_res_identity
                    // Same no-alloc concat trick as above
                    let (concatted_secrets_buf, concatted_secret_size) = concat_with_known_maxlen!(
                        MAX_PUBKEY_SIZE,
                        &kex_res_eph.to_bytes(),
                        &kex_res_identity.to_bytes()
                    );
                    let concatted_secrets = &concatted_secrets_buf[..concatted_secret_size];

                    // The "authed shared secret" is derived from the KEX of the ephemeral input
                    // with the recipient pubkey, and the kex of the identity input with the
                    // recipient pubkey. The HKDF-Expand call only errors if the output values are
                    // 255x the digest size of the hash function. Since these values are fixed at
                    // compile time, we don't worry about it.
                    let mut shared_secret = <SharedSecret<Self> as Default>::default();
                    extract_and_expand::<$kdf>(
                        concatted_secrets,
                        &suite_id,
                        kem_context,
                        &mut shared_secret.0,
                    )
                    .expect("shared secret is way too big");
                    Ok(shared_secret)
                } else {
                    // kem_context = encapped_key || pk_recip || pk_sender_id
                    // We concat without allocation by making a buffer of the maximum possible
                    // size, then taking the appropriately sized slice.
                    let (kem_context_buf, kem_context_size) = concat_with_known_maxlen!(
                        MAX_PUBKEY_SIZE,
                        &encapped_key.to_bytes(),
                        &pk_recip.to_bytes()
                    );
                    let kem_context = &kem_context_buf[..kem_context_size];

                    // The "unauthed shared secret" is derived from just the KEX of the ephemeral
                    // input with the recipient pubkey. The HKDF-Expand call only errors if the
                    // output values are 255x the digest size of the hash function. Since these
                    // values are fixed at compile time, we don't worry about it.
                    let mut shared_secret = <SharedSecret<Self> as Default>::default();
                    extract_and_expand::<$kdf>(
                        &kex_res_eph.to_bytes(),
                        &suite_id,
                        kem_context,
                        &mut shared_secret.0,
                    )
                    .expect("shared secret is way too big");
                    Ok(shared_secret)
                }
            }
        }
    };
}

// Implement DHKEM(X25519, HKDF-SHA256)
#[cfg(feature = "x25519-dalek")]
impl_dhkem!(
    X25519HkdfSha256,
    crate::dhkex::x25519::X25519,
    crate::kdf::HkdfSha256,
    0x0020,
    "Represents DHKEM(X25519, HKDF-SHA256)"
);

// Implement DHKEM(P-256, HKDF-SHA256)
#[cfg(feature = "p256")]
impl_dhkem!(
    DhP256HkdfSha256,
    crate::dhkex::ecdh_nistp::DhP256,
    crate::kdf::HkdfSha256,
    0x0010,
    "Represents DHKEM(P-256, HKDF-SHA256)"
);
