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
use serde::Deserialize;
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

    fn new(
        origin: &str,
        public_token: String,
        assertion_secret: String,
    ) -> Result<Self, MarketError> {
        let origin = Url::parse(origin).map_err(|_| MarketError::InvalidConfiguration)?;
        if origin.scheme() != "https"
            || origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.query().is_some()
            || origin.fragment().is_some()
            || origin.path() != "/"
            || public_token.trim().is_empty()
            || assertion_secret.len() < 32
        {
            return Err(MarketError::InvalidConfiguration);
        }
        let http = Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(6))
            .build()
            .map_err(|_| MarketError::InvalidConfiguration)?;
        Ok(Self {
            http,
            origin,
            public_token,
            assertion_secret,
        })
    }

    fn endpoint(&self, path: &str) -> Result<Url, MarketError> {
        self.origin
            .join(path)
            .map_err(|_| MarketError::InvalidConfiguration)
    }

    /// Resolve one Telegram provider identity to Market's verified customer.
    /// This requires trusted Bot API sender evidence; no user_id comes from a
    /// callback body. Market may create its own customer record here.
    async fn customer_id(&self, actor: VerifiedTelegramActor) -> Result<String, MarketError> {
        let url = self.endpoint("/v1/auth/external")?;
        let response = self
            .http
            .post(url)
            .bearer_auth(&self.public_token)
            .json(
                &json!({"provider": "telegram", "provider_user_id": actor.subject_id.to_string()}),
            )
            .send()
            .await
            .map_err(|_| MarketError::Unavailable)?;
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
        }))
        .map_err(|_| MarketError::InvalidResponse)?;
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
        let body = response
            .bytes()
            .await
            .map_err(|_| MarketError::Unavailable)?;
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
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.public_token)
            .header("X-Market-Customer-Assertion", assertion)
            .send()
            .await
            .map_err(|_| MarketError::Unavailable)?;
        let body = Self::parse_response(response).await?;
        Self::parse_account_link(&body)
    }

    // The issuer directory returns its own public handle. Telegram does not
    // impose the 0x1 pub_dress syntax on a Market-provided linked handle.
    fn parse_account_link(body: &Value) -> Result<AccountLink, MarketError> {
        if body.pointer("/data/type").and_then(Value::as_str) != Some("issuer_account")
            || body.pointer("/data/id").and_then(Value::as_str) != Some(ISSUER)
        {
            return Err(MarketError::InvalidResponse);
        }
        match body
            .pointer("/data/attributes/linked")
            .and_then(Value::as_bool)
        {
            Some(false) => Ok(AccountLink::SignInRequired),
            Some(true) => body
                .pointer("/data/attributes/pub_dress")
                .and_then(Value::as_str)
                .filter(|handle| !handle.is_empty() && handle.chars().count() <= 128)
                .map(|pub_dress| AccountLink::Linked {
                    pub_dress: pub_dress.to_owned(),
                })
                .ok_or(MarketError::InvalidResponse),
            None => Err(MarketError::InvalidResponse),
        }
    }
}

const POINTS_URL: &str = "https://0xda-market.nilx.one/nilx-browser/points";
const MAX_DIRECTORY_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
pub(crate) struct RegisteredMarketPoint {
    pub address: String,
    pub station_pub_dress: String,
    path: String,
}

#[derive(Deserialize)]
struct MarketPoints {
    points: Vec<RegisteredMarketPoint>,
}

pub(crate) async fn registered_market_points() -> Result<Vec<RegisteredMarketPoint>, MarketError> {
    fetch_registered_points(POINTS_URL).await
}

async fn fetch_registered_points(url: &str) -> Result<Vec<RegisteredMarketPoint>, MarketError> {
    let client = Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(6))
        .build()
        .map_err(|_| MarketError::InvalidConfiguration)?;
    // The panel only says "unavailable"; the log says why. Only a category or a
    // status code is recorded: no body, no header and no address.
    let mut response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| {
            let kind = if error.is_timeout() {
                "timeout"
            } else if error.is_connect() {
                "connect"
            } else {
                "request"
            };
            tracing::warn!(kind, "market registry request failed");
            MarketError::Unavailable
        })?;
    if !response.status().is_success() {
        tracing::warn!(
            status = response.status().as_u16(),
            "market registry answered with a failure status"
        );
        return Err(MarketError::Unavailable);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        tracing::warn!("market registry answer was cut off");
        MarketError::Unavailable
    })? {
        if chunk.len() > MAX_DIRECTORY_BYTES - bytes.len() {
            tracing::warn!("market registry answer exceeds the size limit");
            return Err(MarketError::InvalidResponse);
        }
        bytes.extend_from_slice(&chunk);
    }
    parse_registered_points(&bytes).inspect_err(|_| {
        tracing::warn!("market registry answer is not a valid point directory");
    })
}

fn parse_registered_points(bytes: &[u8]) -> Result<Vec<RegisteredMarketPoint>, MarketError> {
    let mut directory: MarketPoints =
        serde_json::from_slice(bytes).map_err(|_| MarketError::InvalidResponse)?;
    let segment = |value: &str, max: usize| {
        value.len() <= max
            && value.split('-').all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            })
    };
    let mut stations = std::collections::HashSet::new();
    let mut addresses = std::collections::HashSet::new();
    if directory.points.len() > 256
        || directory.points.iter().any(|point| {
            !segment(&point.address, 48)
                || !point
                    .station_pub_dress
                    .strip_prefix("0xda-market-")
                    .is_some_and(|place| segment(place, 64))
                || point.path != format!("/{}", point.address)
                || !addresses.insert(point.address.as_str())
                || !stations.insert(point.station_pub_dress.as_str())
        })
    {
        return Err(MarketError::InvalidResponse);
    }
    directory
        .points
        .sort_unstable_by(|a, b| a.address.cmp(&b.address));
    Ok(directory.points)
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
        )
        .unwrap()
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
        let actor =
            VerifiedTelegramActor::from_original_private_update(42, true, true, false).unwrap();
        let assertion = gateway
            .customer_assertion("market-uuid", actor, 100)
            .unwrap();
        let mut segments = assertion.split('.');
        assert_eq!(segments.next(), Some("v1"));
        let encoded = segments.next().unwrap();
        let received_mac = segments.next().unwrap();
        assert!(segments.next().is_none());
        let json: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).unwrap()).unwrap();
        assert_eq!(json["user_id"], "market-uuid");
        assert_eq!(json["provider"], "telegram");
        assert_eq!(json["provider_user_id"], "42");
        assert_eq!(json["iat"], 100);
        assert_eq!(json["exp"], 160);
        let key = hmac::Key::new(hmac::HMAC_SHA256, gateway.assertion_secret.as_bytes());
        let expected_message = format!("{DOMAIN}{encoded}");
        let bytes: Vec<u8> = (0..received_mac.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&received_mac[i..i + 2], 16).unwrap())
            .collect();
        hmac::verify(&key, expected_message.as_bytes(), &bytes).unwrap();
    }

    #[test]
    fn market_directory_accepts_authoritative_non_hex_handles() {
        let linked = json!({
            "data": {
                "type": "issuer_account",
                "id": "0x1",
                "attributes": {"linked": true, "pub_dress": "zero"}
            }
        });
        assert_eq!(
            MarketGateway::parse_account_link(&linked),
            Ok(AccountLink::Linked {
                pub_dress: "zero".to_owned()
            })
        );
        let mut hexadecimal = linked.clone();
        hexadecimal["data"]["attributes"]["pub_dress"] = json!("0x0sky");
        assert_eq!(
            MarketGateway::parse_account_link(&hexadecimal),
            Ok(AccountLink::Linked {
                pub_dress: "0x0sky".to_owned()
            })
        );
    }

    #[test]
    fn market_directory_rejects_missing_or_invalid_authority() {
        let unlinked = json!({
            "data": {
                "type": "issuer_account",
                "id": "0x1",
                "attributes": {"linked": false}
            }
        });
        assert_eq!(
            MarketGateway::parse_account_link(&unlinked),
            Ok(AccountLink::SignInRequired)
        );
        let mut missing_linked = unlinked.clone();
        missing_linked["data"]["attributes"] = json!({});
        assert_eq!(
            MarketGateway::parse_account_link(&missing_linked),
            Err(MarketError::InvalidResponse)
        );
        let mut missing_handle = unlinked.clone();
        missing_handle["data"]["attributes"]["linked"] = json!(true);
        assert_eq!(
            MarketGateway::parse_account_link(&missing_handle),
            Err(MarketError::InvalidResponse)
        );
        let mut empty_handle = missing_handle.clone();
        empty_handle["data"]["attributes"]["pub_dress"] = json!("");
        assert_eq!(
            MarketGateway::parse_account_link(&empty_handle),
            Err(MarketError::InvalidResponse)
        );
        let mut oversized_handle = missing_handle.clone();
        oversized_handle["data"]["attributes"]["pub_dress"] = json!("x".repeat(129));
        assert_eq!(
            MarketGateway::parse_account_link(&oversized_handle),
            Err(MarketError::InvalidResponse)
        );
        let mut wrong_issuer = unlinked;
        wrong_issuer["data"]["id"] = json!("0xda-market");
        assert_eq!(
            MarketGateway::parse_account_link(&wrong_issuer),
            Err(MarketError::InvalidResponse)
        );
    }

    #[test]
    fn configuration_rejects_plaintext_and_weak_assertion_key() {
        assert!(
            MarketGateway::new(
                "http://0xda-market.nilx.one/",
                "token".into(),
                "a-strong-test-secret-that-is-over-32-characters".into()
            )
            .is_err()
        );
        assert!(
            MarketGateway::new(
                "https://0xda-market.nilx.one/",
                "token".into(),
                "short".into()
            )
            .is_err()
        );
    }

    #[test]
    fn market_points_accepts_registered_market_addresses_only() {
        let valid = json!({"points": [
            {"address": "zoloti", "station_pub_dress": "0xda-market-zoloti-vorota", "path": "/zoloti"},
            {"address": "raion", "station_pub_dress": "0xda-market-raion", "path": "/raion"}
        ]});
        let decode = |value: &Value| parse_registered_points(value.to_string().as_bytes());
        let points = decode(&valid).unwrap();
        assert_eq!(points[0].address, "raion");
        assert_eq!(points[1].station_pub_dress, "0xda-market-zoloti-vorota");
        assert!(decode(&json!({"points": []})).unwrap().is_empty());
        assert!(decode(&json!({})).is_err());

        for (field, bad) in [
            ("path", "https://example.org/zoloti"),
            ("address", "../zoloti"),
            ("station_pub_dress", "0x1another-zoloti"),
        ] {
            let mut invalid = valid.clone();
            invalid["points"][0][field] = json!(bad);
            assert!(decode(&invalid).is_err());
        }
        let mut duplicate = valid.clone();
        duplicate["points"][1] = duplicate["points"][0].clone();
        assert!(decode(&duplicate).is_err());
    }

    /// One canned HTTP response on a local port; the request is read and dropped.
    fn serve_once(response: Vec<u8>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/points", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(&response);
        });
        url
    }

    fn http(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    #[tokio::test]
    async fn the_directory_is_read_only_from_a_small_successful_answer() {
        let body = br#"{"points":[{"address":"raion","station_pub_dress":"0xda-market-raion","path":"/raion"}]}"#;
        let ok = fetch_registered_points(&serve_once(http("200 OK", "", body))).await;
        assert_eq!(ok.unwrap()[0].address, "raion");

        let down =
            fetch_registered_points(&serve_once(http("503 Service Unavailable", "", b""))).await;
        assert!(matches!(down, Err(MarketError::Unavailable)));

        // A redirect is an answer, not something to follow.
        let moved = fetch_registered_points(&serve_once(http(
            "302 Found",
            "Location: https://example.org/points\r\n",
            b"",
        )))
        .await;
        assert!(matches!(moved, Err(MarketError::Unavailable)));

        let huge = vec![b' '; MAX_DIRECTORY_BYTES + 1];
        let too_big = fetch_registered_points(&serve_once(http("200 OK", "", &huge))).await;
        assert!(matches!(too_big, Err(MarketError::InvalidResponse)));
    }
}
