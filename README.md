# 0x1 Bot — independent text client

**Status:** isolated test bootstrap; no AIS login, game writes, or production cutover yet.

This repository is the **fourth 0x1 client**, and its first text-only client. The player uses 0x1 directly in a messenger conversation; there is no requirement to open a Web Mini App or a native app. The same Rust Core owns Bond, Avaia, inventory, interactions, sessions, progression, and spatial semantics across text and graphical clients.

The existing Telegram Mini App stays in [nilx-one/web](https://github.com/nilx-one/web). It is an independent Web presentation and is not the gameplay fallback or shared login session for this text client. A future adapter may expose a deliberate link to Web without mixing identities, product sessions or authority.

## Ownership

- aiaiaiai Identity Service (AIS) owns passwords, provider bindings and verified identity.
- nilx-one/core owns product session activation, game state, permissions, and deterministic commands, errors and text projections.
- nilx-one/bot verifies Telegram transport context, sends typed requests to a least-privilege Core Gateway when available, and renders responses as text.
- prism-hub routes between services where installed; it never owns Bond or game state.

## What this PR implements

Only a separate **test bot**, using Rust and Teloxide. Private, original human commands:

- /start and /open: introduce the standalone text client.
- /help: explain basic commands and honest availability.
- /login: optionally request an AIS authorization link from the trusted product Gateway; fail closed when unavailable.
- Other slash commands: reply in text that a verified AIS identity and Core Gateway are required. No gameplay mutation.

Forwarded messages, bot senders and groups are not eligible command sources. No command is automatically handed to the production Web Mini App. A command cannot create location truth or a BondChain.

## Special for seeds — verified Telegram Market channel (staged)

The initial Market integration is **not a purchase button**. It establishes
verified Telegram-to-Market customer identity and looks up whether that Market
customer's provider account is linked to an existing 0x1 Bond. It uses Market's
documented `POST /v1/auth/external` and `GET /v1/issuer-accounts/0x1`
contracts, with a short-lived signed `X-Market-Customer-Assertion` backed by
HMAC-SHA256. The Market may create its own local customer account when that
provider first authenticates. It does not create a Bond.

All settings are optional **only together**, for a trusted staging adapter:

~~~sh
# No credentials are needed for the existing isolated test bot.
# MARKET_CHANNEL_ORIGIN=https://0xda-market.nilx.one/
# MARKET_CHANNEL_PUBLIC_TOKEN=<market-channel-api-token>
# MARKET_CHANNEL_ASSERTION_SECRET=<market-channel-signing-secret-at-least-32-bytes>
~~~

The channel secret is shared **only** with the authenticated Market verifier.
Do not use the issuer token or `NILX_BROWSER_HANDOFF_TOKEN` as the channel
secret. A Telegram username or arbitrary provider ID cannot supply a customer
assertion. The adapter remains unconnected to the live bot panel until the
trusted Core product-session gate and provider-link continuity are reviewed.

**No orders are submitted here.** Completing purchase requires a Core-authorized
active client, Market's verified channel-user check, current active Special
seeds prices, explicit confirmation, idempotent `POST /v1/issuer-orders`,
order status, cancellation/recovery and issuer worker fulfillment. Market's
`ENABLE_ISSUER_ORDER_ROUTES`, Core's `MARKET_ISSUER_WORKER`, catalog SKU
activation, production secrets and deploy remain disabled until independent
E2E evidence and explicit authorization. Read the
[Market issuer-order contract](https://github.com/0xda-market/core/blob/master/docs/architecture/issuer-orders.md)
and [Core's subscription contract](https://github.com/nilx-one/core/blob/master/docs/subscriptions.md).

## Text-client feature scope

The finished text client must expose **all existing Core product capabilities**, including information about Bond/Avaia, inventory and items, subscriptions, organizations and eligible travel. It uses typed Core read/actions and parity tests, not a second set of rules or language-model inference. Work is tracked in [Core #123](https://github.com/nilx-one/core/issues/123) and [Core #124](https://github.com/nilx-one/core/issues/124).

AIS sign-in is a separate dependency ([bot #3](https://github.com/nilx-one/bot/issues/3)). Bot chat has its own Core-controlled product client/session and must participate in Single Active Client rather than sharing the Mini App's browser session.

## Optional AIS login (separate dependent PR)

The explicit /login command asks the authenticated Core-owned Gateway for a
short-lived, single-use OIDC transaction, and only then sends the user to the
AIS page. The bot never guesses an authorization URL, checks a password,
receives a callback token, links a Bond or activates a product session.

The Gateway must implement a service-authenticated
POST /api/v1/telegram/login/start request and return a URL for
https://auth.aiaiaiai.org/0x1 containing state, nonce, code+PKCE S256 and the
dedicated registered Telegram callback. It must bind the real Telegram sender,
persist/consume the transaction, check signed AIS subject continuity and require
explicit account-link confirmation before any Bond binding.

Optional test-only environment settings (all required together):

~~~sh
export BOT_LOGIN_START_URL=https://nilx.one/api/v1/telegram/login/start
export BOT_GATEWAY_SERVICE_TOKEN='<separate 32+ character service secret>'
export BOT_OIDC_CALLBACK_URL=https://nilx.one/api/v1/telegram/login/callback
~~~

The dedicated Telegram callback does not exist yet. Do not substitute the
regular browser product-session callback: that would create a browser session
rather than confirm the correct Telegram client. Without a working Gateway,
the command answers that login is unavailable. This is not an end-to-end
account-link or a production feature.

## Test mode

Use a **separate Telegram test bot token**, never the running production bot's token:

~~~sh
export BOT_RUN_MODE=test
export BOT_TEST_TOKEN='<separate-test-bot-token>'
cargo test --all-targets
cargo run
~~~

An empty token or an unsupported run mode returns a configuration error without opening a polling loop. This repository has no deployed production bot and does not change the existing Web process, Telegram message IDs, native identities or infrastructure.

## Verification

Full CI runs Rust format, Clippy and tests. A later deployable package requires distinct Validate for Deploy checks. Neither pipeline may deploy by itself.

Licensed MPL-2.0. See LICENSE and NOTICE.

---

© 2026 aiaiaiai · aiaiaiai.org
