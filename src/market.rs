// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! Staged, non-purchasing Market channel adapter for a verified Telegram sender.
//! Core/Market own purchases: this adapter never debits seeds, activates an
//! entitlement, or creates an issuer order without a reviewed Core permit.

// The standalone adapter is intentionally not wired into the bot until the
// Core active-session and Market channel-order contracts are implemented.
#![allow(dead_code)]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Client, redirect::Policy};
use ring::hmac;
use serde_json::{Value, json};
use url::Url;

const ISSUER: &str = "0x1";
const DOMAIN: &str = "zero-x-da-market-customer-v1.";
const ASSERTION_SECONDS: u64 = 60;
const MAX_RESPONSE_BYTES: usize = 8192;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MarketError {
    InvalidConfiguration,
    UnverifiedTelegramSender,
    Unavailable,
    InvalidResponse,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VerifiedTelegramActor {
    subject_id: u64,
}

impl VerifiedTelegramActor {
    /// The caller must receive this evidence from the authenticated Telegram
    /// Bot API, not a form, forwarded message or user-entered identifier.
    pub(crate) fn from_original_private_update(
        subject_id: u64,
        private_chat: bool,
        human_sender: bool,
        forwarded: bool,
    ) -> Result<Self, MarketError> {
        if subject_id == 0 || !private_chat || !human_sender || forwarded {
            return Err(MarketError::UnverifiedTelegramSender);
        }
        Ok(Self { subject_id })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AccountLink {
    Linked { pub_dress: String },
    SignInRequired,
}

#[derive(Clone)]
pub(crate) struct MarketGateway {
    http: Client,
    origin: Url,
    public_token: String,
    assertion_secret: String,
}

impl MarketGateway {
    /// All three variables are required together. A missing/partial config
    /// cannot silently enable a weaker checkout or infer an AIS identity.
    pub(crate) fn from_env() -> Result<Option<Self>, MarketError> {
        let origin = std::env::var("MARKET_CHANNEL_ORIGIN").ok();
        let token = std::env::var("MARKET_CHANNEL_PUBLIC_TOKEN").ok();
        let secret = std::env::var("MARKET_CHANNEL_ASSERTION_SECRET").ok();
        match (origin, token, secret) {
            (None, None, None) => Ok(None),
            (Some(origin), Some(token), Some(secret)) => {
                Self::new(&origin, token, secret).map(Some)
            }
            _ => Err(MarketError::InvalidConfiguration),
        }
    }

    fn new(origin: &str, public_token: String, assertion_secret: String) -> Result<Self, MarketError> {
        let origin = Url::parse(origin).map_err(|_| MarketError::InvalidConfiguration)?;
        if origin.scheme() != "https"
            || origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.query().is_some()
            || origin.fragment().is_some()
            || origin.path() != "/"
            || public_token.trim().is_empty()
            || assertion_secret.as_bytes().len() < 32
        {
            return Err(MarketError::InvalidConfiguration);
        }
        let http = Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(6))
            .build()
            .map_err(|_| MarketError::InvalidConfiguration)?;
        Ok(Self { http, origin, public_token, assertion_secret })
    }

    fn endpoint(&self, path: &str) -> Result<Url, MarketError> {
        self.origin.join(path).map_err(|_| MarketError::InvalidConfiguration)
    }

    /// Resolve one Telegram provider identity to Market's verified customer.
    /// This requires trusted Bot API sender evidence; no user_id comes from a
    /// callback body. Market may create its own customer record here.
    async fn customer_id(&self, actor: VerifiedTelegramActor) -> Result<String, MarketError> {
        let url = self.endpoint("/v1/auth/external")?;
        let response = self.http.post(url)
            .bearer_auth(&self.public_token)
            .json(&json!({"provider": "telegram", "provider_user_id": actor.subject_id.to_string()}))
            .send().await.map_err(|_| MarketError::Unavailable)?;
        let body = Self::parse_response(response).await?;
        body.pointer("/data/id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 128)
            .map(str::to_owned)
            .ok_or(MarketError::InvalidResponse)
    }

    fn customer_assertion(
        &self,
        user_id: &str,
        actor: VerifiedTelegramActor,
        now: u64,
    ) -> Result<String, MarketError> {
        if user_id.is_empty() || user_id.len() > 128 {
            return Err(MarketError::InvalidResponse);
        }
        let payload = serde_json::to_vec(&json!({
            "user_id": user_id,
            "provider": "telegram",
            "provider_user_id": actor.subject_id.to_string(),
            "iat": now,
            "exp": now.saturating_add(ASSERTION_SECONDS),
        })).map_err(|_| MarketError::InvalidResponse)?;
        let encoded = URL_SAFE_NO_PAD.encode(payload);
        let message = format!("{DOMAIN}{encoded}");
        let key = hmac::Key::new(hmac::HMAC_SHA256, self.assertion_secret.as_bytes());
        let mac = hmac::sign(&key, message.as_bytes());
        let hex: String = mac.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        Ok(format!("v1.{encoded}.{hex}"))
    }

    async fn parse_response(response: reqwest::Response) -> Result<Value, MarketError> {
        if !response.status().is_success() {
            return Err(MarketError::Unavailable);
        }
        let body = response.bytes().await.map_err(|_| MarketError::Unavailable)?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(MarketError::InvalidResponse);
        }
        serde_json::from_slice(&body).map_err(|_| MarketError::InvalidResponse)
    }

    /// The Market directory, not the bot, knows whether this Telegram account
    /// is attached to a Bond. No subscription status or ability to buy is
    /// inferred from an unlinked or unavailable directory response.
    pub(crate) async fn account_link(
        &self,
        actor: VerifiedTelegramActor,
    ) -> Result<AccountLink, MarketError> {
        let market_user = self.customer_id(actor).await?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| MarketError::Unavailable)?
            .as_secs();
        let assertion = self.customer_assertion(&market_user, actor, now)?;
        let url = self.endpoint(&format!("/v1/issuer-accounts/{ISSUER}"))?;
        let response = self.http.get(url)
            .bearer_auth(&self.public_token)
            .header("X-Market-Customer-Assertion", assertion)
            .send().await.map_err(|_| MarketError::Unavailable)?;
        let body = Self::parse_response(response).await?;
        if body.pointer("/data/type").and_then(Value::as_str) != Some("issuer_account")
            || body.pointer("/data/id").and_then(Value::as_str) != Some(ISSUER)
        {
            return Err(MarketError::InvalidResponse);
        }
        match body.pointer("/data/attributes/linked").and_then(Value::as_bool) {
            Some(false) => Ok(AccountLink::SignInRequired),
            Some(true) => body.pointer("/data/attributes/pub_dress")
                .and_then(Value::as_str)
                .filter(|id| id.starts_with("0x") && id.len() <= 128)
                .map(|pub_dress| AccountLink::Linked { pub_dress: pub_dress.to_owned() })
                .ok_or(MarketError::InvalidResponse),
            None => Err(MarketError::InvalidResponse),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn gateway() -> MarketGateway {
        MarketGateway::new(
            "https://0xda-market.nilx.one/",
            "test-public-token".to_owned(),
            "test-assertion-secret-that-is-longer-than-32-bytes".to_owned(),
        ).unwrap()
    }

    #[test]
    fn requires_real_private_human_source() {
        assert!(VerifiedTelegramActor::from_original_private_update(42, true, true, false).is_ok());
        for (id, private, human, forwarded) in [
            (0, true, true, false),
            (42, false, true, false),
            (42, true, false, false),
            (42, true, true, true),
        ] {
            assert_eq!(
                VerifiedTelegramActor::from_original_private_update(id, private, human, forwarded),
                Err(MarketError::UnverifiedTelegramSender)
            );
        }
    }

    #[test]
    fn assertion_matches_markets_documented_hmac_and_subject() {
        let gateway = gateway();
        let actor = VerifiedTelegramActor::from_original_private_update(42, true, true, false).unwrap();
        let assertion = gateway.customer_assertion("market-uuid", actor, 100).unwrap();
        let mut segments = assertion.split('.');
        assert_eq!(segments.next(), Some("v1"));
        let encoded = segments.next().unwrap();
        let received_mac = segments.next().unwrap();
        assert!(segments.next().is_none());
        let json: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).unwrap()).unwrap();
        assert_eq!(json["user_id"], "market-uuid");
        assert_eq!(json["provider"], "telegram");
        assert_eq!(json["provider_user_id"], "42");
        assert_eq!(json["iat"], 100);
        assert_eq!(json["exp"], 160);
        let key = hmac::Key::new(hmac::HMAC_SHA256, gateway.assertion_secret.as_bytes());
        let expected_message = format!("{DOMAIN}{encoded}");
        let bytes: Vec<u8> = (0..received_mac.len()).step_by(2)
            .map(|i| u8::from_str_radix(&received_mac[i..i + 2], 16).unwrap())
            .collect();
        hmac::verify(&key, expected_message.as_bytes(), &bytes).unwrap();
    }

    #[test]
    fn configuration_rejects_plaintext_and_weak_assertion_key() {
        assert!(MarketGateway::new(
            "http://0xda-market.nilx.one/",
            "token".into(), "a-strong-test-secret-that-is-over-32-characters".into()
        ).is_err());
        assert!(MarketGateway::new(
            "https://0xda-market.nilx.one/",
            "token".into(), "short".into()
        ).is_err());
    }
}
