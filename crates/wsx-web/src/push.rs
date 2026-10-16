//! Web Push: VAPID (RFC 8292) and aes128gcm message encryption (RFC 8291).
//! ^ Policy ported from the 0.30 preview (abe5197 wsx-web/src/push.rs): a browser can
//! only register an endpoint on an origin the site file lists, and only for the
//! topics that device consented to. The OpenSSL/libcurl transport was not ported;
//! see docs/web-client.md "Dependencies".
use crate::devices::{now_unix, random_bytes, write_private};
use crate::site::{parse_origin, PushSettings};
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes128Gcm, Nonce,
};
use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hkdf::Hkdf;
use p256::{
    ecdsa::{signature::Signer, Signature, SigningKey},
    elliptic_curve::sec1::ToEncodedPoint,
    PublicKey, SecretKey,
};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{fs, path::PathBuf, sync::Mutex, time::Duration};

const MAX_ENDPOINT_BYTES: usize = 2048;
// Push services reject bodies above 4096 bytes; keep one aes128gcm record.
const MAX_PAYLOAD_BYTES: usize = 3000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    Blocked,
    Done,
    Error,
    /// Task attention and digests (docs/web-client.md "Task notifications").
    Tasks,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionInput {
    pub endpoint: String,
    pub keys: SubscriptionKeys,
    /// Sent by browsers; the push service enforces expiry, so it is not stored.
    #[serde(default, rename = "expirationTime")]
    _expiration_time: Option<f64>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionKeys {
    pub p256dh: String,
    pub auth: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Subscription {
    device_id: String,
    endpoint: String,
    keys: SubscriptionKeys,
    topics: Vec<Topic>,
    created_unix: u64,
}

pub struct Push {
    file: PathBuf,
    subject: String,
    origins: Vec<String>,
    key: SigningKey,
    public_key: String,
    subscriptions: Mutex<Vec<Subscription>>,
    agent: ureq::Agent,
}

pub fn generate_vapid() -> Result<(String, String)> {
    let secret = random_secret()?;
    Ok((
        URL_SAFE_NO_PAD.encode(secret.to_bytes()),
        URL_SAFE_NO_PAD.encode(secret.public_key().to_encoded_point(false).as_bytes()),
    ))
}

fn random_secret() -> Result<SecretKey> {
    // A random 32-byte string is out of range with negligible probability; retry then.
    for _ in 0..8 {
        if let Ok(secret) = SecretKey::from_slice(&random_bytes::<32>()?) {
            return Ok(secret);
        }
    }
    bail!("cannot generate a P-256 key")
}

impl Push {
    pub fn open(state_dir: &std::path::Path, settings: &PushSettings) -> Result<Self> {
        let bytes = URL_SAFE_NO_PAD
            .decode(settings.private_key.trim())
            .context("push.private_key must be base64url")?;
        let key = SigningKey::from_slice(&bytes)
            .map_err(|_| anyhow!("push.private_key must be a 32-byte P-256 private key"))?;
        let public_key =
            URL_SAFE_NO_PAD.encode(key.verifying_key().to_encoded_point(false).as_bytes());
        let file = state_dir.join("push.json");
        let subscriptions = fs::read(&file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Ok(Self {
            file,
            subject: settings.subject.clone(),
            origins: settings.service_origins.clone(),
            key,
            public_key,
            subscriptions: Mutex::new(subscriptions),
            // Never follow a redirect: the endpoint origin was checked, a redirect target was not.
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(10)))
                .max_redirects(0)
                .http_status_as_error(false)
                .build()
                .into(),
        })
    }

    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    fn save(&self, subscriptions: &[Subscription]) {
        if let Ok(bytes) = serde_json::to_vec(subscriptions) {
            let _ = write_private(&self.file, &bytes);
        }
    }

    pub fn topics(&self, device_id: &str) -> Vec<Topic> {
        let subscriptions = self.subscriptions.lock().unwrap_or_else(|l| l.into_inner());
        subscriptions
            .iter()
            .find(|subscription| subscription.device_id == device_id)
            .map(|subscription| subscription.topics.clone())
            .unwrap_or_default()
    }

    /// Store this device's consent. An empty topic list removes the subscription.
    pub fn subscribe(
        &self,
        device_id: &str,
        input: SubscriptionInput,
        mut topics: Vec<Topic>,
    ) -> Result<()> {
        topics.dedup();
        ensure!(topics.len() <= 4, "too many topics");
        ensure!(
            input.endpoint.len() <= MAX_ENDPOINT_BYTES
                && !input.endpoint.chars().any(|c| c.is_control() || c == ' '),
            "invalid push endpoint"
        );
        let origin = endpoint_origin(&input.endpoint)?;
        ensure!(
            self.origins.contains(&origin),
            "push endpoint origin is not allowed by this site"
        );
        receiver_keys(&input.keys)?;
        let mut subscriptions = self.subscriptions.lock().unwrap_or_else(|l| l.into_inner());
        subscriptions.retain(|subscription| subscription.device_id != device_id);
        if !topics.is_empty() {
            subscriptions.push(Subscription {
                device_id: device_id.to_owned(),
                endpoint: input.endpoint,
                keys: input.keys,
                topics,
                created_unix: now_unix(),
            });
        }
        self.save(&subscriptions);
        Ok(())
    }

    pub fn unsubscribe(&self, device_id: &str) {
        let mut subscriptions = self.subscriptions.lock().unwrap_or_else(|l| l.into_inner());
        let before = subscriptions.len();
        subscriptions.retain(|subscription| subscription.device_id != device_id);
        if subscriptions.len() != before {
            self.save(&subscriptions);
        }
    }

    /// Deliver to every still-paired device that consented to `topic`.
    /// Blocking network I/O: call from a blocking thread. Returns delivered count.
    /// Send to each paired device that consented to `topic`, except the devices in `except`.
    pub fn notify(
        &self,
        topic: Topic,
        payload: &serde_json::Value,
        paired: &[String],
        except: &[String],
    ) -> usize {
        let targets: Vec<Subscription> = {
            let mut subscriptions = self.subscriptions.lock().unwrap_or_else(|l| l.into_inner());
            // A revoked device loses its subscription with its credential.
            let before = subscriptions.len();
            subscriptions.retain(|subscription| paired.contains(&subscription.device_id));
            if subscriptions.len() != before {
                self.save(&subscriptions);
            }
            subscriptions
                .iter()
                .filter(|subscription| {
                    subscription.topics.contains(&topic)
                        && !except.contains(&subscription.device_id)
                })
                .cloned()
                .collect()
        };
        let body = payload.to_string();
        let mut delivered = 0;
        for target in targets {
            match self.send(&target, body.as_bytes()) {
                Ok(status) if (200..300).contains(&status) => delivered += 1,
                // The push service says this subscription is gone for good.
                Ok(404 | 410) => self.unsubscribe(&target.device_id),
                Ok(status) => eprintln!("wsx web: push service answered {status}"),
                Err(error) => eprintln!("wsx web: push delivery failed: {error:#}"),
            }
        }
        delivered
    }

    fn send(&self, target: &Subscription, payload: &[u8]) -> Result<u16> {
        ensure!(payload.len() <= MAX_PAYLOAD_BYTES, "push payload too large");
        let body = encrypt(&target.keys, payload)?;
        let audience = endpoint_origin(&target.endpoint)?;
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "aud": audience, "exp": now_unix() + 12 * 3600, "sub": self.subject,
            })
            .to_string(),
        );
        let signature: Signature = self.key.sign(format!("{header}.{claims}").as_bytes());
        let token = format!(
            "{header}.{claims}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        );
        let response = self
            .agent
            .post(&target.endpoint)
            .header("TTL", "3600")
            .header("Urgency", "high")
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header(
                "Authorization",
                format!("vapid t={token}, k={}", self.public_key),
            )
            .send(&body[..])?;
        Ok(response.status().as_u16())
    }
}

fn endpoint_origin(endpoint: &str) -> Result<String> {
    let (scheme, rest) = endpoint
        .split_once("://")
        .context("push endpoint needs a scheme")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    Ok(parse_origin(&format!("{scheme}://{authority}"))?.origin)
}

fn receiver_keys(keys: &SubscriptionKeys) -> Result<(PublicKey, Vec<u8>, Vec<u8>)> {
    let public_bytes = URL_SAFE_NO_PAD
        .decode(keys.p256dh.trim_end_matches('='))
        .context("p256dh")?;
    let auth = URL_SAFE_NO_PAD
        .decode(keys.auth.trim_end_matches('='))
        .context("auth")?;
    ensure!(
        public_bytes.len() == 65 && auth.len() == 16,
        "push keys have the wrong length"
    );
    let public =
        PublicKey::from_sec1_bytes(&public_bytes).map_err(|_| anyhow!("p256dh is not a point"))?;
    Ok((public, public_bytes, auth))
}

/// RFC 8291 section 3.4: one aes128gcm record for a single receiver.
fn encrypt(keys: &SubscriptionKeys, payload: &[u8]) -> Result<Vec<u8>> {
    let (receiver, receiver_bytes, auth) = receiver_keys(keys)?;
    let sender = random_secret()?;
    let sender_public = sender.public_key().to_encoded_point(false);
    let shared = p256::ecdh::diffie_hellman(sender.to_nonzero_scalar(), receiver.as_affine());
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(&receiver_bytes);
    key_info.extend_from_slice(sender_public.as_bytes());
    let mut input_key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&auth), shared.raw_secret_bytes())
        .expand(&key_info, &mut input_key)
        .map_err(|_| anyhow!("hkdf"))?;
    let salt = random_bytes::<16>()?;
    let derive = Hkdf::<Sha256>::new(Some(&salt), &input_key);
    let (mut content_key, mut nonce) = ([0u8; 16], [0u8; 12]);
    derive
        .expand(b"Content-Encoding: aes128gcm\0", &mut content_key)
        .and_then(|_| derive.expand(b"Content-Encoding: nonce\0", &mut nonce))
        .map_err(|_| anyhow!("hkdf"))?;
    // 0x02 marks the last record; no extra padding.
    let mut plaintext = payload.to_vec();
    plaintext.push(2);
    let ciphertext = Aes128Gcm::new_from_slice(&content_key)
        .map_err(|_| anyhow!("aes key"))?
        .encrypt(Nonce::from_slice(&nonce), plaintext.as_slice())
        .map_err(|_| anyhow!("aes-gcm"))?;
    let mut body = salt.to_vec();
    body.extend_from_slice(&4096u32.to_be_bytes());
    body.push(sender_public.as_bytes().len() as u8);
    body.extend_from_slice(sender_public.as_bytes());
    body.extend_from_slice(&ciphertext);
    Ok(body)
}
