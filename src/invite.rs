use anyhow::{Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Truncating HMAC-SHA256 to 128 bits keeps a link short enough to read out
/// loud while leaving forgery out of reach.
const TAG_BYTES: usize = 16;

#[derive(Debug)]
pub struct SigningKey(Vec<u8>);

impl SigningKey {
    pub fn from_secret(secret: &[u8]) -> Self {
        Self(secret.to_vec())
    }

    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|error| anyhow!("no system randomness: {error}"))?;
        Ok(Self(bytes.to_vec()))
    }

    /// `<seat>.<expiry>.<tag>` — the seat is readable so an operator can tell
    /// two links apart, and the tag is what makes a guessed seat useless.
    pub fn mint(&self, seat: &str, expires_at: i64) -> String {
        let claim = format!("{seat}.{expires_at}");
        let tag = B64.encode(self.tag(&claim));
        format!("{claim}.{tag}")
    }

    pub fn verify(&self, token: &str) -> Result<Invite> {
        let Some((claim, tag)) = token.rsplit_once('.') else {
            bail!("malformed invite");
        };
        let Some((seat, expires_at)) = claim.split_once('.') else {
            bail!("malformed invite");
        };
        let Ok(presented) = B64.decode(tag) else {
            bail!("malformed invite");
        };
        if presented.len() != TAG_BYTES || !constant_time_eq(&presented, &self.tag(claim)) {
            bail!("invite signature does not verify");
        }
        let Ok(expires_at) = expires_at.parse::<i64>() else {
            bail!("malformed invite");
        };
        Ok(Invite {
            seat: seat.to_string(),
            expires_at,
        })
    }

    fn tag(&self, claim: &str) -> Vec<u8> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0).expect("hmac accepts any key length");
        mac.update(claim.as_bytes());
        mac.finalize().into_bytes()[..TAG_BYTES].to_vec()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Invite {
    pub seat: String,
    pub expires_at: i64,
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A seat id travels into a weave node id, a weave stream name and — through
/// open-live's provider — a CouchDB document id, so it is held to the narrowest
/// of those alphabets.
pub fn seat_id(display_name: &str, suffix: &str) -> String {
    let mut slug = String::new();
    for ch in display_name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.extend(ch.to_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= 24 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        format!("guest-{suffix}")
    } else {
        format!("{slug}-{suffix}")
    }
}

pub fn random_suffix() -> Result<String> {
    let mut bytes = [0u8; 2];
    getrandom::fill(&mut bytes).map_err(|error| anyhow!("no system randomness: {error}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SigningKey {
        SigningKey::from_secret(b"test-key")
    }

    #[test]
    fn a_minted_token_verifies_to_its_claim() {
        let token = key().mint("alice-1f2e", 1_757_000_000);
        assert_eq!(
            key().verify(&token).unwrap(),
            Invite {
                seat: "alice-1f2e".into(),
                expires_at: 1_757_000_000
            }
        );
    }

    #[test]
    fn a_tampered_seat_or_expiry_does_not_verify() {
        let token = key().mint("alice-1f2e", 1_757_000_000);
        let (_, tag) = token.rsplit_once('.').unwrap();
        for forged in [
            format!("bob-1f2e.1757000000.{tag}"),
            format!("alice-1f2e.9757000000.{tag}"),
        ] {
            assert!(key().verify(&forged).is_err(), "{forged} verified");
        }
    }

    #[test]
    fn another_key_does_not_verify() {
        let token = key().mint("alice-1f2e", 1_757_000_000);
        assert!(
            SigningKey::from_secret(b"other-key")
                .verify(&token)
                .is_err()
        );
    }

    #[test]
    fn malformed_tokens_are_refused_rather_than_panicking() {
        for token in ["", ".", "alice", "alice.notanumber.AAAAAAAAAAAAAAAAAAAAAA", "a.1.!!"] {
            assert!(key().verify(token).is_err(), "{token:?} verified");
        }
    }

    #[test]
    fn seat_ids_keep_only_what_every_downstream_id_accepts() {
        assert_eq!(seat_id("Alice", "1f2e"), "alice-1f2e");
        assert_eq!(seat_id("Ada Lovelace", "1f2e"), "ada-lovelace-1f2e");
        assert_eq!(seat_id("  Zoë!! ", "1f2e"), "zo-1f2e");
        assert_eq!(seat_id("", "1f2e"), "guest-1f2e");
        assert_eq!(seat_id("!!!", "1f2e"), "guest-1f2e");
        assert!(seat_id(&"x".repeat(200), "1f2e").len() <= 30);
    }
}
