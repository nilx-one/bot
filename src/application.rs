// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! Transport-neutral intent classification for the initial read-only bootstrap.
//! No provider update becomes a Core command or BondChain event at this stage.

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PublicAction {
    OpenMiniApp,
    Help,
    /// Anything requiring a verified AIS binding and a Core product session.
    NeedsCore,
    Ignore,
}

/// Classifies an input; it never authenticates a user or executes product logic.
/// `/command@botname` is a Telegram convention; other providers may implement
/// their own command parser behind the same action boundary.
#[must_use]
pub fn classify_message(text: &str) -> PublicAction {
    let Some(first) = text.split_whitespace().next() else {
        return PublicAction::Ignore;
    };
    let name = first.split('@').next().unwrap_or_default();

    match name {
        "/start" | "/open" => PublicAction::OpenMiniApp,
        "/help" => PublicAction::Help,
        command if command.starts_with('/') => PublicAction::NeedsCore,
        _ => PublicAction::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publicly_safe_commands_do_not_need_a_bond() {
        assert_eq!(classify_message("/open"), PublicAction::OpenMiniApp);
        assert_eq!(
            classify_message("/open@nilxone_bot"),
            PublicAction::OpenMiniApp
        );
        assert_eq!(
            classify_message("/start payload"),
            PublicAction::OpenMiniApp
        );
        assert_eq!(classify_message("/help"), PublicAction::Help);
    }

    #[test]
    fn privileged_and_unknown_commands_do_not_execute() {
        for command in [
            "/unlink",
            "/travel",
            "/set_position",
            "/whoami",
            "/go@nilxone_bot",
            "/admin",
        ] {
            assert_eq!(classify_message(command), PublicAction::NeedsCore);
        }
    }

    #[test]
    fn plain_text_never_claims_a_product_command() {
        assert_eq!(classify_message("hello"), PublicAction::Ignore);
        assert_eq!(classify_message(""), PublicAction::Ignore);
        assert_eq!(classify_message("  "), PublicAction::Ignore);
    }
}
