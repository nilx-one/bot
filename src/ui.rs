// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! Telegram panel *presentation* state. Never persist AIS credentials,
//! product-session truth, inventory, Bond identity or xPing state here.

use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::login::ChatSession;

const MAX_TRACKED_MESSAGES: usize = 96;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Screen {
    Home,
    Login,
    MiniApp,
    Subscription,
    Bond,
    Avaia,
    Inventory,
    Travel,
    Organizations,
    Help,
    Unavailable,
    /// The sender pressed confirm after the browser step.
    ConfirmLogin,
    /// Core opened this chat's own product session.
    SignedIn,
}

impl Screen {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Login => "login",
            Self::MiniApp => "mini_app",
            Self::Subscription => "subscription",
            Self::Bond => "bond",
            Self::Avaia => "avaia",
            Self::Inventory => "inventory",
            Self::Travel => "travel",
            Self::Organizations => "organizations",
            Self::Help => "help",
            Self::Unavailable => "unavailable",
            Self::ConfirmLogin => "confirm_login",
            Self::SignedIn => "signed_in",
        }
    }

    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "home" => Some(Self::Home),
            "login" => Some(Self::Login),
            "mini_app" => Some(Self::MiniApp),
            "subscription" => Some(Self::Subscription),
            "bond" => Some(Self::Bond),
            "avaia" => Some(Self::Avaia),
            "inventory" => Some(Self::Inventory),
            "travel" => Some(Self::Travel),
            "organizations" => Some(Self::Organizations),
            "help" => Some(Self::Help),
            "confirm_login" => Some(Self::ConfirmLogin),
            "signed_in" => Some(Self::SignedIn),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Button {
    Navigate(&'static str, Screen),
    OpenUrl(&'static str, Url),
    /// The AIS sign-in page, opened inside Telegram as a Mini App so the
    /// page carries Telegram's proof of the sender. Not the game Mini App.
    OpenLoginPage(&'static str, Url),
    OpenMiniApp,
}

#[derive(Debug, Clone)]
pub struct View {
    pub text: String,
    pub rows: Vec<Vec<Button>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Panel {
    pub owner_id: u64,
    pub message_id: i32,
    pub revision: u64,
    pub screen: Screen,
    #[serde(default)]
    pub tracked_messages: Vec<i32>,
    /// Authorization URL is ephemeral and MUST NOT be persisted.
    #[serde(skip)]
    pub login_url: Option<Url>,
    /// The message login start was bound to; confirm names it to Core.
    /// Ephemeral: after a restart the owner sends /login again.
    #[serde(skip)]
    pub login_message_id: Option<i32>,
    /// Core's display projection of this chat's session. Not persisted and
    /// not authority: Core decides again on every request.
    #[serde(skip)]
    pub chat_session: Option<ChatSession>,
}

impl Panel {
    #[must_use]
    pub fn new(owner_id: u64, message_id: i32) -> Self {
        Self {
            owner_id,
            message_id,
            revision: 0,
            screen: Screen::Home,
            tracked_messages: Vec::new(),
            login_url: None,
            login_message_id: None,
            chat_session: None,
        }
    }

    pub fn track(&mut self, message_id: i32) {
        if message_id != self.message_id && !self.tracked_messages.contains(&message_id) {
            self.tracked_messages.push(message_id);
            if self.tracked_messages.len() > MAX_TRACKED_MESSAGES {
                self.tracked_messages.remove(0);
            }
        }
    }

    /// Only a callback originating from the current panel and UI revision
    /// can select a screen. No callback can authenticate or mutate Core.
    #[must_use]
    pub fn callback_target(
        &self,
        requester_id: u64,
        message_id: i32,
        data: &str,
    ) -> Option<Screen> {
        if requester_id != self.owner_id || message_id != self.message_id {
            return None;
        }
        let mut fields = data.split(':');
        if fields.next()? != "ui" {
            return None;
        }
        let rev = fields.next()?.parse::<u64>().ok()?;
        let destination = Screen::from_key(fields.next()?)?;
        (fields.next().is_none() && rev == self.revision).then_some(destination)
    }

    /// The caller updates the local panel only after Telegram accepted the edit.
    pub fn change_screen(&mut self, screen: Screen, login_url: Option<Url>) {
        self.screen = screen;
        self.login_url = login_url;
        self.revision = self.revision.saturating_add(1);
    }

    fn signed_in_text(&self) -> String {
        let Some(session) = &self.chat_session else {
            return "Сесію чату не знайдено. Надішли /login.".to_owned();
        };
        let bond = session
            .pub_dress
            .as_deref()
            .unwrap_or("без публічної адреси");
        let activity = if session.active {
            "Цей чат — активний клієнт 0x1."
        } else {
            "Активний інший твій клієнт (Mini App або браузер). Чат увійшов, але змінювати гру зможе після передачі активності."
        };
        format!("Bond: {bond}\n{activity}")
    }

    /// A *trusted* Core authentication-completion handler may use these IDs
    /// for best-effort cleanup before publishing a new pinned panel.
    /// Old messages, unobserved history and Telegram deletion failures remain.
    #[must_use]
    pub fn cleanup_candidates(&self) -> Vec<i32> {
        let mut ids = self.tracked_messages.clone();
        ids.push(self.message_id);
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    #[must_use]
    pub fn view_for(&self, screen: Screen, login_url: Option<&Url>, revision: u64) -> View {
        let signed_in = self.signed_in_text();
        let (title, description) = match screen {
            Screen::Home => (
                "0x1 · Text",
                "Текстовий клієнт 0x1.\nСесія Bond не підтверджена Core.\nОберіть розділ або авторизуйтеся через aiaiaiai AIS.",
            ),
            Screen::Login => (
                "0x1 · AIS",
                if login_url.is_some() {
                    "Натисніть «Увійти»: вхід через AIS відкриється всередині Telegram і сам закриється. Потім натисніть «Підтвердити вхід»."
                } else {
                    "AIS-авторизація недоступна або потребує нового посилання. Натисніть «Отримати посилання»."
                },
            ),
            Screen::MiniApp => (
                "0x1 · Telegram Mini App",
                "Відкрийте 0x1 у Telegram. За потреби увійдіть у Mini App. Перенесення активної гри потребує підтвердження.",
            ),
            Screen::Subscription => (
                "0x1 · Підписка",
                "Підписка недоступна: немає авторизованої проєкції Core Gateway.\nБот не визначає статус або термін дії підписки самостійно.",
            ),
            Screen::Bond => (
                "0x1 · Bond",
                "Профіль Bond та XP будуть доступні тільки через Core Gateway.",
            ),
            Screen::Avaia => (
                "0x1 · Avaia",
                "Стан, позицію та дії Avaia має підтвердити Core Gateway.",
            ),
            Screen::Inventory => (
                "0x1 · Інвентар",
                "Інвентар недоступний: потрібна авторизована проєкція Core Gateway.\nБот не визначає розміри, вміст або доступні дії.",
            ),
            Screen::Travel => (
                "0x1 · Подорожі",
                "Подорожі недоступні: Core Gateway не надав дозволені цілі, маршрути або стан руху.\nБот не створює маршрутів і не підтверджує прибуття.",
            ),
            Screen::Organizations => (
                "0x1 · Організації",
                "Business Bonds і ролі будуть доступні через авторизований Core Gateway.",
            ),
            Screen::Help => (
                "0x1 · Допомога",
                "/start — головний екран\n/open — відкрити Mini App\n/login — AIS\n/subscription — підписка\nІнша навігація здійснюється кнопками цього повідомлення.",
            ),
            Screen::Unavailable => (
                "0x1 · Недоступно",
                "Потрібні підтверджена AIS-ідентичність, активна Core-сесія і відповідний Gateway. Жодних даних не змінено.",
            ),
            Screen::ConfirmLogin => (
                "0x1 · AIS",
                "Вхід не підтверджено. Спершу заверши вхід у браузері, потім натисни «Підтвердити вхід». Або надішли /login ще раз.",
            ),
            Screen::SignedIn => ("0x1 · Увійшли", signed_in.as_str()),
        };
        let mut rows = Vec::new();
        if matches!(screen, Screen::Login | Screen::ConfirmLogin) {
            if let Some(url) = login_url {
                rows.push(vec![Button::OpenLoginPage("Увійти", url.clone())]);
                rows.push(vec![Button::Navigate(
                    "Підтвердити вхід",
                    Screen::ConfirmLogin,
                )]);
            } else {
                rows.push(vec![Button::Navigate("Отримати посилання", Screen::Login)]);
            }
        }
        if screen == Screen::MiniApp {
            rows.push(vec![Button::OpenMiniApp]);
        }
        if screen != Screen::Home {
            rows.push(vec![Button::Navigate("⌂ Головна", Screen::Home)]);
        }
        rows.push(vec![
            Button::Navigate("Bond", Screen::Bond),
            Button::Navigate("Avaia", Screen::Avaia),
        ]);
        rows.push(vec![
            Button::Navigate("Інвентар", Screen::Inventory),
            Button::Navigate("Подорожі", Screen::Travel),
        ]);
        rows.push(vec![
            Button::Navigate("Підписка", Screen::Subscription),
            Button::Navigate("Організації", Screen::Organizations),
        ]);
        rows.push(vec![
            Button::Navigate("AIS", Screen::Login),
            Button::Navigate("Допомога", Screen::Help),
        ]);
        if screen != Screen::MiniApp {
            rows.push(vec![Button::Navigate("Mini App", Screen::MiniApp)]);
        }
        let _ = revision;
        View {
            text: format!("{title}\n\n{description}"),
            rows,
        }
    }
}

#[derive(Debug)]
pub struct UiStore {
    path: PathBuf,
    pub panels: HashMap<i64, Panel>,
}

impl UiStore {
    pub fn open(path: PathBuf) -> io::Result<Self> {
        let panels = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e),
        };
        Ok(Self { path, panels })
    }

    pub fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("tmp");
        let serialized = serde_json::to_vec(&self.panels).map_err(io::Error::other)?;
        fs::write(&tmp, serialized)?;
        fs::rename(tmp, &self.path)?;
        Ok(())
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_requires_owner_message_and_current_revision() {
        let mut panel = Panel::new(42, 10);
        assert_eq!(
            panel.callback_target(42, 10, "ui:0:inventory"),
            Some(Screen::Inventory)
        );
        assert_eq!(panel.callback_target(43, 10, "ui:0:inventory"), None);
        assert_eq!(panel.callback_target(42, 11, "ui:0:inventory"), None);
        assert_eq!(panel.callback_target(42, 10, "ui:0:bad"), None);
        assert_eq!(panel.callback_target(42, 10, "ui:0:inventory:extra"), None);
        panel.change_screen(Screen::Inventory, None);
        assert_eq!(panel.callback_target(42, 10, "ui:0:inventory"), None);
        assert_eq!(
            panel.callback_target(42, 10, "ui:1:subscription"),
            Some(Screen::Subscription)
        );
    }

    #[test]
    fn render_is_clearly_unverified_and_does_not_invent_a_subscription() {
        let panel = Panel::new(42, 10);
        let view = panel.view_for(Screen::Subscription, None, 1);
        assert!(view.text.contains("Core"));
        assert!(!view.text.contains("0x0sky"));
        assert!(view.rows.iter().any(|r| {
            r.iter()
                .any(|b| matches!(b, Button::Navigate("Інвентар", Screen::Inventory)))
        }));
    }

    #[test]
    fn cleanup_is_bounded_and_never_claims_full_history() {
        let mut panel = Panel::new(42, 10);
        for id in 1..200 {
            panel.track(id);
        }
        assert_eq!(panel.tracked_messages.len(), MAX_TRACKED_MESSAGES);
        let candidates = panel.cleanup_candidates();
        assert!(candidates.contains(&10));
        assert!(candidates.contains(&199));
        assert!(!candidates.contains(&1));
    }

    #[test]
    fn persist_only_view_metadata_not_oidc_links() {
        let dir = std::env::temp_dir().join(format!("0x1-ui-test-{}", std::process::id()));
        let path = dir.join("ui.json");
        let mut store = UiStore::open(path.clone()).unwrap();
        let mut panel = Panel::new(42, 10);
        panel.change_screen(
            Screen::Login,
            Some(Url::parse("https://auth.aiaiaiai.org/0x1?state=secret").unwrap()),
        );
        store.panels.insert(42, panel);
        store.save().unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(!content.contains("secret"));
        let restored = UiStore::open(path.clone()).unwrap();
        assert_eq!(restored.panels.get(&42).unwrap().screen, Screen::Login);
        assert!(restored.panels.get(&42).unwrap().login_url.is_none());
        fs::remove_file(path).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
