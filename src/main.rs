// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! Entrypoint for the 0x1 Telegram bot.

use nilx_one_bot::{login::LoginGateway, providers::telegram::run_bot};
use teloxide::prelude::*;
use tracing_subscriber::EnvFilter;

const PRODUCTION_BOT_USERNAME: &str = "nilxone_bot";

fn matches_production_bot(actual: Option<&str>) -> bool {
    actual.is_some_and(|name| name.eq_ignore_ascii_case(PRODUCTION_BOT_USERNAME))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let check_only = match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => return Err(std::io::Error::other("Usage: nilx-one-bot [--check]").into()),
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("nilx_one_bot=info")),
        )
        .init();

    let token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
    if token.trim().is_empty() {
        return Err(std::io::Error::other("TELEGRAM_BOT_TOKEN is required").into());
    }
    let login = LoginGateway::from_env().map_err(std::io::Error::other)?;
    let bot = Bot::new(token);
    let me = bot
        .get_me()
        .await
        .map_err(|_| std::io::Error::other("Cannot verify Telegram bot identity"))?;
    if !matches_production_bot(me.user.username.as_deref()) {
        return Err(std::io::Error::other("TELEGRAM_BOT_TOKEN must belong to @nilxone_bot").into());
    }
    if !check_only {
        run_bot(bot, login).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_actual_telegram_bot_identity() {
        assert!(matches_production_bot(Some("nilxone_bot")));
        assert!(matches_production_bot(Some("NILXONE_BOT")));
        assert!(!matches_production_bot(Some("other_bot")));
        assert!(!matches_production_bot(None));
    }
}
