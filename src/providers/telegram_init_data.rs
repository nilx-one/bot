// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! Telegram Mini App evidence verifier for the future single credential owner.
//! This is a provider adapter, not an AIS login or Core product session.
//! It is not connected to a public route or used for production cutover.

use std::collections::BTreeMap;

use ring::hmac;
use serde::Deserialize;
use url::form_urlencoded;

const FUTURE_SKEW_SECONDS: u64 = 30;
const MAX_INIT_DATA_BYTES: usize = 16 * 1024;
const LABEL: &[u8] = b"WebAppData";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifiedTelegramUser {
    /// Provider-local identifier. Never forward directly as a Bond identity.
    pub id: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitDataRejection {
    Empty,
    TooLarge,
    DuplicateField,
    MissingHash,
    InvalidHash,
    MissingAuthDate,
    InvalidAuthDate,
    Expired,
    FutureAuthDate,
    MissingUser,
    InvalidUser,
    BotUser,
}

#[derive(Deserialize)]
struct TelegramUser {
    id: i64,
    #[serde(default)]
    is_bot: bool,
}

/// Verify signed Telegram initData without exposing the Bot API token.
/// Successful verification proves a Telegram provider account, not AIS
/// identity, Bond authorization, a device or an active product session.
pub fn verify_init_data(
    bot_token: &str,
    init_data: &str,
    now_unix_seconds: u64,
    max_age_seconds: u64,
) -> Result<VerifiedTelegramUser, InitDataRejection> {
    let fields = parse_fields(init_data)?;
    verify_hash(&fields, bot_token)?;

    let auth_date = fields
        .get("auth_date")
        .ok_or(InitDataRejection::MissingAuthDate)?
        .parse::<u64>()
        .map_err(|_| InitDataRejection::InvalidAuthDate)?;
    if auth_date > now_unix_seconds.saturating_add(FUTURE_SKEW_SECONDS) {
        return Err(InitDataRejection::FutureAuthDate);
    }
    if now_unix_seconds.saturating_sub(auth_date) > max_age_seconds {
        return Err(InitDataRejection::Expired);
    }

    let user: TelegramUser =
        serde_json::from_str(fields.get("user").ok_or(InitDataRejection::MissingUser)?)
            .map_err(|_| InitDataRejection::InvalidUser)?;
    if user.id <= 0 {
        return Err(InitDataRejection::InvalidUser);
    }
    if user.is_bot {
        return Err(InitDataRejection::BotUser);
    }
    Ok(VerifiedTelegramUser { id: user.id })
}

fn parse_fields(value: &str) -> Result<BTreeMap<String, String>, InitDataRejection> {
    if value.is_empty() {
        return Err(InitDataRejection::Empty);
    }
    if value.len() > MAX_INIT_DATA_BYTES {
        return Err(InitDataRejection::TooLarge);
    }
    let mut fields = BTreeMap::new();
    for (key, value) in form_urlencoded::parse(value.as_bytes()) {
        if fields
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return Err(InitDataRejection::DuplicateField);
        }
    }
    Ok(fields)
}

fn verify_hash(
    fields: &BTreeMap<String, String>,
    bot_token: &str,
) -> Result<(), InitDataRejection> {
    let hash = fields.get("hash").ok_or(InitDataRejection::MissingHash)?;
    let hash = decode_hash(hash)?;
    let data_check_string = fields
        .iter()
        .filter(|(key, _)| key.as_str() != "hash")
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n");
    let secret = hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA256, LABEL),
        bot_token.as_bytes(),
    );
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_ref());
    hmac::verify(&key, data_check_string.as_bytes(), &hash)
        .map_err(|_| InitDataRejection::InvalidHash)
}

fn decode_hash(hash: &str) -> Result<[u8; 32], InitDataRejection> {
    if hash.len() != 64 {
        return Err(InitDataRejection::InvalidHash);
    }
    let mut result = [0_u8; 32];
    for (index, pair) in hash.as_bytes().chunks_exact(2).enumerate() {
        let nibble = |value| match value {
            b'0'..=b'9' => Some(value - b'0'),
            b'a'..=b'f' => Some(value - b'a' + 10),
            _ => None,
        };
        let high = nibble(pair[0]).ok_or(InitDataRejection::InvalidHash)?;
        let low = nibble(pair[1]).ok_or(InitDataRejection::InvalidHash)?;
        result[index] = (high << 4) | low;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "123456:development-token";
    const NOW: u64 = 1_800_000_000;

    fn signed(token: &str, timestamp: u64, user: &str) -> String {
        let mut fields = BTreeMap::from([
            ("auth_date", timestamp.to_string()),
            ("query_id", "query-1".to_owned()),
            ("user", user.to_owned()),
        ]);
        let data = fields
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("\n");
        let secret = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, LABEL), token.as_bytes());
        let hash = hmac::sign(
            &hmac::Key::new(hmac::HMAC_SHA256, secret.as_ref()),
            data.as_bytes(),
        );
        fields.insert(
            "hash",
            hash.as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        );
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields)
            .finish()
    }

    #[test]
    fn accepts_valid_provider_evidence() {
        let data = signed(TOKEN, NOW - 2, r#"{"id":42,"first_name":"Test"}"#);
        assert_eq!(
            verify_init_data(TOKEN, &data, NOW, 300),
            Ok(VerifiedTelegramUser { id: 42 })
        );
    }

    #[test]
    fn rejects_tampering_and_cross_bot_token() {
        let data = signed(TOKEN, NOW, r#"{"id":42}"#);
        assert_eq!(
            verify_init_data(TOKEN, &data.replace("query-1", "query-2"), NOW, 300),
            Err(InitDataRejection::InvalidHash)
        );
        assert_eq!(
            verify_init_data("another-token", &data, NOW, 300),
            Err(InitDataRejection::InvalidHash)
        );
    }

    #[test]
    fn enforces_time_window_and_rejects_duplicate_fields() {
        let stale = signed(TOKEN, NOW - 301, r#"{"id":42}"#);
        let future = signed(TOKEN, NOW + 31, r#"{"id":42}"#);
        assert_eq!(
            verify_init_data(TOKEN, &stale, NOW, 300),
            Err(InitDataRejection::Expired)
        );
        assert_eq!(
            verify_init_data(TOKEN, &future, NOW, 300),
            Err(InitDataRejection::FutureAuthDate)
        );
        let duplicate = format!("{}&auth_date={NOW}", signed(TOKEN, NOW, r#"{"id":42}"#));
        assert_eq!(
            verify_init_data(TOKEN, &duplicate, NOW, 300),
            Err(InitDataRejection::DuplicateField)
        );
    }

    #[test]
    fn never_promotes_bad_provider_claims() {
        for (user, rejection) in [
            (r#"{"id":42,"is_bot":true}"#, InitDataRejection::BotUser),
            (r#"{"id":0}"#, InitDataRejection::InvalidUser),
            ("not json", InitDataRejection::InvalidUser),
        ] {
            assert_eq!(
                verify_init_data(TOKEN, &signed(TOKEN, NOW, user), NOW, 300),
                Err(rejection)
            );
        }
        assert_eq!(
            verify_init_data(TOKEN, "", NOW, 300),
            Err(InitDataRejection::Empty)
        );
        assert_eq!(
            verify_init_data(TOKEN, &"x".repeat(MAX_INIT_DATA_BYTES + 1), NOW, 300),
            Err(InitDataRejection::TooLarge)
        );
    }
}
