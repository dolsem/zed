use std::time::Duration;

use aes_gcm::aead::Aead as _;
use aes_gcm::{Aes128Gcm, KeyInit as _, Nonce};
use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hkdf::Hkdf;
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToEncodedPoint as _;
use rand::RngCore as _;
use serde::Serialize;
use sha2::Sha256;
use url::Url;

use crate::store::{PushSubscription, generate_secret_key, unix_timestamp};

const RECORD_SIZE: u32 = 4096;
const TAG_LENGTH: usize = 16;
const AUTH_SECRET_LENGTH: usize = 16;
const UNCOMPRESSED_POINT_LENGTH: usize = 65;
/// Push services reject bodies over 4096 bytes. The aes128gcm header takes 86
/// bytes and the tag plus padding delimiter another 17, leaving some margin.
pub const MAX_PLAINTEXT_LENGTH: usize = 3800;
const JWT_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);
const JWT_SUBJECT: &str = "https://zed.dev";
const MESSAGE_TTL_SECONDS: &str = "86400";

pub enum DeliveryOutcome {
    Delivered,
    /// The push service reports that the subscription no longer exists.
    Gone,
}

pub fn public_key_base64url(secret_key: &p256::SecretKey) -> String {
    URL_SAFE_NO_PAD.encode(uncompressed_public_key(secret_key))
}

fn uncompressed_public_key(secret_key: &p256::SecretKey) -> Vec<u8> {
    secret_key
        .public_key()
        .to_encoded_point(false)
        .as_bytes()
        .to_vec()
}

/// Browsers encode subscription keys as unpadded base64url, but tolerate
/// padding in case a client adds it.
pub fn decode_base64url(value: &str) -> Result<Vec<u8>> {
    Ok(URL_SAFE_NO_PAD.decode(value.trim_end_matches('=').as_bytes())?)
}

pub fn validate_subscription(subscription: &PushSubscription) -> Result<()> {
    if subscription.endpoint.len() > 2048 {
        bail!("endpoint is too long");
    }
    let endpoint = Url::parse(&subscription.endpoint).context("endpoint is not a valid URL")?;
    if endpoint.scheme() != "https" {
        bail!("endpoint must use https");
    }
    // Only paired devices can register endpoints, but still refuse anything
    // that would make the server send requests to local or literal addresses.
    match endpoint.host() {
        Some(url::Host::Domain(domain))
            if domain.contains('.') && !domain.eq_ignore_ascii_case("localhost") => {}
        _ => bail!("endpoint must have a public domain name"),
    }
    let user_agent_public_key =
        decode_base64url(&subscription.keys.p256dh).context("p256dh is not valid base64url")?;
    if user_agent_public_key.len() != UNCOMPRESSED_POINT_LENGTH {
        bail!("p256dh must be an uncompressed P-256 point");
    }
    p256::PublicKey::from_sec1_bytes(&user_agent_public_key)
        .map_err(|_| anyhow!("p256dh is not a valid P-256 point"))?;
    let auth_secret =
        decode_base64url(&subscription.keys.auth).context("auth is not valid base64url")?;
    if auth_secret.len() != AUTH_SECRET_LENGTH {
        bail!("auth must be 16 bytes");
    }
    Ok(())
}

pub async fn send(
    http_client: &reqwest::Client,
    vapid_key: &p256::SecretKey,
    subscription: &PushSubscription,
    plaintext: &[u8],
) -> Result<DeliveryOutcome> {
    let endpoint = Url::parse(&subscription.endpoint).context("invalid endpoint")?;
    let user_agent_public_key = decode_base64url(&subscription.keys.p256dh)?;
    let auth_secret = decode_base64url(&subscription.keys.auth)?;

    let body = encrypt(plaintext, &user_agent_public_key, &auth_secret)?;
    let audience = endpoint.origin().ascii_serialization();
    let expires_at = unix_timestamp() + JWT_LIFETIME.as_secs();
    let jwt = vapid_jwt(vapid_key, &audience, expires_at)?;
    let authorization = format!("vapid t={jwt}, k={}", public_key_base64url(vapid_key));

    let response = http_client
        .post(endpoint)
        .header("Authorization", authorization)
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        .header("TTL", MESSAGE_TTL_SECONDS)
        .header("Urgency", "high")
        .body(body)
        .send()
        .await
        .context("push request failed")?;

    let status = response.status();
    if status.is_success() {
        return Ok(DeliveryOutcome::Delivered);
    }
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
        return Ok(DeliveryOutcome::Gone);
    }
    let response_body: String = match response.text().await {
        Ok(text) => text.chars().take(500).collect(),
        Err(error) => format!("(failed to read response body: {error})"),
    };
    bail!("push service responded with {status}: {response_body}")
}

/// Encrypts `plaintext` as a single-record `aes128gcm` message (RFC 8291).
pub fn encrypt(
    plaintext: &[u8],
    user_agent_public_key: &[u8],
    auth_secret: &[u8],
) -> Result<Vec<u8>> {
    let mut salt = [0u8; 16];
    rand::rng().fill_bytes(&mut salt);
    encrypt_with(
        plaintext,
        user_agent_public_key,
        auth_secret,
        &generate_secret_key(),
        &salt,
    )
}

fn encrypt_with(
    plaintext: &[u8],
    user_agent_public_key: &[u8],
    auth_secret: &[u8],
    application_server_key: &p256::SecretKey,
    salt: &[u8; 16],
) -> Result<Vec<u8>> {
    if plaintext.len() + 1 + TAG_LENGTH > RECORD_SIZE as usize {
        bail!("push payload is too large");
    }
    let user_agent_point = p256::PublicKey::from_sec1_bytes(user_agent_public_key)
        .map_err(|_| anyhow!("invalid user agent public key"))?;
    let application_server_public_key = uncompressed_public_key(application_server_key);
    let shared_secret = p256::ecdh::diffie_hellman(
        application_server_key.to_nonzero_scalar(),
        user_agent_point.as_affine(),
    );

    let mut key_info = Vec::with_capacity(14 + 2 * UNCOMPRESSED_POINT_LENGTH);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(user_agent_public_key);
    key_info.extend_from_slice(&application_server_public_key);
    let mut input_keying_material = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth_secret), shared_secret.raw_secret_bytes())
        .expand(&key_info, &mut input_keying_material)
        .map_err(|_| anyhow!("HKDF expansion failed"))?;

    let content_hkdf = Hkdf::<Sha256>::new(Some(salt), &input_keying_material);
    let mut content_encryption_key = [0u8; 16];
    content_hkdf
        .expand(
            b"Content-Encoding: aes128gcm\0",
            &mut content_encryption_key,
        )
        .map_err(|_| anyhow!("HKDF expansion failed"))?;
    let mut nonce = [0u8; 12];
    content_hkdf
        .expand(b"Content-Encoding: nonce\0", &mut nonce)
        .map_err(|_| anyhow!("HKDF expansion failed"))?;

    let mut padded_plaintext = Vec::with_capacity(plaintext.len() + 1);
    padded_plaintext.extend_from_slice(plaintext);
    // Delimiter marking the final record, with no further padding.
    padded_plaintext.push(0x02);
    let cipher = Aes128Gcm::new_from_slice(&content_encryption_key)
        .map_err(|_| anyhow!("invalid content encryption key"))?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), padded_plaintext.as_slice())
        .map_err(|_| anyhow!("AES-GCM encryption failed"))?;

    let mut body = Vec::with_capacity(16 + 4 + 1 + UNCOMPRESSED_POINT_LENGTH + ciphertext.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(UNCOMPRESSED_POINT_LENGTH as u8);
    body.extend_from_slice(&application_server_public_key);
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

/// Builds an RFC 8292 VAPID token. ES256 JWTs carry the raw `r || s`
/// signature rather than DER, which Apple's push service insists on.
fn vapid_jwt(vapid_key: &p256::SecretKey, audience: &str, expires_at: u64) -> Result<String> {
    #[derive(Serialize)]
    struct Claims<'a> {
        aud: &'a str,
        exp: u64,
        sub: &'a str,
    }

    let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&Claims {
        aud: audience,
        exp: expires_at,
        sub: JWT_SUBJECT,
    })?);
    let signing_input = format!("{header}.{claims}");
    let signing_key = SigningKey::from(vapid_key);
    let signature: Signature = signing_key.sign(signing_input.as_bytes());
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::VerifyingKey;
    use p256::ecdsa::signature::Verifier as _;

    fn decode(value: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(value).unwrap()
    }

    #[test]
    fn test_rfc8291_example() {
        let application_server_key =
            p256::SecretKey::from_slice(&decode("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw"))
                .unwrap();
        assert_eq!(
            public_key_base64url(&application_server_key),
            "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"
        );
        let user_agent_public_key = decode(
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
        );
        let auth_secret = decode("BTBZMqHH6r4Tts7J_aSIgg");
        let salt: [u8; 16] = decode("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();

        let body = encrypt_with(
            b"When I grow up, I want to be a watermelon",
            &user_agent_public_key,
            &auth_secret,
            &application_server_key,
            &salt,
        )
        .unwrap();

        assert_eq!(
            URL_SAFE_NO_PAD.encode(body),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );
    }

    #[test]
    fn test_encrypt_uses_fresh_keys() {
        let user_agent_key = generate_secret_key();
        let user_agent_public_key = uncompressed_public_key(&user_agent_key);
        let first = encrypt(b"hello", &user_agent_public_key, &[7; 16]).unwrap();
        let second = encrypt(b"hello", &user_agent_public_key, &[7; 16]).unwrap();
        assert_eq!(first.len(), 16 + 4 + 1 + 65 + 5 + 1 + 16);
        assert_ne!(first, second);
        assert!(encrypt(&[0; 4096], &user_agent_public_key, &[7; 16]).is_err());
    }

    #[test]
    fn test_vapid_jwt_verifies() {
        let vapid_key = generate_secret_key();
        let jwt = vapid_jwt(&vapid_key, "https://web.push.apple.com", 1_700_000_000).unwrap();
        let parts = jwt.split('.').collect::<Vec<_>>();
        assert_eq!(parts.len(), 3);

        let header: serde_json::Value = serde_json::from_slice(&decode(parts[0])).unwrap();
        assert_eq!(header["alg"], "ES256");
        let claims: serde_json::Value = serde_json::from_slice(&decode(parts[1])).unwrap();
        assert_eq!(claims["aud"], "https://web.push.apple.com");
        assert_eq!(claims["exp"], 1_700_000_000);
        assert_eq!(claims["sub"], JWT_SUBJECT);

        let signature_bytes = decode(parts[2]);
        assert_eq!(signature_bytes.len(), 64);
        let signature = Signature::from_slice(&signature_bytes).unwrap();
        let verifying_key =
            VerifyingKey::from_sec1_bytes(&decode(&public_key_base64url(&vapid_key))).unwrap();
        verifying_key
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .unwrap();
        assert!(
            verifying_key
                .verify(format!("{}.{}x", parts[0], parts[1]).as_bytes(), &signature)
                .is_err()
        );
    }

    #[test]
    fn test_validate_subscription() {
        let user_agent_public_key = public_key_base64url(&generate_secret_key());
        let subscription = |endpoint: &str, p256dh: &str, auth: &str| PushSubscription {
            endpoint: endpoint.into(),
            keys: crate::store::PushSubscriptionKeys {
                p256dh: p256dh.into(),
                auth: auth.into(),
            },
        };
        let auth = URL_SAFE_NO_PAD.encode([1u8; 16]);
        assert!(
            validate_subscription(&subscription(
                "https://web.push.apple.com/abc",
                &user_agent_public_key,
                &auth
            ))
            .is_ok()
        );
        for endpoint in [
            "http://web.push.apple.com/abc",
            "https://127.0.0.1/abc",
            "https://localhost/abc",
            "https://[::1]/abc",
            "not a url",
        ] {
            assert!(
                validate_subscription(&subscription(endpoint, &user_agent_public_key, &auth))
                    .is_err(),
                "{endpoint} should be rejected"
            );
        }
        assert!(
            validate_subscription(&subscription(
                "https://web.push.apple.com/abc",
                &user_agent_public_key,
                "AAAA"
            ))
            .is_err()
        );
    }
}
