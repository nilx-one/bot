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

/// Assemble exactly the view and keyboard that handlers send to Telegram.
/// Tests exercise this path rather than an unused static response.
fn render_panel(
    panel: &Panel,
    screen: Screen,
    login_url: Option<&url::Url>,
    revision: u64,
) -> (View, InlineKeyboardMarkup) {
    let view = panel.view_for(screen, login_url, revision);
    let keyboard = markup(&view, revision);
    (view, keyboard)
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
    let (view, keyboard) = render_panel(&initial, Screen::Home, None, 0);
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
    let (view, keyboard) = render_panel(panel, screen, login_url.as_ref(), revision);
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
    update_message_id: i32,
    login: Option<&LoginGateway>,
) -> ResponseResult<()> {
    // Bind the transaction to the actual verified incoming command / callback
    // message, not a stored or zero-valued panel ID.
    let url = match login {
        Some(gateway) => match gateway.start(sender_id, chat_id.0, update_message_id).await {
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

/// Classify an actual private human Telegram input before the handler can
/// make or edit a panel. Unverified sources have no effect.
fn private_action(
    private_chat: bool,
    human_sender: bool,
    forwarded: bool,
    text: &str,
) -> Option<PublicAction> {
    permits_private_command(private_chat, human_sender, forwarded)
        .then(|| classify_message(text))
}

/// Only the stored owner, originating panel message and current revision may
/// navigate. Rendering this screen still grants no Core product authority.
fn callback_screen(
    panel: Option<&Panel>,
    private_chat: bool,
    human_sender: bool,
    requester_id: u64,
    message_id: i32,
    data: Option<&str>,
) -> Option<Screen> {
    if !private_chat || !human_sender {
        return None;
    }
    panel?.callback_target(requester_id, message_id, data.unwrap_or_default())
}

async fn handle_message(
    bot: Bot,
    message: Message,
    state: SharedUi,
    login: Option<LoginGateway>,
) -> ResponseResult<()> {
    let Some(sender) = &message.from else {
        return Ok(());
    };
    let Some(action) = private_action(
        message.chat.is_private(),
        !sender.is_bot,
        message.forward_origin().is_some(),
        message.text().unwrap_or_default(),
    ) else {
        return Ok(());
    };
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
                message.id.0,
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
    let chat = message.chat.id;
    let mut store = state.lock().await;
    let screen = callback_screen(
        store.panels.get(&chat.0),
        message.chat.is_private(),
        !query.from.is_bot,
        query.from.id.0,
        message.id.0,
        query.data.as_deref(),
    );
    bot.answer_callback_query(query.id).await?;
    match screen {
        Some(Screen::Login) => {
            update_login(
                &bot,
                &mut store,
                chat,
                query.from.id.0,
                message.id.0,
                login.as_ref(),
            )
            .await?;
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
    fn command_handlers_are_gated_by_actual_telegram_source() {
        for command in ["/start", "/START", "/Open"] {
            assert_eq!(
                private_action(true, true, false, command),
                Some(PublicAction::EnterTextClient)
            );
        }
        assert_eq!(
            private_action(true, true, false, "/SUBSCRIPTION"),
            Some(PublicAction::Subscription)
        );
        assert_eq!(
            private_action(true, true, false, "/inventory"),
            Some(PublicAction::NeedsCore)
        );
        assert_eq!(
            private_action(true, true, false, "лети на Майдан"),
            Some(PublicAction::Ignore)
        );
        for (private_chat, human_sender, forwarded) in [
            (false, true, false),
            (true, false, false),
            (true, true, true),
        ] {
            assert_eq!(
                private_action(private_chat, human_sender, forwarded, "/start"),
                None
            );
        }
    }

    #[test]
    fn handler_rendered_panel_is_text_only_and_never_invents_core_state() {
        let panel = Panel::new(8, 10);
        for (screen, expected) in [
            (Screen::Home, "Сесія Bond не підтверджена Core"),
            (Screen::Subscription, "Підписка недоступна"),
            (Screen::Inventory, "Інвентар недоступний"),
            (Screen::Travel, "Подорожі недоступні"),
            (Screen::Unavailable, "Жодних даних не змінено"),
        ] {
            let (view, keyboard) = render_panel(&panel, screen, None, 0);
            assert!(view.text.contains(expected), "{screen:?}: {}", view.text);
            let json = serde_json::to_string(&keyboard).unwrap();
            assert!(!json.contains("web_app"));
            assert!(!json.contains("nilx.one/telegram"));
        }
        let (view, keyboard) = render_panel(&panel, Screen::Home, None, 0);
        let json = serde_json::to_string(&keyboard).unwrap();
        assert!(json.contains("ui:0:subscription"));
        assert!(json.contains("ui:0:inventory"));
        assert!(!view.text.contains("Special активна"));
        assert!(!json.contains("/inventory"));
    }

    #[test]
    fn callback_handler_checks_owner_message_revision_and_source() {
        let mut panel = Panel::new(8, 10);
        assert_eq!(
            callback_screen(
                Some(&panel), true, true, 8, 10, Some("ui:0:subscription")
            ),
            Some(Screen::Subscription)
        );
        assert_eq!(
            callback_screen(Some(&panel), false, true, 8, 10, Some("ui:0:subscription")),
            None
        );
        assert_eq!(
            callback_screen(Some(&panel), true, false, 8, 10, Some("ui:0:subscription")),
            None
        );
        assert_eq!(
            callback_screen(Some(&panel), true, true, 9, 10, Some("ui:0:subscription")),
            None
        );
        assert_eq!(
            callback_screen(Some(&panel), true, true, 8, 11, Some("ui:0:subscription")),
            None
        );
        assert_eq!(
            callback_screen(None, true, true, 8, 10, Some("ui:0:subscription")),
            None
        );
        panel.change_screen(Screen::Subscription, None);
        assert_eq!(
            callback_screen(Some(&panel), true, true, 8, 10, Some("ui:0:subscription")),
            None
        );
        assert_eq!(
            callback_screen(Some(&panel), true, true, 8, 10, Some("ui:1:inventory")),
            Some(Screen::Inventory)
        );
    }
}
