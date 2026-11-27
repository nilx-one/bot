// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! Transport-only admin inbox adapter for nilx-one/core#186.
//! The signed Core gateway owns recipients, role decisions and delivery leases.

use std::{collections::HashMap, fs, io, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use teloxide::{
    ApiError, RequestError,
    prelude::*,
    types::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, MessageId},
};

use crate::login::{LoginError, LoginGateway};

// Strict typed mirrors of nilxone_api::bot_admin, until that Core PR is
// merged and the shared wire crate can be pinned in this repository.
const PULL_PATH: &str = "/api/v1/telegram/admin/delivery/pull";
const ACK_PATH: &str = "/api/v1/telegram/admin/delivery/ack";
const DECIDE_PATH: &str = "/api/v1/telegram/admin/decide";
const MAX_REPLY_BYTES: usize = 32 * 1024;
const WIRE_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Pending,
    Approved,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Lease {
    kind: Kind,
    candidate_pub_dress: String,
    provider_actor_id: u64,
    claimed_at: u64,
}

#[derive(Debug, Deserialize)]
struct Entry {
    request_token: String,
    pub_dress: String,
}

#[derive(Debug, Deserialize)]
struct Delivery {
    lease: Lease,
    admin_message_id: Option<i32>,
    pending: Vec<Entry>,
}

#[derive(Serialize)]
struct Pull {
    version: u8,
    limit: usize,
}

#[derive(Deserialize)]
struct PullResponse {
    deliveries: Vec<Delivery>,
}

#[derive(Serialize)]
struct Ack<'a> {
    version: u8,
    lease: &'a Lease,
    message_id: i32,
}

#[derive(Deserialize)]
struct AckResponse {
    acknowledged: bool,
}

#[derive(Serialize)]
struct Chat {
    version: u8,
    provider: &'static str,
    provider_actor_id: u64,
    chat_id: i64,
}

#[derive(Serialize)]
struct Decision<'a> {
    chat: Chat,
    source_message_id: i32,
    request_token: &'a str,
    approve: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Approved,
    Denied,
    AlreadyDecided,
    Forbidden,
}

#[derive(Deserialize)]
struct DecisionResponse {
    outcome: Outcome,
    pending: Vec<Entry>,
}

async fn call<T: DeserializeOwned>(
    gateway: &LoginGateway,
    path: &str,
    request: &impl Serialize,
) -> Result<T, LoginError> {
    let mut response = gateway.post_signed(path, request).await?;
    if !response.status().is_success() {
        return Err(LoginError::Unavailable);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| LoginError::Unavailable)?
    {
        if chunk.len() > MAX_REPLY_BYTES - body.len() {
            return Err(LoginError::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| LoginError::InvalidResponse)
}

fn valid_token(token: &str) -> bool {
    token.len() == 32
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn parse_callback(data: &str) -> Option<(bool, &str)> {
    let (decision, token) = data.strip_prefix("ar:")?.split_once(':')?;
    if !valid_token(token) {
        return None;
    }
    match decision {
        "y" => Some((true, token)),
        "n" => Some((false, token)),
        _ => None,
    }
}

fn safe_address(address: &str) -> bool {
    !address.is_empty() && address.len() <= 128 && !address.chars().any(char::is_control)
}

fn render(pending: &[Entry], recent: Option<&str>) -> Option<(String, InlineKeyboardMarkup)> {
    if pending.len() > 10 {
        return None;
    }
    let mut text = String::from("🔐 0x1 · Admin");
    if let Some(recent) = recent {
        text.push('\n');
        text.push_str(recent);
    }
    let mut buttons = Vec::new();
    if pending.is_empty() {
        text.push_str("\nНемає запитів, що очікують рішення.");
    } else {
        text.push_str("\nОчікують рішення:");
        for entry in pending {
            if !safe_address(&entry.pub_dress) || !valid_token(&entry.request_token) {
                return None;
            }
            text.push_str(&format!("\n• {}", entry.pub_dress));
            buttons.push(vec![
                InlineKeyboardButton::callback(
                    format!("✅ {}", entry.pub_dress),
                    format!("ar:y:{}", entry.request_token),
                ),
                InlineKeyboardButton::callback("❌", format!("ar:n:{}", entry.request_token)),
            ]);
        }
    }
    Some((text, InlineKeyboardMarkup::new(buttons)))
}

/// Transport metadata only. Persist the ID of a replacement sent before Core
/// acknowledged it; the next lease can edit it after an ACK failure/restart.
/// A changed authoritative Core slot always wins.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Replacement {
    previous: Option<i32>,
    sent: i32,
}

struct SlotCache {
    path: PathBuf,
    replacements: HashMap<i64, Replacement>,
}

impl SlotCache {
    fn open(path: PathBuf) -> io::Result<Self> {
        let replacements = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => return Err(error),
        };
        Ok(Self { path, replacements })
    }

    fn slot(&self, chat: i64, core: Option<i32>) -> Option<i32> {
        self.replacements
            .get(&chat)
            .filter(|saved| saved.previous == core && saved.sent > 0)
            .map(|saved| saved.sent)
            .or(core)
    }

    fn save(&self) -> io::Result<()> {
        let tmp = self.path.with_extension("tmp");
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            &tmp,
            serde_json::to_vec(&self.replacements).map_err(io::Error::other)?,
        )?;
        fs::rename(tmp, &self.path)
    }
}

async fn deliver(bot: &Bot, cache: &mut SlotCache, gateway: &LoginGateway, job: Delivery) {
    let Ok(actor) = i64::try_from(job.lease.provider_actor_id) else {
        return;
    };
    if actor <= 0
        || job.lease.claimed_at == 0
        || !safe_address(&job.lease.candidate_pub_dress)
        || job.admin_message_id.is_some_and(|id| id <= 0)
    {
        return;
    }
    let notice = match job.lease.kind {
        Kind::Pending => None,
        Kind::Approved => Some(format!(
            "✅ 0x1 · Користувач {} приєднався до ролі admin.",
            job.lease.candidate_pub_dress
        )),
    };
    let Some((text, markup)) = render(&job.pending, notice.as_deref()) else {
        tracing::warn!("invalid Core admin inbox projection");
        return;
    };
    let chat = ChatId(actor);
    let current = cache.slot(actor, job.admin_message_id);
    let edit = match current {
        Some(id) => bot
            .edit_message_text(chat, MessageId(id), text.clone())
            .reply_markup(markup.clone())
            .await
            .map(|_| ()),
        None => Err(RequestError::Api(ApiError::MessageToEditNotFound)),
    };
    let message_id = match edit {
        Ok(()) | Err(RequestError::Api(ApiError::MessageNotModified)) => {
            let Some(id) = current else { return };
            id
        }
        Err(RequestError::Api(
            ApiError::MessageToEditNotFound
            | ApiError::MessageCantBeEdited
            | ApiError::MessageIdInvalid,
        )) => match bot.send_message(chat, text).reply_markup(markup).await {
            Ok(sent) => {
                cache.replacements.insert(
                    actor,
                    Replacement {
                        previous: job.admin_message_id,
                        sent: sent.id.0,
                    },
                );
                if cache.save().is_err() {
                    tracing::warn!("admin replacement ID could not be persisted locally");
                }
                sent.id.0
            }
            Err(_) => {
                tracing::warn!("admin inbox Telegram send failed; lease retained");
                return;
            }
        },
        Err(_) => {
            tracing::warn!("admin inbox Telegram edit failed; lease retained");
            return;
        }
    };
    let ack = Ack {
        version: WIRE_VERSION,
        lease: &job.lease,
        message_id,
    };
    match call::<AckResponse>(gateway, ACK_PATH, &ack).await {
        Ok(response) if response.acknowledged => {
            if cache.replacements.remove(&actor).is_some() && cache.save().is_err() {
                tracing::warn!("admin replacement cache cleanup failed");
            }
        }
        _ => tracing::warn!("admin inbox receipt not acknowledged; Core retains the lease"),
    }
}

pub(super) async fn run(bot: Bot, gateway: LoginGateway, path: PathBuf) {
    let mut cache = match SlotCache::open(path) {
        Ok(cache) => cache,
        Err(_) => {
            tracing::error!("admin slot cache unavailable; admin transport not started");
            return;
        }
    };
    let mut ticker = tokio::time::interval(Duration::from_secs(5));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        // Serialize panel edits and callback decisions. Do not lock the
        // normal Bot UI or block its polling / login.
        let _guard = gateway.admin_lock.lock().await;
        let result = call::<PullResponse>(
            &gateway,
            PULL_PATH,
            &Pull {
                version: WIRE_VERSION,
                limit: 1,
            },
        )
        .await;
        match result {
            Ok(response) if response.deliveries.len() <= 1 => {
                for delivery in response.deliveries {
                    deliver(&bot, &mut cache, &gateway, delivery).await;
                }
            }
            Ok(_) => tracing::warn!("Core returned an oversized admin delivery batch"),
            Err(_) => tracing::debug!("admin inbox gateway unavailable"),
        }
    }
}

pub(super) async fn handle_callback(
    bot: &Bot,
    query: &CallbackQuery,
    gateway: Option<&LoginGateway>,
) -> ResponseResult<()> {
    let Some(message) = query.message.as_ref().and_then(|m| m.regular_message()) else {
        bot.answer_callback_query(query.id.clone()).await?;
        return Ok(());
    };
    let parsed = query.data.as_deref().and_then(parse_callback);
    let actor = query.from.id.0;
    let chat = message.chat.id;
    let (Some((approve, token)), Some(gateway)) = (parsed, gateway) else {
        bot.answer_callback_query(query.id.clone())
            .text("Запит недоступний.")
            .await?;
        return Ok(());
    };
    if !message.chat.is_private()
        || query.from.is_bot
        || chat.0 <= 0
        || i64::try_from(actor).ok() != Some(chat.0)
        || message.id.0 <= 0
    {
        bot.answer_callback_query(query.id.clone()).await?;
        return Ok(());
    }
    let _guard = gateway.admin_lock.lock().await;
    let request = Decision {
        chat: Chat {
            version: WIRE_VERSION,
            provider: "telegram",
            provider_actor_id: actor,
            chat_id: chat.0,
        },
        source_message_id: message.id.0,
        request_token: token,
        approve,
    };
    let response = call::<DecisionResponse>(gateway, DECIDE_PATH, &request).await;
    let (answer, projection) = match response {
        Ok(response) => {
            let answer = match response.outcome {
                Outcome::Approved => "✅ Роль admin надано.",
                Outcome::Denied => "❌ Запит відхилено.",
                Outcome::AlreadyDecided => "Цей запит уже розглянуто.",
                Outcome::Forbidden => "Потрібна активна admin-сесія.",
            };
            let projection = (response.outcome != Outcome::Forbidden)
                .then(|| render(&response.pending, Some(answer)))
                .flatten();
            (answer, projection)
        }
        Err(_) => ("Сервіс тимчасово недоступний.", None),
    };
    bot.answer_callback_query(query.id.clone())
        .text(answer)
        .show_alert(true)
        .await?;
    if let Some((text, markup)) = projection {
        match bot
            .edit_message_text(chat, message.id, text)
            .reply_markup(markup)
            .await
        {
            Ok(_) | Err(RequestError::Api(ApiError::MessageNotModified)) => {}
            // Do not invent a replacement outside a Core delivery lease.
            Err(_) => tracing::warn!("admin inbox refresh after decision failed"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_keeps_the_legacy_opaque_reference_and_is_strict() {
        let token = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            parse_callback(&format!("ar:y:{token}")),
            Some((true, token))
        );
        assert_eq!(
            parse_callback(&format!("ar:n:{token}")),
            Some((false, token))
        );
        for invalid in [
            "ar:y:bad",
            "ar:x:0123456789abcdef0123456789abcdef",
            "ar:y:0123456789abcdef0123456789abcdef:extra",
        ] {
            assert!(parse_callback(invalid).is_none());
        }
        let (text, buttons) = render(
            &[Entry {
                request_token: token.into(),
                pub_dress: "0x0new".into(),
            }],
            None,
        )
        .unwrap();
        assert!(text.contains("0x0new"));
        assert!(
            serde_json::to_string(&buttons)
                .unwrap()
                .contains(&format!("ar:y:{token}"))
        );
        assert!(
            render(
                &[Entry {
                    request_token: "invalid".into(),
                    pub_dress: "0x0new".into()
                }],
                None
            )
            .is_none()
        );
    }

    #[test]
    fn replacement_is_used_only_until_core_reports_a_different_slot() {
        let cache = SlotCache {
            path: PathBuf::from("unused"),
            replacements: HashMap::from([(
                42,
                Replacement {
                    previous: Some(7),
                    sent: 9,
                },
            )]),
        };
        assert_eq!(cache.slot(42, Some(7)), Some(9));
        assert_eq!(cache.slot(42, Some(10)), Some(10));
        assert_eq!(cache.slot(42, None), None);
        assert_eq!(cache.slot(43, Some(7)), Some(7));
    }

    #[test]
    fn decision_wire_is_the_signed_core_contract() {
        let request = Decision {
            chat: Chat {
                version: 1,
                provider: "telegram",
                provider_actor_id: 42,
                chat_id: 42,
            },
            source_message_id: 9,
            request_token: "0123456789abcdef0123456789abcdef",
            approve: true,
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({
                "chat": {"version":1, "provider":"telegram", "provider_actor_id":42, "chat_id":42},
                "source_message_id":9,
                "request_token":"0123456789abcdef0123456789abcdef",
                "approve":true
            })
        );
    }
}
