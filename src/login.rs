// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! AIS login transport: the bot requests a Core-hosted, one-use OIDC
//! authorization transaction, then claims its verified callback for the
//! original `/login` message. It does not construct its own state/nonce, verify identity,
//! exchange codes, hold Bond sessions or tokens, or link accounts.

use ring::hmac;
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// `nilxone_api::bot_login::SIGNATURE_HEADER`.
const SIGNATURE_HEADER: &str = "x-0x1-telegram-bot-signature";
const START_PATH: &str = "/api/v1/telegram/login/start";
/// `nilxone_api::bot_login::CHAT_LOCATION_PATH`.
const LOCATION_PATH: &str = "/api/v1/telegram/location";
/// `nilxone_api::bot_login::CHAT_PROFILE_PATH`.
const PROFILE_PATH: &str = "/api/v1/telegram/profile";
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
    /// Derived from the bot's own Telegram token: the bot holds no other
    /// credential for the product host.
    key: Arc<hmac::Key>,
    registered_callback: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginError {
    Unavailable,
    InvalidResponse,
    /// Core has no verified browser step for this sender, chat and `/login`
    /// message: unfinished, expired or already used.
    NotConfirmable,
    /// The browser step reached Core and failed; this `/login` is over.
    Failed,
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

#[derive(Deserialize)]
pub(crate) struct BondProfile {
    pub pub_dress: Option<String>,
    pub experience: ProfileExperience,
}

#[derive(Deserialize)]
pub(crate) struct ProfileExperience {
    pub authority: String,
    pub bond_xp: u64,
    pub avaia_xp: u64,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProfileSection {
    Bond,
    Avaia,
    Wallet,
}

#[derive(Deserialize)]
pub(crate) struct AvaiaProfile {
    pub avaia: Option<OwnedAvaia>,
}

#[derive(Deserialize)]
pub(crate) struct OwnedAvaia {
    pub pub_dress: String,
    pub configured: bool,
    pub location: Option<AvaiaLocation>,
}

#[derive(Deserialize)]
pub(crate) struct AvaiaLocation {
    pub latitude_e7: i32,
    pub longitude_e7: i32,
}

#[derive(Deserialize)]
pub(crate) struct WalletProfile {
    pub seeds: Option<u64>,
}

#[derive(Serialize)]
struct ChatProfileRequest {
    version: u8,
    provider: &'static str,
    provider_actor_id: u64,
    chat_id: i64,
    #[serde(skip_serializing_if = "is_bond_section")]
    section: ProfileSection,
}

fn is_bond_section(section: &ProfileSection) -> bool {
    matches!(section, ProfileSection::Bond)
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
    /// Disabled if both settings are missing; a partial configuration fails at
    /// startup rather than running with a weaker login flow. `bot_token` is
    /// the `@nilxone_bot` token the bot already runs with.
    pub fn from_env(bot_token: &str) -> Result<Option<Self>, &'static str> {
        for retired in [
            "BOT_GATEWAY_SERVICE_TOKEN",
            "BOT_AIS_SIGNING_SEED",
            "BOT_AIS_KEY_ID",
        ] {
            if std::env::var_os(retired).is_some() {
                return Err(
                    "the bot signs login requests with TELEGRAM_BOT_TOKEN; remove BOT_AIS_SIGNING_SEED, BOT_AIS_KEY_ID and BOT_GATEWAY_SERVICE_TOKEN",
                );
            }
        }
        let start = std::env::var("BOT_LOGIN_START_URL").ok();
        let callback = std::env::var("BOT_OIDC_CALLBACK_URL").ok();
        match (start, callback) {
            (None, None) => Ok(None),
            (Some(start), Some(callback)) => Self::new(&start, bot_token, callback).map(Some),
            _ => Err("configure BOT_LOGIN_START_URL and BOT_OIDC_CALLBACK_URL together"),
        }
    }

    fn new(
        start_url: &str,
        bot_token: &str,
        registered_callback: String,
    ) -> Result<Self, &'static str> {
        let bot_token = bot_token.trim();
        let valid_token = bot_token.split_once(':').is_some_and(|(id, secret)| {
            !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) && !secret.is_empty()
        });
        if !valid_token {
            return Err("TELEGRAM_BOT_TOKEN is not a Telegram Bot API token");
        }
        let start_url = Url::parse(start_url).map_err(|_| "invalid gateway URL")?;
        let callback = Url::parse(&registered_callback).map_err(|_| "invalid callback URL")?;
        // Start and confirm always go to this one HTTPS endpoint: a host name
        // says nothing about the network it resolves on, so there is no
        // plain-HTTP exception for "internal" names.
        if start_url.scheme() != "https"
            || start_url.host_str().is_none()
            || start_url.port().is_some()
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
        {
            return Err("invalid secure login host configuration");
        }
        let http = Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(6))
            .build()
            .map_err(|_| "failed to initialize the HTTP transport")?;
        let derived = hmac::sign(
            &hmac::Key::new(hmac::HMAC_SHA256, b"0x1-telegram-bot-request-v1"),
            bot_token.as_bytes(),
        );
        Ok(Self {
            http,
            start_url,
            key: Arc::new(hmac::Key::new(hmac::HMAC_SHA256, derived.as_ref())),
            registered_callback,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(start_url: Url) -> Self {
        assert_eq!(start_url.host_str(), Some("127.0.0.1"));
        let mut gateway = Self::new(
            "https://nilx.one/api/v1/telegram/login/start",
            "123:TEST",
            "https://nilx.one/api/v1/telegram/login/callback".to_owned(),
        )
        .unwrap();
        gateway.start_url = start_url;
        gateway
    }

    /// `nilxone_api::bot_login::sign_request`:
    /// `t=<unix>,v1=<hex HMAC-SHA256(key, "<t>\n<path>\n<body>")>`.
    fn signature(&self, timestamp: u64, path: &str, body: &[u8]) -> String {
        let mut context = hmac::Context::with_key(&self.key);
        context.update(timestamp.to_string().as_bytes());
        context.update(b"\n");
        context.update(path.as_bytes());
        context.update(b"\n");
        context.update(body);
        let mut value = format!("t={timestamp},v1=");
        for byte in context.sign().as_ref() {
            use std::fmt::Write as _;
            let _ = write!(value, "{byte:02x}");
        }
        value
    }

    async fn post_signed(
        &self,
        path: &str,
        request: &impl Serialize,
    ) -> Result<reqwest::Response, LoginError> {
        let body = serde_json::to_vec(request).map_err(|_| LoginError::InvalidResponse)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| LoginError::Unavailable)?
            .as_secs();
        let mut url = self.start_url.clone();
        url.set_path(path);
        self.http
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(SIGNATURE_HEADER, self.signature(now, path, &body))
            .body(body)
            .send()
            .await
            .map_err(|_| LoginError::Unavailable)
    }

    pub(crate) async fn report_live_location(
        &self,
        actor: u64,
        chat: i64,
        latitude: f64,
        longitude: f64,
        observed_at: u64,
    ) -> Result<bool, LoginError> {
        if chat <= 0 || i64::try_from(actor).ok() != Some(chat) {
            return Err(LoginError::InvalidResponse);
        }
        #[derive(Serialize)]
        struct Report {
            chat: ChatProfileRequest,
            latitude: f64,
            longitude: f64,
            observed_at: u64,
        }
        let request = Report {
            chat: ChatProfileRequest {
                version: 1,
                provider: "telegram",
                provider_actor_id: actor,
                chat_id: chat,
                section: ProfileSection::Bond,
            },
            latitude,
            longitude,
            observed_at,
        };
        let response = self.post_signed(LOCATION_PATH, &request).await?;
        match response.status() {
            reqwest::StatusCode::NO_CONTENT => Ok(true),
            reqwest::StatusCode::CONFLICT => Ok(false),
            _ => Err(LoginError::Unavailable),
        }
    }

    pub(crate) async fn own_profile(
        &self,
        actor: u64,
        chat: i64,
    ) -> Result<Option<BondProfile>, LoginError> {
        let profile: Option<BondProfile> =
            self.read_profile(actor, chat, ProfileSection::Bond).await?;
        if profile
            .as_ref()
            .is_some_and(|p| p.experience.authority != "client")
        {
            return Err(LoginError::InvalidResponse);
        }
        Ok(profile)
    }

    pub(crate) async fn read_profile<T: serde::de::DeserializeOwned>(
        &self,
        actor: u64,
        chat: i64,
        section: ProfileSection,
    ) -> Result<Option<T>, LoginError> {
        if chat <= 0 || i64::try_from(actor).ok() != Some(chat) {
            return Err(LoginError::InvalidResponse);
        }
        let query = ChatProfileRequest {
            version: 1,
            provider: "telegram",
            provider_actor_id: actor,
            chat_id: chat,
            section,
        };
        let mut response = self.post_signed(PROFILE_PATH, &query).await?;
        let status = response.status();
        if status != reqwest::StatusCode::UNAUTHORIZED && !status.is_success() {
            return Err(LoginError::Unavailable);
        }
        const MAX_PROFILE_BYTES: usize = 4096;
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| LoginError::Unavailable)?
        {
            if chunk.len() > MAX_PROFILE_BYTES - bytes.len() {
                return Err(LoginError::InvalidResponse);
            }
            bytes.extend_from_slice(&chunk);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            let refusal: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|_| LoginError::InvalidResponse)?;
            return if refusal["error"]["code"] == "provider_authentication_required" {
                Ok(None)
            } else {
                Err(LoginError::Unavailable)
            };
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| LoginError::InvalidResponse)
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
        let result = self.post_signed(START_PATH, &request).await?;
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

    /// Claim the verified browser step of a `/login`. The bot asks right
    /// after login start until it is done: Core already proved at the
    /// callback that the identity belongs to this Telegram sender. The
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
        let result = self.post_signed(CONFIRM_PATH, &request).await?;
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

    let refusal = || {
        serde_json::from_slice::<Refusal>(body)
            .ok()
            .map(|refusal| refusal.error.code)
    };
    if status == reqwest::StatusCode::NOT_FOUND
        && refusal().as_deref() == Some("login_confirmation_not_found")
    {
        return Err(LoginError::NotConfirmable);
    }
    if status == reqwest::StatusCode::GONE && refusal().as_deref() == Some("login_failed") {
        return Err(LoginError::Failed);
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
            "123456:test-bot-token",
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
    fn requests_are_signed_with_the_bot_token_like_core_verifies_them() {
        let gateway = LoginGateway::new(
            "https://nilx.one/api/v1/telegram/login/start",
            "123456:test-bot-token",
            CALLBACK.into(),
        )
        .unwrap();
        // The vector nilxone_api::bot_login::sign_request produces.
        assert_eq!(
            gateway.signature(1_000, START_PATH, br#"{"version":1}"#),
            "t=1000,v1=c2426fd9704a0ebc90df96d56d8df7a9befc09cc46a2c58b2f8bb2909f167cfe"
        );
        assert_ne!(
            gateway.signature(1_000, CONFIRM_PATH, br#"{"version":1}"#),
            gateway.signature(1_000, START_PATH, br#"{"version":1}"#)
        );
        // Only the HTTPS login endpoint itself: no plain HTTP, whatever the
        // host name, no explicit port, no credentials, query or other path.
        for refused in [
            "http://nilxone-login-host:8080/api/v1/telegram/login/start",
            "http://nilxone-login-host/api/v1/telegram/login/start",
            "http://localhost/api/v1/telegram/login/start",
            "http://127.0.0.1/api/v1/telegram/login/start",
            "http://nilx.one/api/v1/telegram/login/start",
            "https://nilx.one:8443/api/v1/telegram/login/start",
            "https://user@nilx.one/api/v1/telegram/login/start",
            "https://nilx.one/api/v1/telegram/login/start?x=1",
            "https://nilx.one/api/v1/telegram/login/confirm",
            "ftp://nilx.one/api/v1/telegram/login/start",
        ] {
            assert!(
                LoginGateway::new(refused, "123456:test-bot-token", CALLBACK.into()).is_err(),
                "{refused}"
            );
        }
    }

    #[test]
    fn rejects_browser_callback_for_telegram_login() {
        assert!(
            LoginGateway::new(
                "https://example.com/api/v1/telegram/login/start",
                "123456:test-bot-token",
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
                "123456:test-bot-token",
                CALLBACK.into()
            )
            .is_err()
        );
        assert!(
            LoginGateway::new(
                "https://example.com/api/v1/telegram/login/start",
                "not-a-bot-token",
                CALLBACK.into()
            )
            .is_err()
        );
        assert!(
            LoginGateway::new(
                "https://example.com/api/v1/telegram/login/start",
                "123456:test-bot-token",
                CALLBACK.into()
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn confirm_rejects_another_chat_before_network() {
        let gateway = LoginGateway::new(
            "https://example.com/api/v1/telegram/login/start",
            "123456:test-bot-token",
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
            parse_confirmation(
                reqwest::StatusCode::GONE,
                br#"{"error":{"code":"login_failed"}}"#
            ),
            Err(LoginError::Failed)
        );
        assert_eq!(
            parse_confirmation(reqwest::StatusCode::GONE, b"gone"),
            Err(LoginError::Unavailable)
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
