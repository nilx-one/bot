// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! AIS login transport: the bot requests a Core-hosted, one-use OIDC
//! authorization transaction. It does not construct its own state/nonce,
//! verify identity, exchange codes, hold Bond sessions, or link accounts.

use std::time::Duration;

use reqwest::{Client, redirect::Policy};
use serde::{Deserialize, Serialize};
use url::Url;

/// A configured, authenticated product host endpoint, not the public AIS URL.
/// The host is responsible for binding this request to a verified provider
/// sender and creating durable, replay-safe OAuth state + PKCE.
#[derive(Clone)]
pub struct LoginGateway {
    http: Client,
    start_url: Url,
    service_token: String,
    registered_callback: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginError {
    Unavailable,
    InvalidResponse,
}

#[derive(Serialize)]
struct StartRequest {
    version: u8,
    provider: &'static str,
    provider_actor_id: u64,
    chat_id: i64,
    message_id: i32,
}

impl StartRequest {
    fn from_verified_update(provider_actor_id: u64, chat_id: i64, message_id: i32) -> Self {
        Self {
            version: 1,
            provider: "telegram",
            provider_actor_id,
            chat_id,
            message_id,
        }
    }
}

#[derive(Deserialize)]
struct StartResponse {
    authorization_url: String,
    expires_in_seconds: u64,
}

impl LoginGateway {
    /// Disabled if all settings are missing; partially configured settings
    /// fail at startup, rather than running with a weaker login flow.
    pub fn from_env() -> Result<Option<Self>, &'static str> {
        let start = std::env::var("BOT_LOGIN_START_URL").ok();
        let token = std::env::var("BOT_GATEWAY_SERVICE_TOKEN").ok();
        let callback = std::env::var("BOT_OIDC_CALLBACK_URL").ok();
        match (start, token, callback) {
            (None, None, None) => Ok(None),
            (Some(start), Some(token), Some(callback)) => {
                Self::new(&start, token, callback).map(Some)
            }
            _ => Err(
                "AIS login requires all BOT_LOGIN_START_URL, BOT_GATEWAY_SERVICE_TOKEN, BOT_OIDC_CALLBACK_URL settings",
            ),
        }
    }

    fn new(
        start_url: &str,
        service_token: String,
        registered_callback: String,
    ) -> Result<Self, &'static str> {
        let start_url = Url::parse(start_url).map_err(|_| "invalid gateway URL")?;
        let callback = Url::parse(&registered_callback).map_err(|_| "invalid callback URL")?;
        if start_url.scheme() != "https"
            || start_url.host_str().is_none()
            || !start_url.username().is_empty()
            || start_url.password().is_some()
            || start_url.fragment().is_some()
            || start_url.query().is_some()
            || start_url.path() != "/api/v1/telegram/login/start"
            || callback.scheme() != "https"
            || callback.host_str().is_none()
            || callback.path() != "/api/v1/telegram/login/callback"
            || !callback.username().is_empty()
            || callback.password().is_some()
            || callback.fragment().is_some()
            || callback.query().is_some()
            || service_token.trim().len() < 32
        {
            return Err("invalid secure AIS gateway configuration");
        }
        let http = Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(6))
            .build()
            .map_err(|_| "failed to initialize the HTTP transport")?;
        Ok(Self {
            http,
            start_url,
            service_token,
            registered_callback,
        })
    }

    /// A verified Telegram update supplies these actor/message IDs. The
    /// gateway authenticates the *bot service* and checks active-client policy.
    /// No Telegram token, password or OAuth verifier is exposed in chat.
    pub async fn start(
        &self,
        provider_actor_id: u64,
        chat_id: i64,
        message_id: i32,
    ) -> Result<Url, LoginError> {
        // Never issue a login transaction without a real Telegram message.
        if message_id <= 0 {
            return Err(LoginError::InvalidResponse);
        }
        let request = StartRequest::from_verified_update(provider_actor_id, chat_id, message_id);
        let result = self
            .http
            .post(self.start_url.clone())
            .bearer_auth(&self.service_token)
            .json(&request)
            .send()
            .await
            .map_err(|_| LoginError::Unavailable)?;
        if !result.status().is_success() {
            return Err(LoginError::Unavailable);
        }
        let body = result.bytes().await.map_err(|_| LoginError::Unavailable)?;
        if body.len() > 4_096 {
            return Err(LoginError::InvalidResponse);
        }
        let response: StartResponse =
            serde_json::from_slice(&body).map_err(|_| LoginError::InvalidResponse)?;
        if !(1..=900).contains(&response.expires_in_seconds)
            || response.authorization_url.len() > 2_048
        {
            return Err(LoginError::InvalidResponse);
        }
        let url =
            Url::parse(&response.authorization_url).map_err(|_| LoginError::InvalidResponse)?;
        if !valid_ais_authorization_url(&url, &self.registered_callback) {
            return Err(LoginError::InvalidResponse);
        }
        Ok(url)
    }
}

fn param(url: &Url, key: &str) -> Option<String> {
    let mut values = url.query_pairs().filter(|(name, _)| name == key);
    let first = values.next()?.1.into_owned();
    values.next().is_none().then_some(first)
}

/// Validate the *destination* before providing a Telegram link. Core/AIS still
/// own authorization and callback validation; this rejects forged gateway
/// output, redirects to foreign domains and downgraded PKCE.
#[must_use]
pub fn valid_ais_authorization_url(url: &Url, expected_callback: &str) -> bool {
    if url.scheme() != "https"
        || url.host_str() != Some("auth.aiaiaiai.org")
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.path() != "/0x1"
        || param(url, "client_id").as_deref() != Some("0x1")
        || param(url, "response_type").as_deref() != Some("code")
        || param(url, "redirect_uri").as_deref() != Some(expected_callback)
        || param(url, "code_challenge_method").as_deref() != Some("S256")
    {
        return false;
    }
    let Some(scope) = param(url, "scope") else {
        return false;
    };
    if !scope.split_whitespace().any(|s| s == "openid") {
        return false;
    }
    let valid_token =
        |key: &str| param(url, key).is_some_and(|v| (16..=256).contains(&v.len()) && v.is_ascii());
    let valid_challenge = param(url, "code_challenge").is_some_and(|v| {
        v.len() == 43
            && v.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    });
    valid_token("state") && valid_token("nonce") && valid_challenge
}

#[cfg(test)]
mod tests {
    use super::*;

    const CALLBACK: &str = "https://nilx.one/api/v1/telegram/login/callback";
    const CHALLENGE: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN_12";

    fn link() -> Url {
        let mut url = Url::parse("https://auth.aiaiaiai.org/0x1").unwrap();
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", "0x1")
            .append_pair("redirect_uri", CALLBACK)
            .append_pair("scope", "openid pub_dress")
            .append_pair("state", "unpredictable-random-state")
            .append_pair("nonce", "unpredictable-random-nonce")
            .append_pair("code_challenge", CHALLENGE)
            .append_pair("code_challenge_method", "S256");
        url
    }

    #[test]
    fn only_full_oidc_0x1_entry_is_accepted() {
        let url = link();
        assert!(valid_ais_authorization_url(&url, CALLBACK));
        let mut foreign = url.clone();
        foreign.set_host(Some("attacker.example")).unwrap();
        assert!(!valid_ais_authorization_url(&foreign, CALLBACK));
        let mut wrong = url.clone();
        wrong.set_path("/authorize");
        assert!(!valid_ais_authorization_url(&wrong, CALLBACK));
        let mut downgraded = url.clone();
        downgraded
            .query_pairs_mut()
            .append_pair("client_id", "nilx-one");
        assert!(!valid_ais_authorization_url(&downgraded, CALLBACK));
        let mut tampered = url.clone();
        tampered.query_pairs_mut().append_pair("state", "forged");
        assert!(!valid_ais_authorization_url(&tampered, CALLBACK));
        assert!(!valid_ais_authorization_url(
            &url,
            "https://evil.example/cb"
        ));
        assert!(!valid_ais_authorization_url(
            &Url::parse("https://auth.aiaiaiai.org/0x1").unwrap(),
            CALLBACK
        ));
    }

    #[test]
    fn login_start_payload_uses_the_verified_message_not_the_panel_id() {
        let request = StartRequest::from_verified_update(42, 100, 345);
        let body = serde_json::to_value(request).unwrap();
        assert_eq!(body["version"], 1);
        assert_eq!(body["provider"], "telegram");
        assert_eq!(body["provider_actor_id"], 42);
        assert_eq!(body["chat_id"], 100);
        assert_eq!(body["message_id"], 345);
    }

    #[tokio::test]
    async fn login_start_rejects_invalid_message_id_before_network() {
        let gateway = LoginGateway::new(
            "https://example.com/api/v1/telegram/login/start",
            "test-service-secret-abcdefghijklmnopqrstuvwxyz".into(),
            CALLBACK.into(),
        )
        .unwrap();
        assert_eq!(
            gateway.start(42, 100, 0).await,
            Err(LoginError::InvalidResponse)
        );
        assert_eq!(
            gateway.start(42, 100, -1).await,
            Err(LoginError::InvalidResponse)
        );
    }

    #[test]
    fn rejects_browser_callback_for_telegram_login() {
        assert!(
            LoginGateway::new(
                "https://example.com/api/v1/telegram/login/start",
                "test-service-secret-abcdefghijklmnopqrstuvwxyz".into(),
                "https://nilx.one/auth/callback".into()
            )
            .is_err()
        );
    }

    #[test]
    fn refuses_untrusted_gateway_config() {
        assert!(
            LoginGateway::new(
                "http://example.com/api/v1/telegram/login/start",
                "test-service-secret-abcdefghijklmnopqrstuvwxyz".into(),
                CALLBACK.into()
            )
            .is_err()
        );
        assert!(
            LoginGateway::new(
                "https://example.com/api/v1/telegram/login/start",
                "short".into(),
                CALLBACK.into()
            )
            .is_err()
        );
        assert!(
            LoginGateway::new(
                "https://example.com/api/v1/telegram/login/start",
                "test-service-secret-abcdefghijklmnopqrstuvwxyz".into(),
                CALLBACK.into()
            )
            .is_ok()
        );
    }
}
