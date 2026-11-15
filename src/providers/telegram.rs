// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! Telegram transport for the independent text client.
//! The Web Mini App remains in nilx-one/web but is not the play path here.

use teloxide::{prelude::*, types::Message};

use crate::application::{classify_message, public_reply};

/// No group, bot-originated or forwarded command may become a product action.
#[must_use]
pub const fn permits_private_command(
    private_chat: bool,
    human_sender: bool,
    forwarded: bool,
) -> bool {
    private_chat && human_sender && !forwarded
}

async fn handle_message(bot: Bot, message: Message) -> ResponseResult<()> {
    let human_sender = message.from.as_ref().is_some_and(|sender| !sender.is_bot);
    if !permits_private_command(
        message.chat.is_private(),
        human_sender,
        message.forward_origin().is_some(),
    ) {
        return Ok(());
    }

    if let Some(text) = public_reply(classify_message(message.text().unwrap_or_default())) {
        bot.send_message(message.chat.id, text).await?;
    }
    Ok(())
}

/// Only an isolated test bot may poll; no production token or gameplay writes.
pub async fn run_test_bot(token: String) {
    teloxide::repl(Bot::new(token), handle_message).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_human_original_message_is_the_only_accepted_source() {
        assert!(permits_private_command(true, true, false));
        assert!(!permits_private_command(false, true, false));
        assert!(!permits_private_command(true, false, false));
        assert!(!permits_private_command(true, true, true));
    }

    #[test]
    fn text_responses_never_send_a_web_app_button() {
        for command in ["/start", "/Open", "/help", "/inventory"] {
            let text = public_reply(classify_message(command)).unwrap();
            assert!(!text.contains("https://nilx.one/telegram/"));
        }
    }
}
