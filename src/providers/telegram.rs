// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! Telegram transport for the independent 0x1 text client.

use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, Message},
};

use crate::{
    application::{PublicAction, classify_message, public_reply},
    login::LoginGateway,
};

/// Only a private original message from a human may become a product command.
#[must_use]
pub const fn permits_private_command(
    private_chat: bool,
    human_sender: bool,
    forwarded: bool,
) -> bool {
    private_chat && human_sender && !forwarded
}

async fn handle_message(
    bot: Bot,
    message: Message,
    login: Option<&LoginGateway>,
) -> ResponseResult<()> {
    let human_sender = message.from.as_ref().is_some_and(|sender| !sender.is_bot);
    if !permits_private_command(
        message.chat.is_private(),
        human_sender,
        message.forward_origin().is_some(),
    ) {
        return Ok(());
    }

    let action = classify_message(message.text().unwrap_or_default());
    if action == PublicAction::StartLogin {
        let Some(gateway) = login else {
            bot.send_message(
                message.chat.id,
                "AIS-вхід поки недоступний: Core Gateway не підключений.",
            )
            .await?;
            return Ok(());
        };
        let sender_id = message.from.as_ref().expect("private sender checked").id.0;
        match gateway
            .start(sender_id, message.chat.id.0, message.id.0)
            .await
        {
            Ok(authorization_url) => {
                let keyboard = InlineKeyboardMarkup::new([[InlineKeyboardButton::url(
                    "Увійти через aiaiaiai AIS",
                    authorization_url,
                )]]);
                bot.send_message(message.chat.id, "0x1 · Авторизація через aiaiaiai AIS")
                    .reply_markup(keyboard)
                    .await?;
            }
            Err(_) => {
                bot.send_message(
                    message.chat.id,
                    "Не вдалося безпечно почати AIS-вхід. Повторіть спробу пізніше.",
                )
                .await?;
            }
        }
        return Ok(());
    }

    if let Some(text) = public_reply(action) {
        bot.send_message(message.chat.id, text).await?;
    }
    Ok(())
}

/// Only an isolated test bot may poll; no production token or gameplay writes.
pub async fn run_test_bot(token: String, login: Option<LoginGateway>) {
    let bot = Bot::new(token);
    teloxide::repl(bot, move |bot: Bot, message: Message| {
        let login = login.clone();
        async move { handle_message(bot, message, login.as_ref()).await }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_private_human_original_messages_may_execute_commands() {
        assert!(permits_private_command(true, true, false));
        assert!(!permits_private_command(false, true, false));
        assert!(!permits_private_command(true, false, false));
        assert!(!permits_private_command(true, true, true));
    }

    #[test]
    fn public_replies_do_not_redirect_gameplay_to_web() {
        for command in ["/start", "/Open", "/help", "/inventory"] {
            let text = public_reply(classify_message(command)).unwrap();
            assert!(!text.contains("https://nilx.one/telegram/"));
        }
    }
}
