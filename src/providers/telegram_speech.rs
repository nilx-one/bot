// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! Short-lived speech transport. Core selects recipients, owns leases, and
//! authorizes the exact chat session. A Telegram receipt is not BondChain.

use std::{
    collections::HashMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use nilxone_contracts::{PubDress, SpokenText};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use teloxide::prelude::*;

use crate::login::{LoginError, LoginGateway};

const PULL_PATH: &str = "/api/v1/telegram/speech/pull";
const ACK_PATH: &str = "/api/v1/telegram/speech/ack";
const LEASE_SECONDS: u64 = 60;
const TTL_SECONDS: u64 = 600;
const MAX_REPLY_BYTES: usize = 8 * 1024;

#[derive(Serialize)]
struct Pull {
    version: u8,
    limit: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Delivery {
    id: u64,
    provider_actor_id: u64,
    speaker: String,
    text: String,
    expires_at: u64,
    claimed_at: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PullResponse {
    deliveries: Vec<Delivery>,
}

#[derive(Serialize)]
struct Ack {
    version: u8,
    id: u64,
    claimed_at: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AckResponse {
    acknowledged: bool,
}

fn now() -> Option<u64> {
    Some(SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs())
}

impl Delivery {
    fn presentation(&self, now: u64) -> Option<(ChatId, String)> {
        if self.id == 0
            || self.claimed_at > now
            || now.saturating_sub(self.claimed_at) >= LEASE_SECONDS
            || self.expires_at <= now
            || self.expires_at > now.saturating_add(TTL_SECONDS + 60)
        {
            return None;
        }
        let actor = i64::try_from(self.provider_actor_id)
            .ok()
            .filter(|id| *id > 0)?;
        let speaker: PubDress = self.speaker.parse().ok()?;
        let text: SpokenText = self.text.parse().ok()?;
        Some((ChatId(actor), format!("{speaker}: {text}")))
    }
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

/// A single worker claims one delivery at a time. Retain successful send IDs
/// until TTL to avoid re-sending after an ACK failure in this process.
/// A crash between Telegram success and ACK still permits at-least-once
/// delivery. No content, credentials or recipient IDs are written to logs.
pub async fn run(bot: Bot, gateway: LoginGateway) {
    let mut sent = HashMap::<u64, u64>::new();
    let mut delay = Duration::from_secs(5);
    loop {
        tokio::time::sleep(delay).await;
        delay = Duration::from_secs(5);
        let Some(current) = now() else {
            continue;
        };
        sent.retain(|_, expires| *expires > current);
        let pulled: PullResponse = match call(
            &gateway,
            PULL_PATH,
            &Pull {
                version: 1,
                limit: 1,
            },
        )
        .await
        {
            Ok(response) => response,
            Err(_) => {
                tracing::warn!("Telegram speech pull unavailable");
                delay = Duration::from_secs(30);
                continue;
            }
        };
        if pulled.deliveries.len() > 1 {
            tracing::warn!("Telegram speech pull exceeded requested limit");
            continue;
        }
        for delivery in pulled.deliveries {
            let Some(current) = now() else { continue };
            let Some((chat, text)) = delivery.presentation(current) else {
                tracing::warn!("Telegram speech delivery refused invalid or expired lease");
                continue;
            };
            if let std::collections::hash_map::Entry::Vacant(entry) = sent.entry(delivery.id) {
                let remaining = delivery
                    .expires_at
                    .saturating_sub(current)
                    .min(LEASE_SECONDS.saturating_sub(current.saturating_sub(delivery.claimed_at)));
                let timeout = Duration::from_secs(remaining.min(10));
                // Plain text; no parse mode and no access to Bond state.
                if !matches!(
                    tokio::time::timeout(timeout, bot.send_message(chat, text).send()).await,
                    Ok(Ok(_))
                ) {
                    tracing::warn!("Telegram speech send did not confirm");
                    continue;
                }
                entry.insert(delivery.expires_at);
            }
            let ack = Ack {
                version: 1,
                id: delivery.id,
                claimed_at: delivery.claimed_at,
            };
            match call::<AckResponse>(&gateway, ACK_PATH, &ack).await {
                Ok(response) if response.acknowledged => {}
                _ => tracing::warn!("Telegram speech receipt not acknowledged"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_wire_and_plain_text_presentation() {
        let json = r#"{"id":9,"provider_actor_id":42,"speaker":"0x1speaker","text":"Привіт! 🐈","expires_at":1600,"claimed_at":1000}"#;
        let mut delivery: Delivery = serde_json::from_str(json).unwrap();
        assert_eq!(
            delivery.presentation(1001),
            Some((ChatId(42), "0x1speaker: Привіт! 🐈".into()))
        );
        assert_eq!(delivery.presentation(1060), None);
        assert_eq!(delivery.presentation(999), None);
        assert_eq!(delivery.presentation(1600), None);
        delivery.provider_actor_id = 0;
        assert_eq!(delivery.presentation(1001), None);
        let ack = Ack {
            version: 1,
            id: 9,
            claimed_at: 1000,
        };
        assert_eq!(
            serde_json::to_value(ack).unwrap(),
            serde_json::json!({"version":1,"id":9,"claimed_at":1000})
        );
    }
}
