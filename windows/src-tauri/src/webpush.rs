// Web Push without OpenSSL: the message is encrypted with `aes128gcm`
// (RFC 8291) and signed for the push service with a VAPID JWT (RFC 8292),
// all with RustCrypto. The push service only ever sees ciphertext.
//
// One record, one message: our payloads are a title and a line, far below the
// 4096-byte record size.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use hkdf::Hkdf;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Sha256;

const RECORD_SIZE: u32 = 4096;
/// Header (86) + delimiter + GCM tag must still fit in one record.
const MAX_PLAINTEXT: usize = 3800;
/// RFC 8292 caps a VAPID token at 24 h; half that leaves room for clock skew.
const JWT_LIFETIME_S: u64 = 12 * 60 * 60;
/// The contact the push services may use about our traffic.
const SUBJECT: &str = "https://github.com/cr1st1anhernandez/coucou";

/// A browser's `PushSubscription.toJSON()`; `expirationTime` is ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub endpoint: String,
    pub keys: SubscriptionKeys,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionKeys {
    pub p256dh: String,
    pub auth: String,
}

fn random<const N: usize>() -> Result<[u8; N], String> {
    let mut buf = [0u8; N];
    getrandom::getrandom(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

fn random_secret() -> Result<SecretKey, String> {
    // A random 32-byte string is a valid P-256 scalar except with negligible odds.
    loop {
        if let Ok(key) = SecretKey::from_slice(&random::<32>()?) {
            return Ok(key);
        }
    }
}

/// Encrypts one push message for a subscription (RFC 8291 §3–4).
pub fn encrypt(plaintext: &[u8], sub: &Subscription) -> Result<Vec<u8>, String> {
    let ua_public = B64.decode(sub.keys.p256dh.trim_end_matches('=')).map_err(|e| e.to_string())?;
    let auth = B64.decode(sub.keys.auth.trim_end_matches('=')).map_err(|e| e.to_string())?;
    encrypt_with(plaintext, &ua_public, &auth, &random_secret()?, &random::<16>()?)
}

fn encrypt_with(
    plaintext: &[u8],
    ua_public: &[u8],
    auth_secret: &[u8],
    as_secret: &SecretKey,
    salt: &[u8; 16],
) -> Result<Vec<u8>, String> {
    if plaintext.len() > MAX_PLAINTEXT {
        return Err("push payload too large".into());
    }
    let ua = PublicKey::from_sec1_bytes(ua_public).map_err(|_| "bad p256dh key".to_string())?;
    let as_public = as_secret.public_key().to_encoded_point(false);
    let shared = p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), ua.as_affine());

    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public.as_bytes());
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth_secret), shared.raw_secret_bytes())
        .expand(&key_info, &mut ikm)
        .map_err(|e| e.to_string())?;

    let hk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    let mut nonce = [0u8; 12];
    hk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).map_err(|e| e.to_string())?;
    hk.expand(b"Content-Encoding: nonce\0", &mut nonce).map_err(|e| e.to_string())?;

    // The 0x02 delimiter marks the last (and only) record; no padding.
    let mut record = plaintext.to_vec();
    record.push(2);
    let cipher = Aes128Gcm::new_from_slice(&cek).map_err(|e| e.to_string())?;
    let sealed = cipher
        .encrypt(Nonce::from_slice(&nonce), record.as_slice())
        .map_err(|e| e.to_string())?;

    let mut out = Vec::with_capacity(86 + sealed.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(as_public.as_bytes().len() as u8);
    out.extend_from_slice(as_public.as_bytes());
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Coucou's VAPID key pair. The private half lives in the Credential Manager.
pub struct Vapid {
    key: SigningKey,
}

impl Vapid {
    pub fn generate() -> Result<Self, String> {
        Ok(Self { key: SigningKey::from(random_secret()?) })
    }

    pub fn from_b64(private: &str) -> Option<Self> {
        let bytes = B64.decode(private).ok()?;
        SigningKey::from_slice(&bytes).ok().map(|key| Self { key })
    }

    pub fn private_b64(&self) -> String {
        B64.encode(self.key.to_bytes())
    }

    /// The uncompressed public point, as `applicationServerKey` wants it.
    pub fn public_b64(&self) -> String {
        B64.encode(self.key.verifying_key().to_encoded_point(false).as_bytes())
    }

    /// `Authorization: vapid t=<jwt>, k=<public key>` for this endpoint.
    pub fn authorization(&self, endpoint: &str, now_s: u64) -> Result<String, String> {
        let url = reqwest::Url::parse(endpoint).map_err(|e| e.to_string())?;
        let header = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = json!({
            "aud": url.origin().ascii_serialization(),
            "exp": now_s + JWT_LIFETIME_S,
            "sub": SUBJECT,
        });
        let input = format!("{header}.{}", B64.encode(claims.to_string()));
        let signature: Signature = self.key.sign(input.as_bytes());
        Ok(format!("vapid t={input}.{}, k={}", B64.encode(signature.to_bytes()), self.public_b64()))
    }
}

pub enum Sent {
    Delivered,
    /// 404 / 410: the subscription is dead and should be forgotten.
    Gone,
    Failed(String),
}

/// One push to one subscription. `urgency` is "high" for anything that waits on
/// the user, "normal" otherwise.
pub async fn send(
    client: &reqwest::Client,
    vapid: &Vapid,
    sub: &Subscription,
    payload: &[u8],
    urgency: &str,
    now_s: u64,
) -> Sent {
    let body = match encrypt(payload, sub) {
        Ok(b) => b,
        Err(e) => return Sent::Failed(e),
    };
    let auth = match vapid.authorization(&sub.endpoint, now_s) {
        Ok(a) => a,
        Err(e) => return Sent::Failed(e),
    };
    let response = client
        .post(&sub.endpoint)
        .header("Authorization", auth)
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        .header("TTL", "60")
        .header("Urgency", urgency)
        .body(body)
        .send()
        .await;
    match response {
        Ok(r) if r.status().is_success() => Sent::Delivered,
        Ok(r) if r.status().as_u16() == 404 || r.status().as_u16() == 410 => Sent::Gone,
        Ok(r) => Sent::Failed(format!("HTTP {}", r.status().as_u16())),
        Err(e) => Sent::Failed(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier;

    fn b64(s: &str) -> Vec<u8> {
        B64.decode(s.replace([' ', '\n'], "")).unwrap()
    }

    /// RFC 8291 Appendix A, end to end.
    #[test]
    fn rfc8291_vector() {
        let as_secret = SecretKey::from_slice(&b64("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw")).unwrap();
        let ua_public = b64("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4");
        let auth = b64("BTBZMqHH6r4Tts7J_aSIgg");
        let salt: [u8; 16] = b64("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let plaintext = b64("V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24");

        let out = encrypt_with(&plaintext, &ua_public, &auth, &as_secret, &salt).unwrap();

        let header = b64(
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml\
             mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8",
        );
        let ciphertext = b64("8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ");
        assert_eq!(out[..86], header[..]);
        assert_eq!(out[86..], ciphertext[..]);
    }

    #[test]
    fn vapid_token_verifies() {
        let vapid = Vapid::generate().unwrap();
        let again = Vapid::from_b64(&vapid.private_b64()).unwrap();
        assert_eq!(vapid.public_b64(), again.public_b64());

        let auth = vapid.authorization("https://web.push.apple.com/QGuQyavXut", 1_000).unwrap();
        let rest = auth.strip_prefix("vapid t=").unwrap();
        let (jwt, k) = rest.split_once(", k=").unwrap();
        assert_eq!(k, vapid.public_b64());
        let (input, sig) = jwt.rsplit_once('.').unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&b64(input.split('.').nth(1).unwrap())).unwrap();
        assert_eq!(claims["aud"], "https://web.push.apple.com");
        assert_eq!(claims["exp"], 1_000 + JWT_LIFETIME_S);
        let signature = Signature::from_slice(&b64(sig)).unwrap();
        vapid.key.verifying_key().verify(input.as_bytes(), &signature).unwrap();
    }
}
