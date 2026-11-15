// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! Telegram private-chat transport. One bot-owned panel is edited in place;
//! only a trusted Core callback can later complete authentication and
//! publish the new pinned dashboard. Telegram messages are never Core truth.

use std::{io, path::PathBuf, sync::Arc};

use teloxide::{
    dispatching::UpdateFilterExt,
    dptree,
    prelude::*,
    types::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, Message, MessageId},
};
use tokio::sync::Mutex;

use crate::{
    application::{PublicAction, classify_message},
    login::LoginGateway,
    ui::{Button, Panel, Screen, UiStore, View},
};

type SharedUi = Arc<Mutex<UiStore>>;

/// Only a private original message from a human may become a product command.
#[must_use]
pub const fn permits_private_command(
    private_chat: bool,
    human_sender: bool,
    forwarded: bool,
) -> bool {
    private_chat && human_sender && !forwarded
}

fn markup(view: &View, revision: u64) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(view.rows.iter().map(|row| {
        row.iter()
            .map(|button| match button {
                Button::Navigate(label, target) => InlineKeyboardButton::callback(
                    *label,
                    format!("ui:{revision}:{}", target.key()),
                ),
                Button::OpenUrl(label, url) => InlineKeyboardButton::url(*label, url.clone()),
            })
            .collect::<Vec<_>>()
    }))
}

/// The only normal sendMessage is creation of the first unauthenticated panel.
/// All subsequent commands and button navigation use editMessageText.
async fn ensure_panel(
    bot: &Bot,
    store: &mut UiStore,
    chat_id: ChatId,
    actor_id: u64,
) -> ResponseResult<()> {
    if store.panels.contains_key(&chat_id.0) {
        return Ok(());
    }
    let initial = Panel::new(actor_id, 0);
    let view = initial.view_for(Screen::Home, None, 0);
    let keyboard = markup(&view, 0);
    let sent = bot
        .send_message(chat_id, view.text)
        .reply_markup(keyboard)
        .await?;
    store
        .panels
        .insert(chat_id.0, Panel::new(actor_id, sent.id.0));
    persist(store);
    Ok(())
}

fn persist(store: &UiStore) {
    if let Err(error) = store.save() {
        tracing::warn!(%error, path = ?store.path(), "cannot persist Telegram UI panel metadata");
    }
}

/// Serializing the panel edit and revision update prevents callbacks from
/// reordering edits into a newer screen. UI state changes after API success.
async fn edit_panel(
    bot: &Bot,
    store: &mut UiStore,
    chat_id: ChatId,
    screen: Screen,
    login_url: Option<url::Url>,
) -> ResponseResult<()> {
    let Some(panel) = store.panels.get(&chat_id.0) else {
        return Ok(());
    };
    let revision = panel.revision.saturating_add(1);
    let view = panel.view_for(screen, login_url.as_ref(), revision);
    let keyboard = markup(&view, revision);
    bot.edit_message_text(chat_id, MessageId(panel.message_id), view.text)
        .reply_markup(keyboard)
        .await?;
    if let Some(panel) = store.panels.get_mut(&chat_id.0) {
        panel.change_screen(screen, login_url);
    }
    persist(store);
    Ok(())
}

async fn update_login(
    bot: &Bot,
    store: &mut UiStore,
    chat_id: ChatId,
    sender_id: u64,
    login: Option<&LoginGateway>,
) -> ResponseResult<()> {
    let message_id = store.panels.get(&chat_id.0).map_or(0, |p| p.message_id);
    let url = match login {
        Some(gateway) => match gateway.start(sender_id, chat_id.0, message_id).await {
            Ok(url) => Some(url),
            Err(error) => {
                tracing::warn!(?error, "AIS login-start unavailable");
                None
            }
        },
        None => None,
    };
    edit_panel(bot, store, chat_id, Screen::Login, url).await
}

async fn handle_message(
    bot: Bot,
    message: Message,
    state: SharedUi,
    login: Option<LoginGateway>,
) -> ResponseResult<()> {
    if !permits_private_command(
        message.chat.is_private(),
        message.from.as_ref().is_some_and(|sender| !sender.is_bot),
        message.forward_origin().is_some(),
    ) {
        return Ok(());
    }
    let Some(sender) = &message.from else {
        return Ok(());
    };
    let action = classify_message(message.text().unwrap_or_default());
    let mut store = state.lock().await;

    // Track observed incoming messages for later *best-effort* post-auth
    // cleanup. Telegram cannot delete an arbitrary 48h+ chat history.
    if let Some(panel) = store.panels.get_mut(&message.chat.id.0)
        && panel.owner_id == sender.id.0
    {
        panel.track(message.id.0);
        persist(&store);
    }

    if action == PublicAction::Ignore {
        return Ok(());
    }

    let is_first_panel = !store.panels.contains_key(&message.chat.id.0);
    ensure_panel(&bot, &mut store, message.chat.id, sender.id.0).await?;
    if is_first_panel {
        if let Some(panel) = store.panels.get_mut(&message.chat.id.0) {
            panel.track(message.id.0);
        }
        persist(&store);
    }
    if store
        .panels
        .get(&message.chat.id.0)
        .is_none_or(|panel| panel.owner_id != sender.id.0)
    {
        return Ok(());
    }

    match action {
        PublicAction::EnterTextClient => {
            edit_panel(&bot, &mut store, message.chat.id, Screen::Home, None).await?;
        }
        PublicAction::Help => {
            edit_panel(&bot, &mut store, message.chat.id, Screen::Help, None).await?;
        }
        PublicAction::StartLogin => {
            update_login(
                &bot,
                &mut store,
                message.chat.id,
                sender.id.0,
                login.as_ref(),
            )
            .await?;
        }
        PublicAction::Subscription => {
            edit_panel(
                &bot,
                &mut store,
                message.chat.id,
                Screen::Subscription,
                None,
            )
            .await?;
        }
        PublicAction::NeedsCore => {
            edit_panel(&bot, &mut store, message.chat.id, Screen::Unavailable, None).await?;
        }
        PublicAction::Ignore => {}
    }
    Ok(())
}

async fn handle_callback(
    bot: Bot,
    query: CallbackQuery,
    state: SharedUi,
    login: Option<LoginGateway>,
) -> ResponseResult<()> {
    // Always acknowledge so the Telegram button stops displaying a spinner.
    let Some(message) = query.message.as_ref().and_then(|m| m.regular_message()) else {
        bot.answer_callback_query(query.id).await?;
        return Ok(());
    };
    if !message.chat.is_private() || query.from.is_bot {
        bot.answer_callback_query(query.id).await?;
        return Ok(());
    }
    let chat = message.chat.id;
    let mut store = state.lock().await;
    let screen = store.panels.get(&chat.0).and_then(|panel| {
        panel.callback_target(
            query.from.id.0,
            message.id.0,
            query.data.as_deref().unwrap_or_default(),
        )
    });
    bot.answer_callback_query(query.id).await?;
    match screen {
        Some(Screen::Login) => {
            update_login(&bot, &mut store, chat, query.from.id.0, login.as_ref()).await?;
        }
        Some(next) => {
            edit_panel(&bot, &mut store, chat, next, None).await?;
        }
        None => {}
    }
    Ok(())
}

/// Only an isolated test bot may poll; never binds Telegram to a Bond here.
/// The file stores only chat/panel metadata, *not* AIS or Core sessions.
pub async fn run_test_bot(token: String, login: Option<LoginGateway>) -> io::Result<()> {
    let path =
        std::env::var("BOT_UI_STATE_PATH").unwrap_or_else(|_| "bot-ui-state.json".to_owned());
    let state: SharedUi = Arc::new(Mutex::new(UiStore::open(PathBuf::from(path))?));
    let bot = Bot::new(token);
    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_callback_query().endpoint(handle_callback));
    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state, login])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
    Ok(())
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
    fn buttons_are_navigation_not_slash_commands() {
        let panel = Panel::new(8, 10);
        let view = panel.view_for(Screen::Home, None, 0);
        let keyboard = markup(&view, 0);
        let serialized = serde_json::to_string(&keyboard).unwrap();
        assert!(serialized.contains("ui:0:subscription"));
        assert!(serialized.contains("ui:0:inventory"));
        assert!(!serialized.contains("/inventory"));
    }
}
