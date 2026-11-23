// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! AIS login transport: the bot requests a Core-hosted, one-use OIDC
//! authorization transaction, then forwards the original sender's confirm
//! press. It does not construct its own state/nonce, verify identity,
//! exchange codes, hold Bond sessions or tokens, or link accounts.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{
    rand::{SecureRandom as _, SystemRandom},
    signature::Ed25519KeyPair,
};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const TOKEN_ENDPOINT: &str = "https://auth.aiaiaiai.org/token";
const CLIENT_ID: &str = "0x1-telegram-test-bot";
const LOGIN_SCOPE: &str = "telegram.login.start";
const START_PATH: &str = "/api/v1/telegram/login/start";
/// `nilxone_api::bot_login::CONFIRM_PATH`, on the same product host.
const CONFIRM_PATH: &str = "/api/v1/telegram/login/confirm";

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
    key: Arc<Ed25519KeyPair>,
    kid: String,
    registered_callback: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginError {
    Unavailable,
    InvalidResponse,
    /// Core has no verified browser step for this sender, chat and `/login`
    /// message: unfinished, expired or already used.
    NotConfirmable,
}

/// Core's answer to a confirmation: the chat has its own product session.
/// A projection for display only; it authorizes nothing in the bot.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatSession {
    pub profile_id: String,
    pub pub_dress: Option<String>,
    /// Single Active Client: false while the Mini App or a browser is active.
    pub active: bool,
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
        if std::env::var_os("BOT_GATEWAY_SERVICE_TOKEN").is_some() {
            return Err("replace BOT_GATEWAY_SERVICE_TOKEN with the AIS bot signing key");
        }
        let seed = std::env::var("BOT_AIS_SIGNING_SEED").ok();
        let kid = std::env::var("BOT_AIS_KEY_ID").ok();
        let callback = std::env::var("BOT_OIDC_CALLBACK_URL").ok();
        match (start, seed, kid, callback) {
            (None, None, None, None) => Ok(None),
            (Some(start), Some(seed), Some(kid), Some(callback)) => {
                Self::new(&start, &seed, kid, callback).map(Some)
            }
            _ => {
                Err("configure the login start URL, AIS signing seed, key ID and callback together")
            }
        }
    }

    fn new(
        start_url: &str,
        seed: &str,
        kid: String,
        registered_callback: String,
    ) -> Result<Self, &'static str> {
        let seed = URL_SAFE_NO_PAD
            .decode(seed)
            .map_err(|_| "invalid AIS bot signing seed")?;
        let key = Ed25519KeyPair::from_seed_unchecked(&seed)
            .map_err(|_| "invalid AIS bot signing seed")?;
        let start_url = Url::parse(start_url).map_err(|_| "invalid gateway URL")?;
        let callback = Url::parse(&registered_callback).map_err(|_| "invalid callback URL")?;
        if start_url.scheme() != "https"
            || start_url.host_str().is_none()
            || !start_url.username().is_empty()
            || start_url.password().is_some()
            || start_url.fragment().is_some()
            || start_url.query().is_some()
            || start_url.path() != START_PATH
            || callback.scheme() != "https"
            || callback.host_str().is_none()
            || callback.path() != "/api/v1/telegram/login/callback"
            || !callback.username().is_empty()
            || callback.password().is_some()
            || callback.fragment().is_some()
            || callback.query().is_some()
            || kid.is_empty()
            || kid.len() > 64
            || !kid
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
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
            key: Arc::new(key),
            kid,
            registered_callback,
        })
    }

    fn assertion(&self) -> Result<String, LoginError> {
        let mut id = [0u8; 16];
        SystemRandom::new()
            .fill(&mut id)
            .map_err(|_| LoginError::Unavailable)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| LoginError::Unavailable)?
            .as_secs();
        let header = serde_json::json!({"alg":"EdDSA", "kid": self.kid});
        let claims = serde_json::json!({"iss":CLIENT_ID, "sub":CLIENT_ID, "aud":TOKEN_ENDPOINT,
            "iat":now, "exp":now+60, "jti":URL_SAFE_NO_PAD.encode(id)});
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let signature = self.key.sign(signed.as_bytes());
        Ok(format!(
            "{signed}.{}",
            URL_SAFE_NO_PAD.encode(signature.as_ref())
        ))
    }

    async fn access_token(&self) -> Result<String, LoginError> {
        #[derive(Deserialize)]
        struct Grant {
            access_token: String,
            token_type: String,
            scope: String,
            expires_in: u64,
        }
        let assertion = self.assertion()?;
        let response = self
            .http
            .post(TOKEN_ENDPOINT)
            .form(&[
                ("grant_type", "client_credentials"),
                ("audience", "0x1"),
                ("scope", LOGIN_SCOPE),
                (
                    "client_assertion_type",
                    "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
                ),
                ("client_assertion", assertion.as_str()),
            ])
            .send()
            .await
            .map_err(|_| LoginError::Unavailable)?;
        if !response.status().is_success() {
            return Err(LoginError::Unavailable);
        }
        let grant: Grant = response
            .json()
            .await
            .map_err(|_| LoginError::InvalidResponse)?;
        if grant.scope != LOGIN_SCOPE
            || !grant.token_type.eq_ignore_ascii_case("Bearer")
            || !(1..=300).contains(&grant.expires_in)
            || !(1..=4096).contains(&grant.access_token.len())
        {
            return Err(LoginError::InvalidResponse);
        }
        Ok(grant.access_token)
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
        if message_id <= 0 || chat_id <= 0 || i64::try_from(provider_actor_id).ok() != Some(chat_id)
        {
            return Err(LoginError::InvalidResponse);
        }
        let request = StartRequest::from_verified_update(provider_actor_id, chat_id, message_id);
        let result = self
            .http
            .post(self.start_url.clone())
            .bearer_auth(self.access_token().await?)
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

    /// Forward a confirm press from the verified original sender. The
    /// `login_message_id` is the message login start was bound to; Core
    /// accepts it only for the same sender and private chat, once.
    pub async fn confirm(
        &self,
        sender_id: u64,
        chat_id: i64,
        login_message_id: i32,
    ) -> Result<ChatSession, LoginError> {
        if login_message_id <= 0 || chat_id <= 0 || i64::try_from(sender_id).ok() != Some(chat_id) {
            return Err(LoginError::InvalidResponse);
        }
        let request = StartRequest::from_verified_update(sender_id, chat_id, login_message_id);
        let mut url = self.start_url.clone();
        url.set_path(CONFIRM_PATH);
        let result = self
            .http
            .post(url)
            .bearer_auth(self.access_token().await?)
            .json(&request)
            .send()
            .await
            .map_err(|_| LoginError::Unavailable)?;
        let status = result.status();
        let body = result.bytes().await.map_err(|_| LoginError::Unavailable)?;
        if body.len() > 4_096 {
            return Err(LoginError::InvalidResponse);
        }
        parse_confirmation(status, &body)
    }
}

fn parse_confirmation(status: reqwest::StatusCode, body: &[u8]) -> Result<ChatSession, LoginError> {
    #[derive(Deserialize)]
    struct Refusal {
        error: ErrorCode,
    }

    #[derive(Deserialize)]
    struct ErrorCode {
        code: String,
    }

    if status == reqwest::StatusCode::NOT_FOUND
        && serde_json::from_slice::<Refusal>(body)
            .is_ok_and(|refusal| refusal.error.code == "login_confirmation_not_found")
    {
        return Err(LoginError::NotConfirmable);
    }
    if !status.is_success() {
        return Err(LoginError::Unavailable);
    }
    let session: ChatSession =
        serde_json::from_slice(body).map_err(|_| LoginError::InvalidResponse)?;
    if session.profile_id.is_empty() || session.profile_id.len() > 128 {
        return Err(LoginError::InvalidResponse);
    }
    Ok(session)
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
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "test-key".into(),
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
    fn signed_workload_assertions_are_fresh_and_target_only_ais() {
        use ring::signature::{ED25519, KeyPair, UnparsedPublicKey};
        let gateway = LoginGateway::new(
            "https://nilx.one/api/v1/telegram/login/start",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "test-key".into(),
            CALLBACK.into(),
        )
        .unwrap();
        let assertion = gateway.assertion().unwrap();
        let (signed, signature) = assertion.rsplit_once('.').unwrap();
        UnparsedPublicKey::new(&ED25519, gateway.key.public_key().as_ref())
            .verify(
                signed.as_bytes(),
                &URL_SAFE_NO_PAD.decode(signature).unwrap(),
            )
            .unwrap();
        let decode = |s: &str| -> serde_json::Value {
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(s).unwrap()).unwrap()
        };
        let (header, claims) = signed.split_once('.').unwrap();
        assert_eq!(decode(header)["kid"], "test-key");
        let claims = decode(claims);
        assert_eq!(claims["iss"], CLIENT_ID);
        assert_eq!(claims["sub"], CLIENT_ID);
        assert_eq!(claims["aud"], TOKEN_ENDPOINT);
        assert_eq!(
            claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(),
            60
        );
        let next = gateway.assertion().unwrap();
        assert_ne!(
            claims["jti"],
            decode(next.split('.').nth(1).unwrap())["jti"]
        );
    }

    #[test]
    fn rejects_browser_callback_for_telegram_login() {
        assert!(
            LoginGateway::new(
                "https://example.com/api/v1/telegram/login/start",
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                "test-key".into(),
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
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                "test-key".into(),
                CALLBACK.into()
            )
            .is_err()
        );
        assert!(
            LoginGateway::new(
                "https://example.com/api/v1/telegram/login/start",
                "short",
                "test-key".into(),
                CALLBACK.into()
            )
            .is_err()
        );
        assert!(
            LoginGateway::new(
                "https://example.com/api/v1/telegram/login/start",
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                "test-key".into(),
                CALLBACK.into()
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn confirm_rejects_another_chat_before_network() {
        let gateway = LoginGateway::new(
            "https://example.com/api/v1/telegram/login/start",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "test-key".into(),
            CALLBACK.into(),
        )
        .unwrap();
        for (sender, chat, message) in [(42, 43, 7), (42, 42, 0), (0, 0, 7)] {
            assert_eq!(
                gateway.confirm(sender, chat, message).await,
                Err(LoginError::InvalidResponse)
            );
        }
    }

    #[test]
    fn confirmation_distinguishes_a_missing_attempt_from_a_missing_route() {
        let status = reqwest::StatusCode::NOT_FOUND;
        assert_eq!(
            parse_confirmation(
                status,
                br#"{"error":{"code":"login_confirmation_not_found"}}"#
            ),
            Err(LoginError::NotConfirmable)
        );
        assert_eq!(
            parse_confirmation(status, b"not found"),
            Err(LoginError::Unavailable)
        );
    }

    #[test]
    fn chat_session_projection_never_accepts_a_token() {
        let session: ChatSession =
            serde_json::from_str(r#"{"profile_id":"p","pub_dress":"0x0sky","active":false}"#)
                .unwrap();
        assert!(!session.active);
        assert!(
            serde_json::from_str::<ChatSession>(
                r#"{"profile_id":"p","pub_dress":null,"active":true,"token":"t"}"#
            )
            .is_err()
        );
    }
}
