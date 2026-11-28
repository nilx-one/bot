// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! Entrypoint for the 0x1 Telegram bot.

use nilx_one_bot::{
    login::{LoginGateway, gateway_public_key},
    providers::{telegram::run_bot, telegram_speech},
};
use teloxide::prelude::*;
use tracing_subscriber::EnvFilter;

const PRODUCTION_BOT_USERNAME: &str = "nilxone_bot";

fn matches_production_bot(actual: Option<&str>) -> bool {
    actual.is_some_and(|name| name.eq_ignore_ascii_case(PRODUCTION_BOT_USERNAME))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (check_only, public_key_only, check_gateway) =
        match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
            [] => (false, false, false),
            [flag] if flag == "--check" => (true, false, false),
            [flag] if flag == "--gateway-public-key" => (true, true, false),
            [flag] if flag == "--check-gateway" => (true, false, true),
            _ => {
                return Err(std::io::Error::other(
                    "Usage: nilx-one-bot [--check|--gateway-public-key|--check-gateway]",
                )
                .into());
            }
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
    let login = LoginGateway::from_env(&token).map_err(std::io::Error::other)?;
    let bot = Bot::new(token.clone());
    let me = bot
        .get_me()
        .await
        .map_err(|_| std::io::Error::other("Cannot verify Telegram bot identity"))?;
    if !matches_production_bot(me.user.username.as_deref()) {
        return Err(std::io::Error::other("TELEGRAM_BOT_TOKEN must belong to @nilxone_bot").into());
    }
    if public_key_only {
        let public_key = gateway_public_key(&token).map_err(std::io::Error::other)?;
        // Only print verified public metadata. No token, seed or signature.
        println!("TELEGRAM_BOT_ID={}", me.user.id.0);
        println!("TELEGRAM_BOT_PUBLIC_KEY={public_key}");
    } else if check_gateway {
        login
            .as_ref()
            .ok_or_else(|| std::io::Error::other("Gateway configuration is required"))?
            .check_authority()
            .await
            .map_err(|_| std::io::Error::other("Signed Gateway readiness probe failed"))?;
        println!("Telegram Gateway v2 signed probe passed");
    } else if !check_only {
        let speech_worker = login
            .clone()
            .map(|gateway| tokio::spawn(telegram_speech::run(bot.clone(), gateway)));
        let result = run_bot(bot, login).await;
        if let Some(worker) = speech_worker {
            worker.abort();
        }
        result?;
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
