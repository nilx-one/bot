// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! Isolated test-only bot. Configuration failures are reported, not panics.

use nilx_one_bot::{login::LoginGateway, providers::telegram::run_test_bot};
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
    let login = LoginGateway::from_env().map_err(std::io::Error::other)?;
    run_test_bot(token.to_owned(), login).await;
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
}
