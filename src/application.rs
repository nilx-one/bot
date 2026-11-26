// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! Provider-neutral command classification for the independent 0x1 text client.
//! No Telegram update becomes a Bond, a session or a Core command here.

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PublicAction {
    /// Enter the text experience, without opening the Web Mini App.
    EnterTextClient,
    /// Explicit request to show the Telegram Mini App launch button.
    OpenMiniApp,
    Help,
    /// Requires a server-issued, single-use AIS browser transaction.
    StartLogin,
    /// Text-only entry to the Core-owned subscription projection.
    Subscription,
    OwnBond,
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
        "/start" => PublicAction::EnterTextClient,
        "/open" => PublicAction::OpenMiniApp,
        "/help" => PublicAction::Help,
        "/login" => PublicAction::StartLogin,
        "/bond" => PublicAction::OwnBond,
        "/subscription" => PublicAction::Subscription,
        command if command.starts_with('/') => PublicAction::NeedsCore,
        _ => PublicAction::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_case_insensitive_and_do_not_open_web() {
        for command in ["/start", "/START", "/start@nilxone_bot"] {
            assert_eq!(classify_message(command), PublicAction::EnterTextClient);
        }
        for command in ["/open", "/Open", "/OPEN@nilxone_bot"] {
            assert_eq!(classify_message(command), PublicAction::OpenMiniApp);
        }
        assert_eq!(classify_message("/Bond@nilxone_bot"), PublicAction::OwnBond);
        assert_eq!(classify_message("/LOGIN"), PublicAction::StartLogin);
        assert_eq!(classify_message("/HELP"), PublicAction::Help);
        assert_eq!(
            classify_message("/SUBSCRIPTION"),
            PublicAction::Subscription
        );
    }

    #[test]
    fn unsupported_commands_need_authorized_core_and_text_is_not_a_command() {
        for command in [
            "/unlink",
            "/travel",
            "/set_position",
            "/inventory",
            "/avaia",
            "/admin",
        ] {
            assert_eq!(classify_message(command), PublicAction::NeedsCore);
        }
        for input in ["hello", "лети на золоті", "", "   "] {
            assert_eq!(classify_message(input), PublicAction::Ignore);
        }
    }
}
