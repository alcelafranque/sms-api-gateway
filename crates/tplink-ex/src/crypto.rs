//! Cryptographic primitives of the `cgi_gdpr` scheme.
//!
//! Every request body is AES-encrypted with a per-session key chosen by the
//! client, and carries a `sign` field: an RSA encryption, under the device's
//! short-lived public key, of `h=<md5(user+pass)>&s=<seq + len(data)>`
//! (prefixed with the AES key and IV on login, which is how the device learns
//! them).
//!
//! These building blocks are exposed for tooling and interoperability checks;
//! regular users only need [`Client`](crate::Client).

use std::fmt;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use md5::{Digest, Md5};
use num_bigint::BigUint;
use rand::{Rng, RngCore};
use sha1::Sha1;

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

const GCM_TAG_LEN: usize = 16;
/// Base64 length of a 16-byte GCM tag.
const GCM_TAG_B64_LEN: usize = 24;
const SHA1_LEN: usize = 20;

/// Errors of the cryptographic layer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CryptoError {
    #[error("invalid RSA public key: {0}")]
    InvalidKey(String),
    #[error("invalid session key material: {0}")]
    InvalidSessionKey(String),
    #[error("message does not fit in an RSA block")]
    MessageTooLong,
    #[error("ciphertext is not valid base64")]
    NotBase64,
    #[error("decryption failed (wrong key, wrong cipher mode or corrupted data)")]
    DecryptFailed,
}

/// `md5(username + password)` as lowercase hex, the `h` value of every
/// signature.
pub fn credentials_hash(username: &str, password: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(username.as_bytes());
    hasher.update(password.as_bytes());
    to_hex(&hasher.finalize())
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Padding used for the RSA signature blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignaturePadding {
    /// Textbook RSA: each block is the message slice right-padded with zero
    /// bytes to the modulus length.
    Raw,
    /// RSAES-OAEP with SHA-1 and MGF1-SHA-1, empty label.
    Oaep,
}

/// The device's RSA public key, as served by `cgi/getGDPRParm`.
#[derive(Clone, PartialEq, Eq)]
pub struct RsaPublicKey {
    n: BigUint,
    e: BigUint,
    size: usize,
}

impl fmt::Debug for RsaPublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RsaPublicKey")
            .field("bits", &self.n.bits())
            .finish_non_exhaustive()
    }
}

impl RsaPublicKey {
    /// Parses the hex-encoded modulus (`nn`) and public exponent (`ee`).
    pub fn from_hex(nn: &str, ee: &str) -> Result<Self, CryptoError> {
        let nn = nn.trim();
        let ee = ee.trim();
        let n = BigUint::parse_bytes(nn.as_bytes(), 16)
            .ok_or_else(|| CryptoError::InvalidKey("modulus is not hex".into()))?;
        let e = BigUint::parse_bytes(ee.as_bytes(), 16)
            .ok_or_else(|| CryptoError::InvalidKey("exponent is not hex".into()))?;
        if n.bits() < 256 {
            return Err(CryptoError::InvalidKey(format!(
                "modulus too short ({} bits)",
                n.bits()
            )));
        }
        if e < BigUint::from(3u8) {
            return Err(CryptoError::InvalidKey("exponent too small".into()));
        }
        // Block size follows the advertised hex length, like the web UI does.
        let size = nn.len().div_ceil(2);
        Ok(Self { n, e, size })
    }

    /// Modulus length in bytes (64 for the usual 512-bit device keys).
    pub fn size(&self) -> usize {
        self.size
    }

    fn encrypt_int(&self, block: &[u8]) -> Result<String, CryptoError> {
        let m = BigUint::from_bytes_be(block);
        if m >= self.n {
            return Err(CryptoError::MessageTooLong);
        }
        let c = m.modpow(&self.e, &self.n);
        Ok(format!(
            "{:0>width$}",
            c.to_str_radix(16),
            width = self.size * 2
        ))
    }

    fn encrypt_raw(&self, chunk: &[u8]) -> Result<String, CryptoError> {
        if chunk.len() > self.size {
            return Err(CryptoError::MessageTooLong);
        }
        let mut block = chunk.to_vec();
        block.resize(self.size, 0);
        self.encrypt_int(&block)
    }

    fn encrypt_oaep(&self, msg: &[u8], rng: &mut impl RngCore) -> Result<String, CryptoError> {
        let k = self.size;
        if k < 2 * SHA1_LEN + 2 || msg.len() > k - 2 * SHA1_LEN - 2 {
            return Err(CryptoError::MessageTooLong);
        }
        // DB = lHash || PS || 0x01 || M
        let mut db = Vec::with_capacity(k - SHA1_LEN - 1);
        db.extend_from_slice(&Sha1::digest(b""));
        db.resize(k - msg.len() - SHA1_LEN - 2, 0);
        db.push(0x01);
        db.extend_from_slice(msg);

        let mut seed = [0u8; SHA1_LEN];
        rng.fill_bytes(&mut seed);
        xor_in_place(&mut db, &mgf1_sha1(&seed, k - SHA1_LEN - 1));
        xor_in_place(&mut seed, &mgf1_sha1(&db, SHA1_LEN));

        let mut em = Vec::with_capacity(k);
        em.push(0);
        em.extend_from_slice(&seed);
        em.extend_from_slice(&db);
        self.encrypt_int(&em)
    }

    /// Encrypts `text` block by block and concatenates the fixed-width hex
    /// blocks, which is the format of the `sign` field.
    pub fn sign(&self, padding: SignaturePadding, text: &str) -> Result<String, CryptoError> {
        self.sign_with_rng(padding, text, &mut rand::rng())
    }

    fn sign_with_rng(
        &self,
        padding: SignaturePadding,
        text: &str,
        rng: &mut impl RngCore,
    ) -> Result<String, CryptoError> {
        let step = match padding {
            SignaturePadding::Raw => self.size,
            SignaturePadding::Oaep => self
                .size
                .checked_sub(2 * SHA1_LEN + 2)
                .filter(|s| *s > 0)
                .ok_or(CryptoError::MessageTooLong)?,
        };
        let mut out = String::new();
        for chunk in text.as_bytes().chunks(step) {
            out.push_str(&match padding {
                SignaturePadding::Raw => self.encrypt_raw(chunk)?,
                SignaturePadding::Oaep => self.encrypt_oaep(chunk, rng)?,
            });
        }
        Ok(out)
    }
}

fn xor_in_place(dst: &mut [u8], mask: &[u8]) {
    for (d, m) in dst.iter_mut().zip(mask) {
        *d ^= m;
    }
}

fn mgf1_sha1(seed: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + SHA1_LEN);
    let mut counter: u32 = 0;
    while out.len() < len {
        let mut h = Sha1::new();
        h.update(seed);
        h.update(counter.to_be_bytes());
        out.extend_from_slice(&h.finalize());
        counter += 1;
    }
    out.truncate(len);
    out
}

/// AES mode of the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMode {
    /// AES-128-CBC with PKCS#7 padding, 16-byte IV.
    Cbc,
    /// AES-128-GCM, 12-byte nonce, detached 16-byte tag.
    Gcm,
}

/// Encrypted request payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    /// Base64 ciphertext (the `data=` field).
    pub data: String,
    /// Base64 GCM tag (the `tag=` field), GCM only.
    pub tag: Option<String>,
}

/// Session AES key and IV/nonce. Both are ASCII digit strings whose bytes
/// are used directly as key material.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionCipher {
    mode: SessionMode,
    key: String,
    iv: String,
}

impl fmt::Debug for SessionCipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionCipher")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl SessionCipher {
    const KEY_LEN: usize = 16;

    fn iv_len(mode: SessionMode) -> usize {
        match mode {
            SessionMode::Cbc => 16,
            SessionMode::Gcm => 12,
        }
    }

    /// Generates key material the way the device web UI does: the current
    /// time in milliseconds followed by a random 9-digit number, truncated to
    /// the key / IV length (key and IV are drawn independently).
    pub fn generate(mode: SessionMode) -> Self {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let mut rng = rand::rng();
        let mut draw = |len: usize| {
            let r: u32 = rng.random_range(100_000_000..1_000_000_000);
            let mut s = format!("{ts}{r}");
            s.truncate(len);
            s
        };
        let key = draw(Self::KEY_LEN);
        let iv = draw(Self::iv_len(mode));
        Self { mode, key, iv }
    }

    /// Builds a cipher from explicit key material (16-byte key, 16-byte IV
    /// for CBC or 12-byte nonce for GCM).
    pub fn from_parts(mode: SessionMode, key: &str, iv: &str) -> Result<Self, CryptoError> {
        if key.len() != Self::KEY_LEN {
            return Err(CryptoError::InvalidSessionKey(format!(
                "key must be {} bytes",
                Self::KEY_LEN
            )));
        }
        if iv.len() != Self::iv_len(mode) {
            return Err(CryptoError::InvalidSessionKey(format!(
                "iv must be {} bytes for {mode:?}",
                Self::iv_len(mode)
            )));
        }
        Ok(Self {
            mode,
            key: key.to_owned(),
            iv: iv.to_owned(),
        })
    }

    pub fn mode(&self) -> SessionMode {
        self.mode
    }

    /// The `key=...&iv=...` prefix of the login signature. GCM firmwares
    /// expect both values base64-encoded, CBC ones as plain strings.
    pub fn login_params(&self) -> String {
        match self.mode {
            SessionMode::Cbc => format!("key={}&iv={}", self.key, self.iv),
            SessionMode::Gcm => format!(
                "key={}&iv={}",
                B64.encode(self.key.as_bytes()),
                B64.encode(self.iv.as_bytes())
            ),
        }
    }

    /// Encrypts a request payload.
    pub fn seal(&self, plaintext: &str) -> Sealed {
        let key = self.key.as_bytes();
        let iv = self.iv.as_bytes();
        match self.mode {
            SessionMode::Cbc => {
                let enc = Aes128CbcEnc::new_from_slices(key, iv).expect("lengths checked");
                let ct = enc.encrypt_padded_vec_mut::<Pkcs7>(plaintext.as_bytes());
                Sealed {
                    data: B64.encode(ct),
                    tag: None,
                }
            }
            SessionMode::Gcm => {
                let cipher = Aes128Gcm::new_from_slice(key).expect("length checked");
                let mut ct = cipher
                    .encrypt(Nonce::from_slice(iv), plaintext.as_bytes())
                    .expect("AES-GCM encryption of in-memory data cannot fail");
                let tag = ct.split_off(ct.len() - GCM_TAG_LEN);
                Sealed {
                    data: B64.encode(ct),
                    tag: Some(B64.encode(tag)),
                }
            }
        }
    }

    /// Decrypts a response body. GCM responses are the base64 ciphertext
    /// immediately followed by the base64 tag (last 24 characters).
    pub fn open(&self, body: &str) -> Result<String, CryptoError> {
        let body: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        let key = self.key.as_bytes();
        let iv = self.iv.as_bytes();
        let plain = match self.mode {
            SessionMode::Cbc => {
                let ct = B64.decode(&body).map_err(|_| CryptoError::NotBase64)?;
                Aes128CbcDec::new_from_slices(key, iv)
                    .expect("lengths checked")
                    .decrypt_padded_vec_mut::<Pkcs7>(&ct)
                    .map_err(|_| CryptoError::DecryptFailed)?
            }
            SessionMode::Gcm => {
                if body.len() < GCM_TAG_B64_LEN
                    || !body.is_char_boundary(body.len() - GCM_TAG_B64_LEN)
                {
                    return Err(CryptoError::NotBase64);
                }
                let (ct, tag) = body.split_at(body.len() - GCM_TAG_B64_LEN);
                let mut payload = B64.decode(ct).map_err(|_| CryptoError::NotBase64)?;
                let tag = B64.decode(tag).map_err(|_| CryptoError::NotBase64)?;
                if tag.len() != GCM_TAG_LEN {
                    return Err(CryptoError::NotBase64);
                }
                payload.extend_from_slice(&tag);
                Aes128Gcm::new_from_slice(key)
                    .expect("length checked")
                    .decrypt(Nonce::from_slice(iv), payload.as_slice())
                    .map_err(|_| CryptoError::DecryptFailed)?
            }
        };
        Ok(String::from_utf8_lossy(&plain).into_owned())
    }
}

/// Builds the plaintext that goes into `sign`: `[key=..&iv=..&]h=<hash>&s=<seq>`.
///
/// `seq` must already include the length of the base64 `data` field.
pub fn signature_text(login: Option<&SessionCipher>, hash: &str, seq: u64) -> String {
    match login {
        Some(cipher) => format!("{}&h={hash}&s={seq}", cipher.login_params()),
        None => format!("h={hash}&s={seq}"),
    }
}

#[cfg(test)]
mod tests {
    //! Known-answer vectors produced by the Python reference implementation
    //! (`tplinkrouterc6u.common.encryption`, pycryptodome) for a fixed test
    //! key pair and fixed session material.

    use super::*;

    const NN: &str = concat!(
        "808a338df1ae2de086ee020dca55c7134206a0aa1dd261ab28afd8c328ab1e7c",
        "506e32a08c3917f74561265e4075aa4ef72a6b1818e6f08fab97d2400988ae65"
    );
    const CBC_DATA: &str = concat!(
        "062sNxz+e25AqffgJdFxUnoQmH29CMhpGTV4jusuASEM6ErJ4fOC4R++xbbk21mQ",
        "kOHWZQWAH6qHzMUjg7722fDXeT/gmVYnIbg8FtLj02KXYHaD0QGug+bvhY5kFq/c",
        "l625PnZPvzPS034c1cwA3broDU0BobfjhH8mwQvF50hFt7tv+hZKFEI+dWi+5ZQj",
        "g46EpeI8LgvQghGNzrnp1g=="
    );
    const GCM_DATA: &str = concat!(
        "jAlbkO55Q+wjWG++nYC5da864ij8bFE1rEtIWemlgx3gwZxBjFy0KLAUvs0FubAu",
        "RZ3+iC3ihxuUVGMKQoiGytWYCLHeoeyPoD5gRdevRT42ejbxjn/QDVRW3Nyjy5yc",
        "F743a48e7zwMmSa0rmyG+VYWaZGBeJ/CoHF1X717qghVH/D/7xZwpSZ11aoiYq0S",
        "mMTeoP732FA9ELo="
    );
    const CBC_SIGN_LOGIN: &str = concat!(
        "073ae29bf6c9424bafd6356218d3ecccd89360f5aea14beec924dcee5986515f",
        "6e14e17f5c56057ed1ce4ffaae93304b07ef6fb340d0b68e995fe7e68efc2e62",
        "379494ce5230a248b4b1de2efa0db60073c8eeab44e5b722d0407f650704f5e0",
        "15e04ac46a0dc7579e6938c48612acfe6cb76c8afac7d4710f4840a1bfc2de8c"
    );
    const CBC_SIGN: &str = concat!(
        "622332008cd50146c67da5c44feba6747c794310336c354d94300e24803975c3",
        "0396e8b83792dc797cdacc7244744e5c6fa365676e103bbb36c7d79c9cf02374"
    );
    const GCM_SIGN_LOGIN: &str = concat!(
        "24f62281397028c3942b42be9762545705af67e02d1d2d42e9437d220605d91a",
        "cfc9745972f643aeccf3dfd254cde88fd5d3c26a7f5b9d6a7210885a917848ad",
        "117a691179c9ca0cd3d88edc924faf49a9b5d1fb8b785c146d615dc00dae550f",
        "4ffec30f2cc4be971d240cb27d8304753f5b746fd2488c1934647dd17768e567"
    );
    const OAEP_SIGN_LOGIN: &str = concat!(
        "03f36bdce715fe8d5ea1871c93700ad0510f8a20cf19175ebcf892350ef2bead",
        "e1811494b99334572df3c82ba34b01a01db7ca6ea4d870a4989ce44e2f36fb79",
        "5047123852e3e60897773d14e5ea5da24377c56ea30e263d1b061436f3bcc1f1",
        "75a427f84e1618976cfe89ee284c692ae50c51b1b933686028ab2ef9ba47da99",
        "0f748f3aa6f38f9c2f2a332564cf0a81db7da5a2ac53f560ad072748b9ae2907",
        "4070819939e596fcc2dc8a7c5926db39f7ca660eeb537dd8146b62aa1880f7a9",
        "50a0474430c3b27cfe4c1161f02e164cd324475075053f21910a3e80bd14fc68",
        "64a6a232063e9fd422af2f4bcaf738201809944d4e95a5396843ee21426dffcc",
        "2f607949394f62fa1c8001a34fc679da6c56793596c76cee436284b93ad130ca",
        "4e0677e9aeb02caf5732214fc0974519a7aaacf96d7c7d905265ba064d9d0428"
    );
    const CBC_REPLY: &str = concat!(
        "P8eRyarq9HSzkbFD2IXJi5gqENIG+4mzZJW/tQjule/alMAEJbis2Y6kUSC4R+II",
        "GIbWUIt5dwzKItP7RrcvZI10+vepgiUxC/kCrkiUm1zuWZcFn2i+Ra1agLNW1LXH"
    );
    const GCM_REPLY: &str = concat!(
        "jAlbkO55Q+wjWGiliIK+GeB1sGG+Yls7rkUFWfvo31ri05xJgFm0KLBD/d8Fq/N3",
        "DY/omkuL812kOg1tNuj4r7v4fsa92ojwwF0aE571Qgg6dyfnkynIQwRLegg=3yUx",
        "JHYIopOlbVZK1Gtr3Q=="
    );
    const EE: &str = "010001";
    const HASH: &str = "644999c38677dacac31bf7eef05b7bfe";
    const GCM_TAG: &str = "IrDa6sH4sZvy5CL51lw23Q==";
    const KEY: &str = "1732190000123456";
    const IV16: &str = "1732190000987654";
    const IV12: &str = "173219000012";
    const SEQ: u64 = 123_456_789;
    const PLAIN: &str = concat!(
        r#"{"data":{"stack":"0,0,0,0,0,0","pstack":"0,0,0,0,0,0","to":"+33612345678","#,
        r#""textContent":"héllo €😀"},"operation":"so","oid":"DEV2_LTE_SMS_SENDNEWMSG"}"#
    );
    const REPLY: &str = concat!(
        r#"{"data":{"totalNumber":"2"},"operation":"go","#,
        r#""oid":"DEV2_LTE_SMS_RECVMSGBOX","success":true}"#
    );

    fn rsa() -> RsaPublicKey {
        RsaPublicKey::from_hex(NN, EE).unwrap()
    }

    /// Yields the byte stream 0, 1, 2... like the reference run's `randfunc`.
    struct CountingRng(u8);

    impl RngCore for CountingRng {
        fn next_u32(&mut self) -> u32 {
            let mut b = [0; 4];
            self.fill_bytes(&mut b);
            u32::from_le_bytes(b)
        }
        fn next_u64(&mut self) -> u64 {
            let mut b = [0; 8];
            self.fill_bytes(&mut b);
            u64::from_le_bytes(b)
        }
        fn fill_bytes(&mut self, dst: &mut [u8]) {
            for b in dst {
                *b = self.0;
                self.0 = self.0.wrapping_add(1);
            }
        }
    }

    #[test]
    fn credentials_hash_matches_reference() {
        assert_eq!(credentials_hash("user", "p@ss wörd"), HASH);
    }

    #[test]
    fn cbc_session_matches_reference() {
        let c = SessionCipher::from_parts(SessionMode::Cbc, KEY, IV16).unwrap();
        let sealed = c.seal(PLAIN);
        assert_eq!(sealed.data, CBC_DATA);
        assert_eq!(sealed.tag, None);
        let seq = SEQ + sealed.data.len() as u64;
        let login = signature_text(Some(&c), HASH, seq);
        assert!(login.starts_with("key=1732190000123456&iv=1732190000987654&h="));
        assert_eq!(
            rsa().sign(SignaturePadding::Raw, &login).unwrap(),
            CBC_SIGN_LOGIN
        );
        let regular = signature_text(None, HASH, seq);
        assert_eq!(
            rsa().sign(SignaturePadding::Raw, &regular).unwrap(),
            CBC_SIGN
        );
        assert_eq!(c.open(CBC_REPLY).unwrap(), REPLY);
    }

    #[test]
    fn gcm_session_matches_reference() {
        let c = SessionCipher::from_parts(SessionMode::Gcm, KEY, IV12).unwrap();
        let sealed = c.seal(PLAIN);
        assert_eq!(sealed.data, GCM_DATA);
        assert_eq!(sealed.tag.as_deref(), Some(GCM_TAG));
        let seq = SEQ + sealed.data.len() as u64;
        // GCM firmwares take base64 key material in the login signature.
        let login = signature_text(Some(&c), HASH, seq);
        assert!(login.starts_with("key=MTczMjE5MDAwMDEyMzQ1Ng==&iv=MTczMjE5MDAwMDEy&h="));
        assert_eq!(
            rsa().sign(SignaturePadding::Raw, &login).unwrap(),
            GCM_SIGN_LOGIN
        );
        // Replies are the base64 ciphertext followed by the base64 tag.
        assert_eq!(c.open(GCM_REPLY).unwrap(), REPLY);
    }

    #[test]
    fn oaep_signature_matches_reference_with_same_seeds() {
        let c = SessionCipher::from_parts(SessionMode::Gcm, KEY, IV12).unwrap();
        let seq = SEQ + c.seal(PLAIN).data.len() as u64;
        let text = signature_text(Some(&c), HASH, seq);
        let sign = rsa()
            .sign_with_rng(SignaturePadding::Oaep, &text, &mut CountingRng(0))
            .unwrap();
        // 22-byte chunks on a 512-bit key: five 64-byte blocks.
        assert_eq!(sign.len(), 5 * 128);
        assert_eq!(sign, OAEP_SIGN_LOGIN);
    }

    #[test]
    fn tampered_or_foreign_ciphertexts_are_rejected() {
        let gcm = SessionCipher::from_parts(SessionMode::Gcm, KEY, IV12).unwrap();
        let mut tampered = GCM_REPLY.to_owned();
        tampered.replace_range(0..1, if tampered.starts_with('A') { "B" } else { "A" });
        assert_eq!(gcm.open(&tampered), Err(CryptoError::DecryptFailed));
        // A CBC reply is not a valid GCM reply for the same key.
        assert!(gcm.open(CBC_REPLY).is_err());
        let other = SessionCipher::from_parts(SessionMode::Cbc, "0000000000000000", IV16).unwrap();
        assert_ne!(other.open(CBC_REPLY).ok().as_deref(), Some(REPLY));
        assert_eq!(gcm.open("<html>403</html>"), Err(CryptoError::NotBase64));
    }

    #[test]
    fn generated_key_material_has_the_expected_shape() {
        for mode in [SessionMode::Cbc, SessionMode::Gcm] {
            let c = SessionCipher::generate(mode);
            assert_eq!(c.key.len(), 16);
            assert_eq!(c.iv.len(), if mode == SessionMode::Cbc { 16 } else { 12 });
            assert!(
                c.key
                    .bytes()
                    .chain(c.iv.bytes())
                    .all(|b| b.is_ascii_digit())
            );
            let sealed = c.seal("round trip");
            let body = sealed.data + sealed.tag.as_deref().unwrap_or("");
            assert_eq!(c.open(&body).unwrap(), "round trip");
        }
    }
}
