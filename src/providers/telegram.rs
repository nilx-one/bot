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
    permits_private_command(private_chat, human_sender, forwarded).then(|| classify_message(text))
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

    // The first /start already sent the Home panel via ensure_panel. Editing
    // the just-sent identical screen adds a needless Telegram API request and
    // can race with the new panel before the user interacts.
    if is_first_panel && action == PublicAction::EnterTextClient {
        return Ok(());
    }

    match action {
        PublicAction::EnterTextClient => {
            edit_panel(&bot, &mut store, message.chat.id, Screen::Home, None).await?;
        }
        PublicAction::OpenMiniApp => {
            // A Telegram sender and bare Mini App URL are not proof of
            // Bond/device/session continuity. No WebView link until Core binds it.
            edit_panel(&bot, &mut store, message.chat.id, Screen::MiniApp, None).await?;
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
        for command in ["/start", "/START"] {
            assert_eq!(
                private_action(true, true, false, command),
                Some(PublicAction::EnterTextClient)
            );
        }
        assert_eq!(
            private_action(true, true, false, "/OPEN"),
            Some(PublicAction::OpenMiniApp)
        );
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
            (Screen::MiniApp, "Mini App недоступний"),
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
        assert!(json.contains("ui:0:mini_app"));
        assert!(!view.text.contains("Special активна"));
        assert!(!json.contains("/inventory"));
    }

    #[test]
    fn callback_handler_checks_owner_message_revision_and_source() {
        let mut panel = Panel::new(8, 10);
        assert_eq!(
            callback_screen(Some(&panel), true, true, 8, 10, Some("ui:0:subscription")),
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

#[cfg(test)]
mod transport_integration_tests {
    use super::*;
    use serde_json::{Value, json};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    // A local Telegram Bot API is enough to test actual handlers. No live
    // Telegram account, test token, or production endpoint is required.
    fn telegram_api(
        expected: Vec<&'static str>,
    ) -> (Bot, thread::JoinHandle<Vec<(String, Value)>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let bot = Bot::new("123:TEST")
            .set_api_url(url::Url::parse(&format!("http://{address}/")).unwrap());

        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for method in expected {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut bytes = Vec::new();
                let headers_end = loop {
                    let mut buffer = [0; 8192];
                    let n = stream.read(&mut buffer).unwrap();
                    assert!(n != 0, "unexpected EOF before request headers");
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let headers = std::str::from_utf8(&bytes[..headers_end]).unwrap();
                let request_path = headers.split_whitespace().nth(1).unwrap().to_owned();
                assert!(
                    request_path
                        .to_ascii_lowercase()
                        .ends_with(&method.to_ascii_lowercase()),
                    "unexpected method {request_path}"
                );
                let size: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|s| s.trim().parse().ok())
                    })
                    .unwrap_or(0);
                while bytes.len() - headers_end < size {
                    let mut buffer = [0; 8192];
                    let n = stream.read(&mut buffer).unwrap();
                    assert!(n != 0, "unexpected EOF before JSON body");
                    bytes.extend_from_slice(&buffer[..n]);
                }
                let body: Value =
                    serde_json::from_slice(&bytes[headers_end..headers_end + size]).unwrap();
                requests.push((method.to_owned(), body));

                let result = if method == "sendMessage" || method == "editMessageText" {
                    json!({
                        "message_id": 100,
                        "date": 1700000000,
                        "chat": {"id": 42, "type": "private", "first_name": "Human"},
                        "text": "0x1 · Text"
                    })
                } else {
                    json!(true)
                };
                let response = json!({"ok": true, "result": result}).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.len(),
                    response
                )
                .unwrap();
            }
            requests
        });
        (bot, handle)
    }

    fn incoming(message_id: i32, text: &str) -> Message {
        serde_json::from_value(json!({
            "message_id": message_id,
            "date": 1700000000,
            "chat": {"id": 42, "type": "private", "first_name": "Human"},
            "from": {"id": 42, "is_bot": false, "first_name": "Human"},
            "text": text
        }))
        .unwrap()
    }

    fn callback(message_id: i32, data: &str) -> CallbackQuery {
        serde_json::from_value(json!({
            "id": "callback-test-id",
            "from": {"id": 42, "is_bot": false, "first_name": "Human"},
            "chat_instance": "private-instance",
            "message": {
                "message_id": message_id,
                "date": 1700000000,
                "chat": {"id": 42, "type": "private", "first_name": "Human"},
                "text": "0x1 · Text"
            },
            "data": data
        }))
        .unwrap()
    }

    fn temp_ui() -> (SharedUi, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "nilx-bot-telegram-handler-{}-{nonce}.json",
            std::process::id()
        ));
        (
            Arc::new(Mutex::new(UiStore::open(path.clone()).unwrap())),
            path,
        )
    }

    #[tokio::test]
    async fn open_requires_core_continuity_and_never_launches_webview() {
        let (bot, server) = telegram_api(vec!["sendMessage", "editMessageText"]);
        let (state, path) = temp_ui();
        handle_message(bot, incoming(10, "/open"), state.clone(), None)
            .await
            .unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0, "sendMessage");
        assert_eq!(requests[1].0, "editMessageText");
        assert_eq!(requests[1].1["message_id"], 100);
        assert!(
            requests[1].1["text"]
                .as_str()
                .unwrap()
                .contains("Core не підтвердив спільну логічну Telegram-сесію")
        );
        let markup = requests[1].1["reply_markup"].to_string();
        assert!(!markup.contains("web_app"));
        assert!(!markup.contains("nilx.one/telegram"));
        let store = state.lock().await;
        assert_eq!(store.panels.get(&42).unwrap().screen, Screen::MiniApp);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn real_message_handler_reuses_one_panel_and_never_fakes_core_data() {
        let (bot, server) = telegram_api(vec![
            "sendMessage",
            "editMessageText",
            "editMessageText",
            "editMessageText",
        ]);
        let (state, path) = temp_ui();
        for (message_id, text) in [(10, "/start"), (11, "/SUBSCRIPTION"), (12, "/inventory")] {
            handle_message(bot.clone(), incoming(message_id, text), state.clone(), None)
                .await
                .unwrap();
        }
        // The fourth API call is an actual return to Home, not another send.
        handle_message(bot, incoming(13, "/start"), state.clone(), None)
            .await
            .unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].0, "sendMessage");
        for (method, body) in requests.iter().skip(1) {
            assert_eq!(method, "editMessageText");
            assert_eq!(body["message_id"], 100);
        }
        assert!(
            requests[1].1["text"]
                .as_str()
                .unwrap()
                .contains("Підписка недоступна")
        );
        assert!(
            requests[2].1["text"]
                .as_str()
                .unwrap()
                .contains("Жодних даних не змінено")
        );
        assert!(
            requests[3].1["text"]
                .as_str()
                .unwrap()
                .contains("Сесія Bond не підтверджена")
        );
        let store = state.lock().await;
        assert_eq!(store.panels.get(&42).unwrap().message_id, 100);
        assert_eq!(store.panels.get(&42).unwrap().screen, Screen::Home);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn real_handler_ignores_groups_bots_and_forwards_without_api_calls() {
        let (bot, server) = telegram_api(vec![]);
        let (state, path) = temp_ui();

        let mut group = serde_json::to_value(incoming(20, "/start")).unwrap();
        group["chat"]["type"] = json!("supergroup");
        group["chat"]["title"] = json!("test group");

        let mut bot_sender = serde_json::to_value(incoming(21, "/start")).unwrap();
        bot_sender["from"]["is_bot"] = json!(true);

        let mut forwarded = serde_json::to_value(incoming(22, "/start")).unwrap();
        forwarded["forward_origin"] = json!({
            "type": "user",
            "date": 1700000000,
            "sender_user": {"id": 99, "is_bot": false, "first_name": "Elsewhere"}
        });

        for update in [group, bot_sender, forwarded] {
            let message: Message = serde_json::from_value(update).unwrap();
            handle_message(bot.clone(), message, state.clone(), None)
                .await
                .unwrap();
        }
        assert!(state.lock().await.panels.is_empty());
        assert!(server.join().unwrap().is_empty());
        // The handler never saved a panel for an unauthorized source.
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn real_callback_handler_edits_only_a_fresh_authorized_panel() {
        let (bot, server) = telegram_api(vec![
            "answerCallbackQuery",
            "editMessageText",
            "answerCallbackQuery",
        ]);
        let (state, path) = temp_ui();
        {
            let mut store = state.lock().await;
            store.panels.insert(42, Panel::new(42, 100));
        }
        handle_callback(
            bot.clone(),
            callback(100, "ui:0:inventory"),
            state.clone(),
            None,
        )
        .await
        .unwrap();
        handle_callback(bot, callback(100, "ui:0:subscription"), state.clone(), None)
            .await
            .unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].0, "answerCallbackQuery");
        assert_eq!(requests[1].0, "editMessageText");
        assert_eq!(requests[2].0, "answerCallbackQuery");
        assert!(
            requests[1].1["text"]
                .as_str()
                .unwrap()
                .contains("Інвентар недоступний")
        );
        let store = state.lock().await;
        assert_eq!(store.panels.get(&42).unwrap().screen, Screen::Inventory);
        assert_eq!(store.panels.get(&42).unwrap().revision, 1);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
}
