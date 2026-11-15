# 0x1 Bot

Messenger-native surfaces of the **same 0x1 product**. First implementation: Telegram chat bot. Future WhatsApp and Viber adapters may be added when their real capabilities and provider requirements are validated.

**Status:** test-only bootstrap; no live cutover, no Bond mutations and no production token support.

## One Telegram client, two surfaces

- **Telegram chat bot:** message delivery, commands and private-chat presentation, eventually handled in this repository.
- **Telegram Mini App:** the same Web 0x1 product opened *inside the Telegram bot* via the Web App button. Its React composition root and Telegram WebView SDK stay in [nilx-one/web](https://github.com/nilx-one/web) at `apps/telegram-mini-app` and `packages/host-telegram`. Do **not** duplicate or move the Web UI into this repository.

Bot chat and its Mini App are intended to be **one logical Telegram product client**. Their common Core-owned product session needs the normative Single Active Client revision described in [0x1#87](https://github.com/nilx-one/0x1/pull/87). Until implemented, the bot does not authorize gameplay writes or bypass a client handoff.

## Boundaries

- [aiaiaiai Identity Service](https://github.com/aiaiaiai-org/4x-ais) owns identity and provider binding. Telegram transport evidence is not a Bond login.
- [nilx-one/core](https://github.com/nilx-one/core) owns product sessions, permissions, game state, transitions, interactions and BondChain.
- [Core Gateway proposal](https://github.com/nilx-one/core/issues/122) will allow authenticated bot adapters to call narrow, typed Core operations. This repository must **not** connect to the Core/Web SQLite database.
- [0x1 architecture PR](https://github.com/nilx-one/0x1/pull/88) defines safe Telegram extraction and follow-up.
- prism-hub routes service-to-service requests where available, but is not the authority for identity or product state.

## Source layout

```
src/
  main.rs                 explicit test-only process composition
  application.rs          public command classification; no game authority
  providers/
    telegram.rs           Telegram Bot API adapter and embedded Mini App button
```

The first slice intentionally supports only private-chat `/start`, `/open`, and `/help`. All other slash commands produce a non-mutating unavailable response. The current Telegram production bot, its persistent Open/Status/Admin message slots, location commands and admin workflows continue running in Web until a separately authorized cutover.

## Local test mode

Use a **different Telegram test bot** from the existing production bot:

```sh
export BOT_RUN_MODE=test
export BOT_TEST_TOKEN='<separate-test-bot-token>'
cargo test
cargo run
```

No token is saved in the repository. The application refuses any `BOT_RUN_MODE` other than `test`. It does not configure production webhook, production polling or existing Telegram message slots.

## Verification

Full CI runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all-targets`. This repository has no production deploy target, so Validate for Deploy is not applicable at bootstrap. Add deploy preflight when a real release package/configuration exists. CI never deploys.

## License

MPL-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

---

© 2026 aiaiaiai · aiaiaiai.org
