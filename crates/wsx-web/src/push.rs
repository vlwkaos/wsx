//! Web Push: VAPID (RFC 8292) and aes128gcm message encryption (RFC 8291).
//! ^ Policy ported from the 0.30 preview (abe5197 wsx-web/src/push.rs): a browser can
//! only register an endpoint on an origin the site file lists, and only for the
//! switches that device turned on. The OpenSSL/libcurl transport was not ported;
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

/// The topics of earlier versions. Read once from `push.json` and mapped to the switches; never written.
#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OldTopic {
    Blocked,
    Done,
    Error,
    Tasks,
}

/// The two switches that push at once; the digest has its own time (docs/web-client.md
/// "Notifications").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Switch {
    Manager,
    Agents,
}

/// When a device gets its daily digest: a minute of its day, in the offset it had then.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestTime {
    pub minute: u16,
    pub utc_offset_minutes: i16,
}

impl DigestTime {
    /// The device's day number at `unix`, and whether its digest minute has come that day.
    pub fn day(&self, unix: u64) -> (i64, bool) {
        let local = unix as i64 + i64::from(self.utc_offset_minutes) * 60;
        (
            local.div_euclid(86_400),
            local.rem_euclid(86_400) / 60 >= i64::from(self.minute),
        )
    }
}

/// What a device chose: the API's body and part of its stored subscription.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choices {
    #[serde(default)]
    pub manager: bool,
    #[serde(default)]
    pub agents: bool,
    #[serde(default)]
    pub digest: Option<DigestTime>,
}

impl Choices {
    pub fn any(&self) -> bool {
        self.manager || self.agents || self.digest.is_some()
    }
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
pub struct Subscription {
    pub device_id: String,
    endpoint: String,
    keys: SubscriptionKeys,
    #[serde(default)]
    pub choices: Choices,
    /// Sessions this device hears nothing about.
    #[serde(default)]
    pub muted: Vec<u64>,
    /// The device's day of the last digest and when it went: the next lists what came after.
    #[serde(default)]
    pub digest_sent: Option<(i64, u64)>,
    #[serde(default, skip_serializing)]
    topics: Vec<OldTopic>,
    created_unix: u64,
}

impl Subscription {
    pub fn wants(&self, switch: Switch) -> bool {
        match switch {
            Switch::Manager => self.choices.manager,
            Switch::Agents => self.choices.agents,
        }
    }

    /// Earlier topics as switches: blocked and error are "An agent needs me", tasks is
    /// "Manager needs me", and done and tasks' digest are the daily digest, at 09:00 here.
    fn migrate(&mut self) {
        let topics = std::mem::take(&mut self.topics);
        let choices = &mut self.choices;
        choices.agents |= topics.contains(&OldTopic::Blocked) || topics.contains(&OldTopic::Error);
        choices.manager |= topics.contains(&OldTopic::Tasks);
        if choices.digest.is_none()
            && (topics.contains(&OldTopic::Done) || topics.contains(&OldTopic::Tasks))
        {
            choices.digest = Some(DigestTime {
                minute: 9 * 60,
                utc_offset_minutes: local_offset_minutes(),
            });
        }
    }
}

/// This host's UTC offset now, for a digest time migrated without the device's own.
fn local_offset_minutes() -> i16 {
    let now = now_unix() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call; localtime_r is thread-safe.
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return 0;
    }
    (tm.tm_gmtoff / 60) as i16
}

/// The first digest after the digest is switched on: today when its time is still to come.
fn first_digest(time: DigestTime, now: u64) -> (i64, u64) {
    let (day, passed) = time.day(now);
    (if passed { day } else { day - 1 }, now)
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
        let mut subscriptions: Vec<Subscription> = fs::read(&file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let old = subscriptions
            .iter()
            .any(|subscription| !subscription.topics.is_empty());
        for subscription in &mut subscriptions {
            subscription.migrate();
            if subscription.digest_sent.is_none() {
                subscription.digest_sent = subscription
                    .choices
                    .digest
                    .map(|time| first_digest(time, now_unix()));
            }
        }
        // ^ A device whose topics map to no switch has nothing left to receive.
        subscriptions.retain(|subscription| subscription.choices.any());
        let push = Self {
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
        };
        if old {
            push.save(&crate::locked(&push.subscriptions));
        }
        Ok(push)
    }

    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    fn save(&self, subscriptions: &[Subscription]) {
        if let Ok(bytes) = serde_json::to_vec(subscriptions) {
            let _ = write_private(&self.file, &bytes);
        }
    }

    /// This device's subscription, if it has one.
    pub fn device(&self, device_id: &str) -> Option<Subscription> {
        crate::locked(&self.subscriptions)
            .iter()
            .find(|subscription| subscription.device_id == device_id)
            .cloned()
    }

    /// Store this device's choices. With every switch off the subscription is removed.
    /// Its mutes and the last digest are kept while it stays.
    pub fn subscribe(
        &self,
        device_id: &str,
        input: SubscriptionInput,
        choices: Choices,
    ) -> Result<()> {
        if let Some(time) = choices.digest {
            ensure!(
                time.minute < 24 * 60 && (-14 * 60..=14 * 60).contains(&time.utc_offset_minutes),
                "digest time is out of range"
            );
        }
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
        let mut subscriptions = crate::locked(&self.subscriptions);
        let before = subscriptions
            .iter()
            .position(|subscription| subscription.device_id == device_id)
            .map(|index| subscriptions.remove(index));
        if choices.any() {
            let now = now_unix();
            // ^ The same time keeps the next digest; a new one keeps what it will list.
            let digest_sent = choices.digest.map(|time| {
                let kept = before.as_ref().and_then(|before| before.digest_sent);
                match (
                    kept,
                    before.as_ref().and_then(|before| before.choices.digest),
                ) {
                    (Some(kept), Some(was)) if was == time => kept,
                    (Some((_, since)), _) => (first_digest(time, now).0, since),
                    (None, _) => first_digest(time, now),
                }
            });
            subscriptions.push(Subscription {
                device_id: device_id.to_owned(),
                endpoint: input.endpoint,
                keys: input.keys,
                choices,
                muted: before
                    .as_ref()
                    .map(|before| before.muted.clone())
                    .unwrap_or_default(),
                digest_sent,
                topics: Vec::new(),
                created_unix: before.map_or(now, |before| before.created_unix),
            });
        }
        self.save(&subscriptions);
        Ok(())
    }

    pub fn unsubscribe(&self, device_id: &str) {
        let mut subscriptions = crate::locked(&self.subscriptions);
        let before = subscriptions.len();
        subscriptions.retain(|subscription| subscription.device_id != device_id);
        if subscriptions.len() != before {
            self.save(&subscriptions);
        }
    }

    /// Mute or unmute one session for this device. Answers its mutes, `None` without a
    /// subscription.
    pub fn mute(&self, device_id: &str, session: u64, muted: bool) -> Option<Vec<u64>> {
        let mut subscriptions = crate::locked(&self.subscriptions);
        let subscription = subscriptions
            .iter_mut()
            .find(|subscription| subscription.device_id == device_id)?;
        subscription.muted.retain(|other| *other != session);
        if muted {
            subscription.muted.push(session);
        }
        let list = subscription.muted.clone();
        self.save(&subscriptions);
        Some(list)
    }

    /// Forget the mutes of sessions that no longer exist.
    pub fn retain_muted(&self, alive: &[u64]) {
        let mut subscriptions = crate::locked(&self.subscriptions);
        let mut changed = false;
        for subscription in subscriptions.iter_mut() {
            let before = subscription.muted.len();
            subscription.muted.retain(|session| alive.contains(session));
            changed |= subscription.muted.len() != before;
        }
        if changed {
            self.save(&subscriptions);
        }
    }

    /// Record that a device's digest for `day` went (or had nothing to say) at `unix`.
    pub fn digest_sent(&self, device_id: &str, day: i64, unix: u64) {
        let mut subscriptions = crate::locked(&self.subscriptions);
        if let Some(subscription) = subscriptions
            .iter_mut()
            .find(|subscription| subscription.device_id == device_id)
        {
            subscription.digest_sent = Some((day, unix));
            self.save(&subscriptions);
        }
    }

    /// The subscriptions of still-paired devices; a revoked device loses its subscription
    /// with its credential.
    pub fn devices(&self, paired: &[String]) -> Vec<Subscription> {
        let mut subscriptions = crate::locked(&self.subscriptions);
        let before = subscriptions.len();
        subscriptions.retain(|subscription| paired.contains(&subscription.device_id));
        if subscriptions.len() != before {
            self.save(&subscriptions);
        }
        subscriptions.clone()
    }

    /// Send one payload to one device. Blocking network I/O: call from a blocking thread.
    pub fn notify(&self, device_id: &str, payload: &serde_json::Value) {
        let Some(target) = self.device(device_id) else {
            return;
        };
        match self.send(&target, payload.to_string().as_bytes()) {
            Ok(status) if (200..300).contains(&status) => {}
            // The push service says this subscription is gone for good.
            Ok(404 | 410) => self.unsubscribe(&target.device_id),
            Ok(status) => eprintln!("wsx web: push service answered {status}"),
            Err(error) => eprintln!("wsx web: push delivery failed: {error:#}"),
        }
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
