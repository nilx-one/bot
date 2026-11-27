// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! Telegram private-chat transport. One bot-owned panel is edited in place;
//! only a trusted Core callback can later complete authentication and
//! publish the new pinned dashboard. Telegram messages are never Core truth.

use std::{
    io,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use teloxide::{
    ApiError, RequestError,
    dispatching::UpdateFilterExt,
    dptree,
    prelude::*,
    types::{
        CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, Message, MessageId, WebAppInfo,
    },
};
use tokio::sync::Mutex;

use crate::{
    application::{PublicAction, classify_message},
    login::{AvaiaProfile, ChatSession, LoginError, LoginGateway, ProfileSection, WalletProfile},
    ui::{Button, Panel, Screen, UiStore, View},
};

const MINI_APP_URL: &str = "https://nilx.one/telegram/";
/// How often the bot asks Core whether the browser step of a `/login` is
/// done. Core allows 40 asks a minute per sender. Tests ask at once against
/// their local mock instead of waiting real seconds.
#[cfg(not(test))]
const LOGIN_POLL_INTERVAL: Duration = Duration::from_millis(2_500);
#[cfg(test)]
const LOGIN_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Core keeps a login five minutes before the browser step and a verified
/// callback five more; after that the answer can only be a failure.
const LOGIN_WAIT: Duration = Duration::from_secs(600);

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
                Button::OpenLoginPage(label, url) => {
                    InlineKeyboardButton::web_app(*label, WebAppInfo { url: url.clone() })
                }
                Button::OpenMiniApp => InlineKeyboardButton::web_app(
                    "Відкрити Mini App",
                    WebAppInfo {
                        url: url::Url::parse(MINI_APP_URL).expect("valid Telegram Mini App URL"),
                    },
                ),
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
    edit_panel_text(bot, store, chat_id, screen, login_url, None).await
}

async fn edit_panel_text(
    bot: &Bot,
    store: &mut UiStore,
    chat_id: ChatId,
    screen: Screen,
    login_url: Option<url::Url>,
    text: Option<String>,
) -> ResponseResult<()> {
    let Some(panel) = store.panels.get(&chat_id.0) else {
        return Ok(());
    };
    let revision = panel.revision.saturating_add(1);
    let (mut view, keyboard) = render_panel(panel, screen, login_url.as_ref(), revision);
    if let Some(text) = text {
        view.text = text;
    }
    let old_message = MessageId(panel.message_id);
    let resend = panel.resend;
    let edited = if resend {
        Err(RequestError::Api(ApiError::MessageToEditNotFound))
    } else {
        bot.edit_message_text(chat_id, old_message, view.text.clone())
            .reply_markup(keyboard.clone())
            .await
            .map(|_| ())
    };
    match edited {
        Ok(()) | Err(RequestError::Api(ApiError::MessageNotModified)) => {}
        // A typed command, or a panel that is gone (deleted) or can no
        // longer be edited: send a new one under the latest message.
        Err(RequestError::Api(
            ApiError::MessageToEditNotFound
            | ApiError::MessageCantBeEdited
            | ApiError::MessageIdInvalid,
        )) => {
            let sent = bot
                .send_message(chat_id, view.text)
                .reply_markup(keyboard)
                .await?;
            if let Some(panel) = store.panels.get_mut(&chat_id.0) {
                panel.message_id = sent.id.0;
                panel.resend = false;
            }
            // Best effort: an old panel left behind keeps stale buttons,
            // which the revision check already ignores.
            if resend && let Err(error) = bot.delete_message(chat_id, old_message).await {
                tracing::debug!(%error, "cannot delete the previous Telegram panel");
            }
        }
        Err(error) => return Err(error),
    }
    if let Some(panel) = store.panels.get_mut(&chat_id.0) {
        panel.change_screen(screen, login_url);
    }
    persist(store);
    Ok(())
}

/// What the panel shows when the login host cannot start a login.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StartFailure {
    /// `/start` is the entry: keep the player in the text client's Home.
    Home,
    /// `/login` or «Увійти» asked for a link: say it is unavailable.
    Unavailable,
}

/// The verified update that asked for a login: its sender, the message the
/// transaction is bound to, and what to show if the host cannot start it.
#[derive(Clone, Copy)]
struct LoginCommand {
    sender_id: u64,
    message_id: i32,
    on_failure: StartFailure,
}

async fn update_login(
    bot: &Bot,
    state: &SharedUi,
    store: &mut UiStore,
    chat_id: ChatId,
    command: LoginCommand,
    login: Option<&LoginGateway>,
) -> ResponseResult<()> {
    let LoginCommand {
        sender_id,
        message_id: update_message_id,
        on_failure,
    } = command;
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
    let started = url.is_some();
    let screen = if started || on_failure == StartFailure::Unavailable {
        Screen::Login
    } else {
        Screen::Home
    };
    edit_panel(bot, store, chat_id, screen, url).await?;
    if let Some(panel) = store.panels.get_mut(&chat_id.0) {
        panel.login_message_id = started.then_some(update_message_id);
    }
    persist(store);
    if started && let Some(gateway) = login {
        tokio::spawn(await_login(
            bot.clone(),
            Arc::clone(state),
            gateway.clone(),
            chat_id,
            sender_id,
            update_message_id,
        ));
    }
    Ok(())
}

/// Whether `login_message_id` is still this chat's pending `/login`. A newer
/// `/login`, a legacy confirm press or a finished login ends the wait.
fn awaits(store: &UiStore, chat_id: ChatId, login_message_id: i32) -> bool {
    store
        .panels
        .get(&chat_id.0)
        .is_some_and(|panel| panel.login_message_id == Some(login_message_id))
}

enum Claim {
    /// Ask again: the browser step is unfinished or Core was briefly away.
    Waiting,
    /// The login is over: signed in, or failed or expired (`None`).
    Done(Option<ChatSession>),
}

fn after_claim(outcome: Result<ChatSession, LoginError>, expired: bool) -> Claim {
    match outcome {
        Ok(session) => Claim::Done(Some(session)),
        Err(LoginError::NotConfirmable | LoginError::Unavailable) if !expired => Claim::Waiting,
        Err(error) => {
            tracing::info!(?error, "Telegram login did not complete");
            Claim::Done(None)
        }
    }
}

/// After `/login`, claim the verified browser step as soon as Core has it,
/// so the chat shows the result without a press. Core already proved at the
/// callback that the AIS identity belongs to this sender. A failure or the
/// end of the wait shows that the login did not happen.
async fn await_login(
    bot: Bot,
    state: SharedUi,
    gateway: LoginGateway,
    chat_id: ChatId,
    sender_id: u64,
    login_message_id: i32,
) {
    let deadline = Instant::now() + LOGIN_WAIT;
    loop {
        tokio::time::sleep(LOGIN_POLL_INTERVAL).await;
        if !awaits(&*state.lock().await, chat_id, login_message_id) {
            return;
        }
        let outcome = gateway
            .confirm(sender_id, chat_id.0, login_message_id)
            .await;
        let Claim::Done(session) = after_claim(outcome, Instant::now() >= deadline) else {
            continue;
        };
        let mut store = state.lock().await;
        if !awaits(&store, chat_id, login_message_id) {
            return;
        }
        let signed_in = session.is_some();
        if let Some(panel) = store.panels.get_mut(&chat_id.0) {
            panel.login_message_id = None;
            if session.is_some() {
                panel.chat_session = session;
            }
        }
        let screen = if signed_in {
            Screen::SignedIn
        } else {
            Screen::LoginFailed
        };
        if let Err(error) = edit_panel(&bot, &mut store, chat_id, screen, None).await {
            tracing::warn!(%error, "cannot show the Telegram login result");
            persist(&store);
        }
        return;
    }
}

/// The owner pressed confirm on the current panel. Core decides whether the
/// browser step belongs to this sender, chat and login message; the bot only
/// shows the answer.
async fn confirm_login(
    bot: &Bot,
    store: &mut UiStore,
    chat_id: ChatId,
    sender_id: u64,
    login: Option<&LoginGateway>,
) -> ResponseResult<()> {
    let panel = store.panels.get(&chat_id.0);
    if panel.is_some_and(|panel| {
        panel.chat_session.is_some()
            && panel.login_url.is_some()
            && panel.login_message_id.is_none()
    }) {
        // Core already confirmed this press. Retry the UI edit without consuming it again.
        return edit_panel(bot, store, chat_id, Screen::SignedIn, None).await;
    }
    let pending = panel.and_then(|panel| panel.login_message_id.zip(panel.login_url.clone()));
    let (Some(gateway), Some((login_message_id, url))) = (login, pending) else {
        return edit_panel(bot, store, chat_id, Screen::Login, None).await;
    };
    match gateway
        .confirm(sender_id, chat_id.0, login_message_id)
        .await
    {
        Ok(session) => {
            if let Some(panel) = store.panels.get_mut(&chat_id.0) {
                panel.chat_session = Some(session);
                panel.login_message_id = None;
            }
            edit_panel(bot, store, chat_id, Screen::SignedIn, None).await
        }
        Err(LoginError::NotConfirmable) => {
            // Keep the same link: the browser step may still be unfinished.
            edit_panel(bot, store, chat_id, Screen::ConfirmLogin, Some(url)).await
        }
        Err(LoginError::Failed) => {
            if let Some(panel) = store.panels.get_mut(&chat_id.0) {
                panel.login_message_id = None;
            }
            edit_panel(bot, store, chat_id, Screen::LoginFailed, None).await
        }
        Err(error) => {
            tracing::warn!(?error, "Telegram login confirmation unavailable");
            edit_panel(bot, store, chat_id, Screen::Login, None).await
        }
    }
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

async fn profile_text(
    host: &LoginGateway,
    actor: u64,
    chat: i64,
    section: ProfileSection,
) -> Result<Option<String>, LoginError> {
    match section {
        ProfileSection::Bond => Ok(host.own_profile(actor, chat).await?.map(|profile| {
            format!(
                "{}\n\nДосвід за даними клієнта\nBond · {} XP\nAvaia · {} XP",
                profile
                    .pub_dress
                    .as_deref()
                    .unwrap_or("Без публічної адреси"),
                profile.experience.bond_xp,
                profile.experience.avaia_xp,
            )
        })),
        ProfileSection::Avaia => {
            let profile: Option<AvaiaProfile> = host.read_profile(actor, chat, section).await?;
            Ok(profile.map(|profile| {
                let Some(avaia) = profile.avaia else {
                    return "Core поки не має даних про твою Avaia.".to_owned();
                };
                let status = if avaia.configured {
                    "Налаштована"
                } else {
                    "Ще не налаштована"
                };
                let location = avaia.location.map_or_else(
                    || "Ще не збережено".to_owned(),
                    |location| {
                        format!(
                            "{:.5}, {:.5}",
                            f64::from(location.latitude_e7) / 10_000_000.0,
                            f64::from(location.longitude_e7) / 10_000_000.0
                        )
                    },
                );
                format!(
                    "{}\n{}\nОстання збережена позиція · {}",
                    avaia.pub_dress, status, location
                )
            }))
        }
        ProfileSection::Wallet => {
            let profile: Option<WalletProfile> = host.read_profile(actor, chat, section).await?;
            Ok(profile.map(|profile| {
                let balance = profile.seeds.map_or_else(
                    || "Гаманець Seeds недоступний для цієї сесії.".to_owned(),
                    |seeds| format!("Гаманець · {seeds} Seeds"),
                );
                format!("{balance}\nКишеня та рюкзак доступні на клієнті, де збережено інвентар.")
            }))
        }
    }
}

async fn show_profile(
    bot: &Bot,
    state: &SharedUi,
    chat: ChatId,
    actor: u64,
    panel_at_request: (i32, u64),
    screen: Screen,
    login: Option<&LoginGateway>,
) -> ResponseResult<()> {
    let (title, section, unavailable) = match screen {
        Screen::Bond => (
            "Bond",
            ProfileSection::Bond,
            "Не вдалося отримати профіль Bond. Спробуй /bond ще раз.",
        ),
        Screen::Avaia => (
            "Avaia",
            ProfileSection::Avaia,
            "Дані Avaia недоступні. Спробуй /avaia ще раз.",
        ),
        Screen::Inventory => (
            "Інвентар",
            ProfileSection::Wallet,
            "Інвентар недоступний. Спробуй /inventory ще раз.",
        ),
        _ => return Ok(()),
    };
    let profile = match login {
        Some(host) => profile_text(host, actor, chat.0, section).await,
        None => Err(LoginError::Unavailable),
    };
    let detail = match profile {
        Ok(Some(text)) => text,
        Ok(None) => "Щоб переглянути дані, увійди через /login.".to_owned(),
        Err(_) => unavailable.to_owned(),
    };
    let text = format!("0x1 · {title}\n\n{detail}");
    let mut store = state.lock().await;
    if store.panels.get(&chat.0).is_some_and(|panel| {
        panel.owner_id == actor && (panel.message_id, panel.revision) == panel_at_request
    }) {
        edit_panel_text(bot, &mut store, chat, screen, None, Some(text)).await?;
    }
    Ok(())
}

async fn show_market_points(
    bot: &Bot,
    state: &SharedUi,
    chat: ChatId,
    actor: u64,
    panel_at_request: (i32, u64),
) -> ResponseResult<()> {
    use std::fmt::Write as _;

    let mut text = "0x1 · xPing\n\n".to_owned();
    match crate::market::registered_market_points().await {
        Ok(points) if points.is_empty() => text.push_str("У реєстрі 0xda-market поки немає точок."),
        Ok(points) => {
            text.push_str("Зареєстровані точки 0xda-market");
            for point in points.iter().take(30) {
                let _ = write!(text, "\n{} · {}", point.address, point.station_pub_dress);
            }
            if points.len() > 30 {
                let _ = write!(text, "\nПоказано 30 із {}.", points.len());
            }
            text.push_str("\n\nЗапуск подорожі з чату ще недоступний.");
        }
        Err(_) => text.push_str("Реєстр 0xda-market зараз недоступний. Спробуй /xping ще раз."),
    }
    let mut store = state.lock().await;
    if store.panels.get(&chat.0).is_some_and(|panel| {
        panel.owner_id == actor && (panel.message_id, panel.revision) == panel_at_request
    }) {
        edit_panel_text(
            bot,
            &mut store,
            chat,
            Screen::MarketPoints,
            None,
            Some(text),
        )
        .await?;
    }
    Ok(())
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

    // With no gateway, the first /start already sent the offline Home panel.
    // A configured gateway must instead start the same login as /login.
    if is_first_panel && action == PublicAction::EnterTextClient && login.is_none() {
        return Ok(());
    }
    // Answer a typed command under it, not in a panel far up or hidden.
    if !is_first_panel && let Some(panel) = store.panels.get_mut(&message.chat.id.0) {
        panel.resend = true;
    }

    // A chat that already holds a session is not asked to sign in again: its
    // display projection only chooses the screen and authorizes nothing. A
    // restart forgets it, so /start then asks for sign-in again.
    let signed_in = store
        .panels
        .get(&message.chat.id.0)
        .is_some_and(|panel| panel.chat_session.is_some());

    match action {
        PublicAction::EnterTextClient if login.is_none() => {
            edit_panel(&bot, &mut store, message.chat.id, Screen::Home, None).await?;
        }
        PublicAction::EnterTextClient if signed_in => {
            edit_panel(&bot, &mut store, message.chat.id, Screen::SignedIn, None).await?;
        }
        PublicAction::OpenMiniApp => {
            edit_panel(&bot, &mut store, message.chat.id, Screen::MiniApp, None).await?;
        }
        PublicAction::Help => {
            edit_panel(&bot, &mut store, message.chat.id, Screen::Help, None).await?;
        }
        PublicAction::EnterTextClient | PublicAction::StartLogin => {
            update_login(
                &bot,
                &state,
                &mut store,
                message.chat.id,
                LoginCommand {
                    sender_id: sender.id.0,
                    message_id: message.id.0,
                    on_failure: if action == PublicAction::EnterTextClient {
                        StartFailure::Home
                    } else {
                        StartFailure::Unavailable
                    },
                },
                login.as_ref(),
            )
            .await?;
        }
        PublicAction::OwnBond | PublicAction::OwnAvaia | PublicAction::Inventory => {
            let screen = match action {
                PublicAction::OwnAvaia => Screen::Avaia,
                PublicAction::Inventory => Screen::Inventory,
                _ => Screen::Bond,
            };
            let panel = &store.panels[&message.chat.id.0];
            let panel_at_request = (panel.message_id, panel.revision);
            drop(store);
            return show_profile(
                &bot,
                &state,
                message.chat.id,
                sender.id.0,
                panel_at_request,
                screen,
                login.as_ref(),
            )
            .await;
        }
        PublicAction::MarketPoints => {
            let panel = &store.panels[&message.chat.id.0];
            let panel_at_request = (panel.message_id, panel.revision);
            drop(store);
            return show_market_points(
                &bot,
                &state,
                message.chat.id,
                sender.id.0,
                panel_at_request,
            )
            .await;
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
                &state,
                &mut store,
                chat,
                LoginCommand {
                    sender_id: query.from.id.0,
                    message_id: message.id.0,
                    on_failure: StartFailure::Unavailable,
                },
                login.as_ref(),
            )
            .await?;
        }
        Some(screen @ (Screen::Bond | Screen::Avaia | Screen::Inventory)) => {
            let panel = &store.panels[&chat.0];
            let panel_at_request = (panel.message_id, panel.revision);
            drop(store);
            return show_profile(
                &bot,
                &state,
                chat,
                query.from.id.0,
                panel_at_request,
                screen,
                login.as_ref(),
            )
            .await;
        }
        Some(Screen::MarketPoints) => {
            let panel = &store.panels[&chat.0];
            let panel_at_request = (panel.message_id, panel.revision);
            drop(store);
            return show_market_points(&bot, &state, chat, query.from.id.0, panel_at_request).await;
        }
        Some(Screen::ConfirmLogin) => {
            confirm_login(&bot, &mut store, chat, query.from.id.0, login.as_ref()).await?;
        }
        Some(next) => {
            edit_panel(&bot, &mut store, chat, next, None).await?;
        }
        None => {}
    }
    Ok(())
}

/// Start polling with the Telegram client verified by the entrypoint.
/// The file stores only chat/panel metadata, *not* AIS or Core sessions.
pub async fn run_bot(bot: Bot, login: Option<LoginGateway>) -> io::Result<()> {
    let path =
        std::env::var("BOT_UI_STATE_PATH").unwrap_or_else(|_| "bot-ui-state.json".to_owned());
    let state: SharedUi = Arc::new(Mutex::new(UiStore::open(PathBuf::from(path))?));
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
            private_action(true, true, false, "/bond"),
            Some(PublicAction::OwnBond)
        );
        assert_eq!(
            private_action(true, true, false, "/inventory"),
            Some(PublicAction::Inventory)
        );
        assert_eq!(
            private_action(true, true, false, "лети на Майдан"),
            Some(PublicAction::Ignore)
        );
        assert_eq!(
            private_action(true, true, false, "/xping@nilxone_bot"),
            Some(PublicAction::MarketPoints)
        );
        for (private_chat, human_sender, forwarded) in [
            (false, true, false),
            (true, false, false),
            (true, true, true),
        ] {
            for command in ["/start", "/bond", "/avaia", "/inventory", "/xping"] {
                assert_eq!(
                    private_action(private_chat, human_sender, forwarded, command),
                    None
                );
            }
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

    #[test]
    fn the_wait_ends_on_a_session_a_failure_or_expiry_only() {
        let session = ChatSession {
            profile_id: "p".into(),
            pub_dress: None,
            active: true,
        };
        assert!(matches!(
            after_claim(Ok(session.clone()), true),
            Claim::Done(Some(found)) if found == session
        ));
        for waiting in [LoginError::NotConfirmable, LoginError::Unavailable] {
            assert!(matches!(after_claim(Err(waiting), false), Claim::Waiting));
            assert!(matches!(after_claim(Err(waiting), true), Claim::Done(None)));
        }
        for over in [LoginError::Failed, LoginError::InvalidResponse] {
            assert!(matches!(after_claim(Err(over), false), Claim::Done(None)));
        }
    }

    #[test]
    fn only_the_latest_login_message_is_awaited() {
        let mut store = UiStore::open(
            std::env::temp_dir().join(format!("nilx-bot-awaits-{}.json", std::process::id())),
        )
        .unwrap();
        let chat = ChatId(42);
        assert!(!awaits(&store, chat, 7));
        let mut panel = Panel::new(42, 10);
        panel.login_message_id = Some(7);
        store.panels.insert(42, panel);
        assert!(awaits(&store, chat, 7));
        assert!(!awaits(&store, chat, 6), "a newer /login replaced it");
        assert!(!awaits(&store, ChatId(43), 7));
        store.panels.get_mut(&42).unwrap().login_message_id = None;
        assert!(!awaits(&store, chat, 7), "the login is over");
    }

    #[test]
    fn login_link_needs_no_confirm_button_and_the_session_is_core_projection() {
        let mut panel = Panel::new(8, 10);
        let url = url::Url::parse("https://auth.aiaiaiai.org/0x1?state=s").unwrap();
        for screen in [Screen::Login, Screen::ConfirmLogin] {
            let (_, keyboard) = render_panel(&panel, screen, Some(&url), 3);
            let json = serde_json::to_string(&keyboard).unwrap();
            // A Mini App button, so Telegram signs the sender for the page.
            assert!(
                json.contains(r#""web_app":{"url":"https://auth.aiaiaiai.org/0x1?state=s"}"#),
                "{screen:?}: {json}"
            );
            // The bot claims the login by itself: nothing to press.
            assert!(!json.contains("confirm_login"), "{screen:?}");
        }
        let (view, keyboard) = render_panel(&panel, Screen::LoginFailed, None, 3);
        assert!(view.text.contains("Вхід не вдався"), "{}", view.text);
        assert!(
            serde_json::to_string(&keyboard)
                .unwrap()
                .contains("ui:3:login")
        );
        let (_, keyboard) = render_panel(&panel, Screen::Login, None, 3);
        assert!(
            !serde_json::to_string(&keyboard)
                .unwrap()
                .contains("confirm_login")
        );
        let (view, _) = render_panel(&panel, Screen::SignedIn, None, 4);
        assert!(view.text.contains("Надішли /login"), "{}", view.text);
        panel.chat_session = Some(crate::login::ChatSession {
            profile_id: "p".into(),
            pub_dress: Some("0x0sky".into()),
            active: false,
        });
        let (view, _) = render_panel(&panel, Screen::SignedIn, None, 4);
        assert!(view.text.contains("0x0sky"));
        assert!(view.text.contains("Активний інший твій клієнт"));
        panel.chat_session.as_mut().unwrap().active = true;
        let (view, _) = render_panel(&panel, Screen::SignedIn, None, 4);
        assert!(view.text.contains("Цей чат — активний клієнт 0x1."));
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
        telegram_api_with(expected, false)
    }

    /// `gone_panel`: every editMessageText fails as Telegram does once the
    /// panel message was deleted.
    fn telegram_api_with(
        expected: Vec<&'static str>,
        gone_panel: bool,
    ) -> (Bot, thread::JoinHandle<Vec<(String, Value)>>) {
        let (bot, _, handle) = telegram_and_core_api(expected, gone_panel);
        (bot, handle)
    }

    fn telegram_and_core_api(
        expected: Vec<&'static str>,
        gone_panel: bool,
    ) -> (Bot, url::Url, thread::JoinHandle<Vec<(String, Value)>>) {
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
                let signed = headers
                    .to_ascii_lowercase()
                    .contains("x-0x1-telegram-bot-signature:");
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

                if matches!(method, "start" | "confirm" | "profile") {
                    assert!(signed);
                    if method == "profile" {
                        assert_eq!(request_path, "/api/v1/telegram/profile");
                    } else {
                        assert_eq!(request_path, format!("/api/v1/telegram/login/{method}"));
                    }
                    let response = if method == "start" {
                        let mut url = url::Url::parse("https://auth.aiaiaiai.org/0x1").unwrap();
                        url.query_pairs_mut()
                            .append_pair("response_type", "code")
                            .append_pair("client_id", "0x1")
                            .append_pair(
                                "redirect_uri",
                                "https://nilx.one/api/v1/telegram/login/callback",
                            )
                            .append_pair("scope", "openid pub_dress telegram_binding")
                            .append_pair("state", "unpredictable-random-state")
                            .append_pair("nonce", "unpredictable-random-nonce")
                            .append_pair(
                                "code_challenge",
                                "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN_12",
                            )
                            .append_pair("code_challenge_method", "S256");
                        json!({"authorization_url": url.as_str(), "expires_in_seconds": 300})
                    } else if method == "profile" {
                        match requests.last().unwrap().1["section"].as_str() {
                            Some("wallet") => json!({"seeds": 51}),
                            Some("avaia") => json!({"avaia": {
                                "pub_dress": "0x0skyai",
                                "configured": true,
                                "location": {"latitude_e7": 504500000, "longitude_e7": 305200000}
                            }}),
                            _ => json!({
                                "pub_dress": "0x0sky",
                                "experience": {"authority": "client", "bond_xp": 3, "avaia_xp": 5}
                            }),
                        }
                    } else {
                        json!({"profile_id": "p", "pub_dress": "0x0sky", "active": false})
                    }
                    .to_string();
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response.len(),
                        response
                    )
                    .unwrap();
                    continue;
                }

                if gone_panel && method == "editMessageText" {
                    let response = json!({
                        "ok": false,
                        "error_code": 400,
                        "description": "Bad Request: message to edit not found"
                    })
                    .to_string();
                    write!(
                        stream,
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response.len(),
                        response
                    )
                    .unwrap();
                    continue;
                }
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
        let start_url =
            url::Url::parse(&format!("http://{address}/api/v1/telegram/login/start")).unwrap();
        (bot, start_url, handle)
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
    async fn start_opens_ais_and_claims_without_a_press_on_new_and_existing_panels() {
        for existing in [false, true] {
            let expected = if existing {
                vec![
                    "start",
                    "sendMessage",
                    "deleteMessage",
                    "confirm",
                    "editMessageText",
                ]
            } else {
                vec![
                    "sendMessage",
                    "start",
                    "editMessageText",
                    "confirm",
                    "editMessageText",
                ]
            };
            let (bot, start_url, server) = telegram_and_core_api(expected, false);
            let gateway = LoginGateway::for_test(start_url);
            let (state, path) = temp_ui();
            if existing {
                state.lock().await.panels.insert(42, Panel::new(42, 10));
            }

            handle_message(bot, incoming(11, "/start"), state.clone(), Some(gateway))
                .await
                .unwrap();
            {
                let store = state.lock().await;
                let panel = store.panels.get(&42).unwrap();
                assert_eq!(panel.screen, Screen::Login);
                assert_eq!(panel.login_message_id, Some(11));
                assert!(panel.chat_session.is_none());
            }
            tokio::time::timeout(Duration::from_secs(12), async {
                loop {
                    if state.lock().await.panels.get(&42).unwrap().screen == Screen::SignedIn {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the bot must claim the Core result without a button press");

            let requests = server.join().unwrap();
            for (method, body) in &requests {
                if matches!(method.as_str(), "start" | "confirm") {
                    assert_eq!(body["provider_actor_id"], 42);
                    assert_eq!(body["chat_id"], 42);
                    assert_eq!(body["message_id"], 11);
                }
            }
            let login_panel = &requests[if existing { 1 } else { 2 }].1;
            let keyboard = login_panel["reply_markup"].to_string();
            assert!(keyboard.contains("web_app"));
            assert!(keyboard.contains("https://auth.aiaiaiai.org/0x1?"));
            assert!(!keyboard.contains("confirm_login"));
            if existing {
                assert_eq!(requests[2].1["message_id"], 10);
            }
            let result = &requests.last().unwrap().1;
            assert_eq!(result["message_id"], 100);
            assert!(
                result["text"]
                    .as_str()
                    .unwrap()
                    .contains("Активний інший твій клієнт")
            );
            let store = state.lock().await;
            let panel = store.panels.get(&42).unwrap();
            assert!(panel.login_message_id.is_none());
            assert!(!panel.chat_session.as_ref().unwrap().active);
            drop(store);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[tokio::test]
    async fn start_on_a_signed_in_chat_shows_the_session_instead_of_a_new_login() {
        // No "start" or "confirm" request may reach Core.
        let (bot, start_url, server) =
            telegram_and_core_api(vec!["sendMessage", "deleteMessage"], false);
        let gateway = LoginGateway::for_test(start_url);
        let (state, path) = temp_ui();
        let mut panel = Panel::new(42, 10);
        panel.chat_session = Some(crate::login::ChatSession {
            profile_id: "p".into(),
            pub_dress: Some("0x0sky".into()),
            active: true,
        });
        state.lock().await.panels.insert(42, panel);

        handle_message(bot, incoming(11, "/start"), state.clone(), Some(gateway))
            .await
            .unwrap();
        let requests = server.join().unwrap();
        assert!(
            requests[0].1["text"]
                .as_str()
                .unwrap()
                .contains("Цей чат — активний клієнт 0x1."),
            "{requests:?}"
        );
        let store = state.lock().await;
        let panel = store.panels.get(&42).unwrap();
        assert_eq!(panel.screen, Screen::SignedIn);
        assert!(
            panel.login_message_id.is_none(),
            "no login wait was started"
        );
        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn an_unavailable_login_host_keeps_start_on_home_and_says_so_on_login() {
        // Nothing listens on port 9: every login start fails to connect.
        let unreachable =
            url::Url::parse("http://127.0.0.1:9/api/v1/telegram/login/start").unwrap();
        for (command, expected) in [
            ("/start", "Сесія Bond не підтверджена"),
            ("/login", "AIS-авторизація недоступна"),
        ] {
            let (bot, server) = telegram_api(vec!["sendMessage", "deleteMessage"]);
            let (state, path) = temp_ui();
            state.lock().await.panels.insert(42, Panel::new(42, 10));
            handle_message(
                bot,
                incoming(11, command),
                state.clone(),
                Some(LoginGateway::for_test(unreachable.clone())),
            )
            .await
            .unwrap();
            let requests = server.join().unwrap();
            let text = requests[0].1["text"].as_str().unwrap();
            assert!(text.contains(expected), "{command}: {text}");
            let store = state.lock().await;
            let panel = store.panels.get(&42).unwrap();
            assert!(
                panel.login_message_id.is_none(),
                "{command}: no wait without a login"
            );
            drop(store);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[tokio::test]
    async fn a_typed_command_answers_under_it_and_removes_the_old_panel() {
        // After "clear history for me" the old panel still exists for the
        // bot: editing it succeeds and the owner sees nothing.
        let (bot, server) = telegram_api(vec!["sendMessage", "deleteMessage"]);
        let (state, path) = temp_ui();
        state.lock().await.panels.insert(42, Panel::new(42, 10));
        handle_message(bot, incoming(11, "/help"), state.clone(), None)
            .await
            .unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests[0].0, "sendMessage");
        assert!(requests[0].1["text"].as_str().unwrap().contains("Допомога"));
        assert_eq!(requests[1].0, "deleteMessage");
        assert_eq!(requests[1].1["message_id"], 10);
        let store = state.lock().await;
        let panel = store.panels.get(&42).unwrap();
        assert_eq!(panel.message_id, 100, "later edits target the new panel");
        assert_eq!(panel.screen, Screen::Help);
        assert!(!panel.resend);
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn a_button_on_a_deleted_panel_sends_it_again_instead_of_going_silent() {
        let (bot, server) = telegram_api_with(
            vec!["answerCallbackQuery", "editMessageText", "sendMessage"],
            true,
        );
        let (state, path) = temp_ui();
        state.lock().await.panels.insert(42, Panel::new(42, 10));
        handle_callback(bot, callback(10, "ui:0:help"), state.clone(), None)
            .await
            .unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests[1].1["message_id"], 10);
        assert_eq!(requests[2].0, "sendMessage");
        let store = state.lock().await;
        let panel = store.panels.get(&42).unwrap();
        assert_eq!(panel.message_id, 100);
        assert_eq!(panel.screen, Screen::Help);
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn open_renders_native_webview_button_without_session_data() {
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
                .contains("Перенесення активної гри потребує підтвердження")
        );
        assert_eq!(
            requests[1].1["reply_markup"]["inline_keyboard"][0][0],
            json!({
                "text": "Відкрити Mini App",
                "web_app": {"url": "https://nilx.one/telegram/"}
            })
        );
        let store = state.lock().await;
        assert_eq!(store.panels.get(&42).unwrap().screen, Screen::MiniApp);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn real_message_handler_reuses_one_panel_and_never_fakes_core_data() {
        let (bot, server) = telegram_api(vec![
            "sendMessage",
            "sendMessage",
            "deleteMessage",
            "sendMessage",
            "deleteMessage",
            "sendMessage",
            "deleteMessage",
        ]);
        let (state, path) = temp_ui();
        for (message_id, text) in [(10, "/start"), (11, "/SUBSCRIPTION"), (12, "/travel")] {
            handle_message(bot.clone(), incoming(message_id, text), state.clone(), None)
                .await
                .unwrap();
        }
        // The fourth API call is an actual return to Home, not another send.
        handle_message(bot, incoming(13, "/start"), state.clone(), None)
            .await
            .unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 7);
        // One panel at a time: each typed command sends it under the command
        // and deletes the previous one.
        let panels: Vec<_> = requests
            .iter()
            .filter(|(method, _)| method == "sendMessage")
            .collect();
        for (method, _) in requests.iter().skip(1).step_by(2) {
            assert_eq!(method, "sendMessage");
        }
        for (method, body) in requests.iter().skip(2).step_by(2) {
            assert_eq!(method, "deleteMessage");
            assert_eq!(body["message_id"], 100);
        }
        let requests: Vec<_> = panels.into_iter().cloned().collect();
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
    async fn bond_panel_reads_current_profile_and_ignores_a_stale_button() {
        let (bot, start_url, server) = telegram_and_core_api(
            vec![
                "answerCallbackQuery",
                "profile",
                "editMessageText",
                "answerCallbackQuery",
            ],
            false,
        );
        let host = LoginGateway::for_test(start_url);
        let (state, path) = temp_ui();
        state.lock().await.panels.insert(42, Panel::new(42, 100));

        for _ in 0..2 {
            handle_callback(
                bot.clone(),
                callback(100, "ui:0:bond"),
                state.clone(),
                Some(host.clone()),
            )
            .await
            .unwrap();
        }

        let requests = server.join().unwrap();
        assert_eq!(
            requests[1].1,
            json!({"version": 1, "provider": "telegram", "provider_actor_id": 42, "chat_id": 42})
        );
        let text = requests[2].1["text"].as_str().unwrap();
        assert!(text.contains("0x0sky"));
        assert!(text.contains("Bond · 3 XP"));
        assert!(text.contains("Avaia · 5 XP"));
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn avaia_and_inventory_panels_show_the_core_state() {
        let (bot, start_url, server) = telegram_and_core_api(
            vec![
                "answerCallbackQuery",
                "profile",
                "editMessageText",
                "answerCallbackQuery",
                "profile",
                "editMessageText",
            ],
            false,
        );
        let host = LoginGateway::for_test(start_url);
        let (state, path) = temp_ui();
        state.lock().await.panels.insert(42, Panel::new(42, 100));
        for target in ["ui:0:avaia", "ui:1:inventory"] {
            handle_callback(
                bot.clone(),
                callback(100, target),
                state.clone(),
                Some(host.clone()),
            )
            .await
            .unwrap();
        }
        let requests = server.join().unwrap();
        assert_eq!(requests[1].1["section"], "avaia");
        let avaia = requests[2].1["text"].as_str().unwrap();
        assert!(avaia.contains("0x0skyai"));
        assert!(avaia.contains("50.45000, 30.52000"));
        assert_eq!(requests[4].1["section"], "wallet");
        assert!(requests[5].1["text"].as_str().unwrap().contains("51 Seeds"));
        std::fs::remove_file(path).unwrap();
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

    #[tokio::test]
    async fn confirm_without_a_started_login_never_claims_a_session() {
        let (bot, server) = telegram_api(vec!["answerCallbackQuery", "editMessageText"]);
        let (state, path) = temp_ui();
        {
            let mut store = state.lock().await;
            store.panels.insert(42, Panel::new(42, 100));
        }
        handle_callback(
            bot,
            callback(100, "ui:0:confirm_login"),
            state.clone(),
            None,
        )
        .await
        .unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests[1].0, "editMessageText");
        let text = requests[1].1["text"].as_str().unwrap();
        assert!(text.contains("AIS-авторизація недоступна"), "{text}");
        let store = state.lock().await;
        let panel = store.panels.get(&42).unwrap();
        assert_eq!(panel.screen, Screen::Login);
        assert!(panel.chat_session.is_none());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
}
