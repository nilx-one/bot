// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! Isolated test-only bot. Configuration failures are reported, not panics.

use nilx_one_bot::{login::LoginGateway, providers::telegram::run_test_bot};
use teloxide::prelude::*;
use tracing_subscriber::EnvFilter;

fn test_token<'a>(mode: &str, token: &'a str) -> Result<&'a str, &'static str> {
    if mode != "test" {
        return Err("BOT_RUN_MODE must be test; production polling is not supported");
    }
    if token.trim().is_empty() {
        return Err("BOT_TEST_TOKEN must be a separate, nonempty test-bot token");
    }
    Ok(token)
}

// The isolated test transport must never consume @nilxone_bot updates.
const PRODUCTION_BOT_USERNAME: &str = "nilxone_bot";

fn test_username(name: &str) -> Result<&str, &'static str> {
    if !(5..=32).contains(&name.len())
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        || name.eq_ignore_ascii_case(PRODUCTION_BOT_USERNAME)
    {
        return Err("BOT_TEST_USERNAME must name a distinct Telegram test bot");
    }
    Ok(name)
}

fn matches_test_bot(expected: &str, actual: Option<&str>) -> bool {
    actual.is_some_and(|name| {
        name.eq_ignore_ascii_case(expected) && !name.eq_ignore_ascii_case(PRODUCTION_BOT_USERNAME)
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("nilx_one_bot=info")),
        )
        .init();

    let mode = std::env::var("BOT_RUN_MODE").unwrap_or_default();
    let token = std::env::var("BOT_TEST_TOKEN").unwrap_or_default();
    let token = test_token(&mode, &token).map_err(std::io::Error::other)?;
    if std::env::var_os("TELOXIDE_TOKEN").is_some() {
        return Err(
            std::io::Error::other("TELOXIDE_TOKEN is forbidden in the isolated test bot").into(),
        );
    }
    let username = std::env::var("BOT_TEST_USERNAME").unwrap_or_default();
    let username = test_username(&username).map_err(std::io::Error::other)?;
    // Telegram getMe identifies the credential's actual bot. Reject mismatches
    // before polling; never log the credential or reuse the production token.
    let me = Bot::new(token.to_owned())
        .get_me()
        .await
        .map_err(|_| std::io::Error::other("Cannot verify Telegram test bot identity"))?;
    if !matches_test_bot(username, me.user.username.as_deref()) {
        return Err(
            std::io::Error::other("Configured token does not belong to BOT_TEST_USERNAME").into(),
        );
    }
    let login = LoginGateway::from_env().map_err(std::io::Error::other)?;
    run_test_bot(token.to_owned(), login).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_test_mode_and_nonempty_test_token_are_accepted() {
        assert_eq!(test_token("test", "some-test-token"), Ok("some-test-token"));
        assert!(test_token("production", "some-token").is_err());
        assert!(test_token("", "some-token").is_err());
        assert!(test_token("test", "").is_err());
        assert!(test_token("test", "   ").is_err());
    }

    #[test]
    fn rejects_production_or_malformed_test_username() {
        assert!(test_username("nilxone_bot").is_err());
        assert!(test_username("NILXONE_BOT").is_err());
        assert!(test_username("@other_bot").is_err());
        assert!(test_username("").is_err());
        assert_eq!(test_username("other_bot"), Ok("other_bot"));
    }

    #[test]
    fn verifies_actual_telegram_bot_identity() {
        assert!(matches_test_bot("other_bot", Some("OTHER_BOT")));
        assert!(!matches_test_bot("other_bot", Some("nilxone_bot")));
        assert!(!matches_test_bot("nilxone_bot", Some("nilxone_bot")));
        assert!(!matches_test_bot("other_bot", Some("different_bot")));
        assert!(!matches_test_bot("other_bot", None));
    }
}
