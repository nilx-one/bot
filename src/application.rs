// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! Provider-neutral command classification for the independent 0x1 text client.
//! No Telegram update becomes a Bond, a session or a Core command here.

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PublicAction {
    /// Enter the text experience, without opening the Web Mini App.
    EnterTextClient,
    Help,
    /// Requires verified AIS identity, Core product session and typed gateway.
    NeedsCore,
    Ignore,
}

/// Returns the semantic action, never authenticates or executes a game rule.
/// Telegram may append an @botname recipient to slash commands.
#[must_use]
pub fn classify_message(text: &str) -> PublicAction {
    let Some(first) = text.split_whitespace().next() else {
        return PublicAction::Ignore;
    };
    let raw_name = first.split('@').next().unwrap_or_default();
    let name = raw_name.to_ascii_lowercase();

    match name.as_str() {
        "/start" | "/open" => PublicAction::EnterTextClient,
        "/help" => PublicAction::Help,
        command if command.starts_with('/') => PublicAction::NeedsCore,
        _ => PublicAction::Ignore,
    }
}

/// Text replies for unauthenticated bootstrap commands. A future authorized
/// Core answer replaces NeedsCore; no Web button, invented data or action.
#[must_use]
pub const fn public_reply(action: PublicAction) -> Option<&'static str> {
    match action {
        PublicAction::EnterTextClient => Some(
            "0x1 · Текстовий клієнт\n\
             Тут можна користуватися 0x1 без карти.\n\
             /help — команди та стан доступності.",
        ),
        PublicAction::Help => Some(
            "0x1 · Допомога\n\
             /start — відкрити текстовий клієнт\n\
             /help — допомога\n\
             Інвентар, Bond, Avaia та інші дії стануть доступні після AIS і Core Gateway.",
        ),
        PublicAction::NeedsCore => Some(
            "Ця команда поки недоступна: потрібні AIS-авторизація та Core Gateway. Дані 0x1 не змінено.",
        ),
        PublicAction::Ignore => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_entry_is_not_the_web_mini_app() {
        for command in ["/start", "/open", "/Open", "/START", "/open@nilxone_bot"] {
            assert_eq!(classify_message(command), PublicAction::EnterTextClient);
            let reply = public_reply(classify_message(command)).unwrap();
            assert!(reply.contains("Текстовий клієнт"));
            assert!(!reply.contains("nilx.one/telegram"));
        }
    }

    #[test]
    fn help_and_unavailable_are_text_only() {
        assert_eq!(classify_message("/HELP"), PublicAction::Help);
        assert!(public_reply(PublicAction::Help).unwrap().contains("Допомога"));
        for command in ["/unlink", "/travel", "/set_position", "/inventory", "/avaia", "/admin"] {
            assert_eq!(classify_message(command), PublicAction::NeedsCore);
            let reply = public_reply(classify_message(command)).unwrap();
            assert!(reply.contains("Core Gateway"));
            assert!(!reply.contains("http"));
        }
    }

    #[test]
    fn ordinary_text_has_no_implicit_authority() {
        for input in ["hello", "лети на золоті", "", "   "] {
            assert_eq!(classify_message(input), PublicAction::Ignore);
            assert_eq!(public_reply(classify_message(input)), None);
        }
    }
}
