// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! Telegram chat adapter. Mini App remains the shared Web product hosted
//! at nilx.one; opening it is not a second game-client activation.

use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, Message, WebAppInfo},
};
use url::Url;

use crate::application::{PublicAction, classify_message};

pub const MINI_APP_URL: &str = "https://nilx.one/telegram/";

#[must_use]
pub fn mini_app_keyboard() -> InlineKeyboardMarkup {
    let url = Url::parse(MINI_APP_URL).expect("canonical Telegram Mini App URL must be valid");
    InlineKeyboardMarkup::new([[InlineKeyboardButton::web_app(
        "Open 0x1",
        WebAppInfo { url },
    )]])
}

async fn handle_message(bot: Bot, message: Message) -> ResponseResult<()> {
    // Never treat a forum/group sender, forwarded text, or anonymous sender as
    // an authenticated Bond. Only private chats are handled by this slice.
    if !message.chat.is_private() || message.from.as_ref().is_none_or(|sender| sender.is_bot) {
        return Ok(());
    }

    match classify_message(message.text().unwrap_or_default()) {
        PublicAction::OpenMiniApp => {
            bot.send_message(message.chat.id, "0x1 · Відкрити Mini App")
                .reply_markup(mini_app_keyboard())
                .await?;
        }
        PublicAction::Help => {
            bot.send_message(
                message.chat.id,
                "Відкрийте 0x1 через /open. Дії з Bond доступні через активну сесію 0x1.",
            )
            .reply_markup(mini_app_keyboard())
            .await?;
        }
        PublicAction::NeedsCore => {
            bot.send_message(
                message.chat.id,
                "Ця команда поки недоступна в тестовому боті. Відкрийте 0x1 через /open.",
            )
            .reply_markup(mini_app_keyboard())
            .await?;
        }
        PublicAction::Ignore => {}
    }
    Ok(())
}

/// Only explicitly activated test deployments may poll. The production bot
/// remains solely owned by the existing Web identity process until cutover.
pub async fn run_test_bot(token: String) {
    let bot = Bot::new(token);
    teloxide::repl(bot, handle_message).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn open_uses_an_embedded_telegram_web_app_button() {
        let keyboard = serde_json::to_value(mini_app_keyboard()).expect("telegram markup");
        assert_eq!(
            keyboard,
            json!({
                "inline_keyboard": [[{
                    "text": "Open 0x1",
                    "web_app": {"url": MINI_APP_URL}
                }]]
            })
        );
    }
}
