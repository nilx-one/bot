# 0x1 Bot — Telegram text client

**Status:** isolated test transport with a single-message UI and optional AIS login-start; no verified Core product session, game writes, or production cutover yet.

This repository is the **fourth 0x1 client**, and its first text-only client. Players use 0x1 directly in the `@nilxone_bot` chat. The optional Mini App opens the Web interface inside the same bot. The same Rust Core owns Bond, Avaia, inventory, interactions, sessions, progression, and spatial semantics across text and graphical clients.

The existing Telegram Mini App stays in [nilx-one/web](https://github.com/nilx-one/web) as an independent **Web presentation**. The bot is an independently deployable text runtime, but chat and embedded Mini App target **one logical Telegram product client** only after Core verifies the same Bond, Telegram host, physical device and session-bound single-use proof. This is the proposed normative revision in [0x1 #90](https://github.com/nilx-one/0x1/pull/90), tracked in [bot #2](https://github.com/nilx-one/bot/issues/2); it is not yet operational.

## Ownership

- aiaiaiai Identity Service (AIS) owns passwords, provider bindings and verified identity.
- nilx-one/core owns product session activation, game state, permissions, and deterministic commands, errors and text projections.
- nilx-one/bot verifies Telegram transport context, sends typed requests to a least-privilege Core Gateway when available, and renders responses as text.
- prism-hub routes between services where installed; it never owns Bond or game state.

## What this PR implements

Only a separate **test bot**, using Rust and Teloxide. Private, original human commands:

- /start: open the text client's main panel.
- /open: optional shortcut that shows the button to open the embedded Mini App. The WebView uses its own sign-in, and Core handles any transfer of the active game.
- /help: explain basic commands and honest availability.
- /login: optionally request an AIS authorization link from the trusted product Gateway; fail closed when unavailable.
- Other slash commands: reply in text that a verified AIS identity and Core Gateway are required. No gameplay mutation.

Forwarded messages, bot senders and groups are not eligible command sources. The Mini App opens only when the user taps its launch button. A command cannot create location truth or a BondChain.

## Single-message Telegram UI

- `/start`, `/login`, `/subscription`, `/help` and `/open` are top-level commands. `/open` is **not** a synonym for `/start`: it is the explicit entry to the Mini App launch panel. Features such as Bond, Avaia, inventory, subscriptions, organizations and eligible travel are navigated via Inline Keyboard, **not** additional slash commands.
- The first private interaction sends one unpinned login panel. Subsequent commands and button presses call `editMessageText` for that same `message_id`; callbacks check sender, originating message and revision to reject stale presses.
- The test adapter persists **presentation metadata only** in `BOT_UI_STATE_PATH` (default `bot-ui-state.json`): chat, owner, panel message ID, current screen and recently observed message IDs. It never persists an AIS authorization URL, password, token, game state, Core session or Bond identity.
- `/subscription` is a navigation shortcut. Without a real verified Core session, its screen reports unavailability; it does not fabricate Special/Exclusive status. No arbitrary text prompts navigate the world.
- **Sign-in completion:** after `/login`, the panel shows the AIS link and «Підтвердити вхід». The browser returns to the Core Telegram login host's callback, which verifies the AIS Telegram binding. The confirm press sends the sender, chat and original `/login` message to `POST /api/v1/telegram/login/confirm`. Start, callback, and confirmation belong to the dedicated Core login host and use the shared product database. There Core opens this chat's own product session under Single Active Client, beside the Mini App's session. The bot receives only `{profile_id, pub_dress, active}`, never a token, and keeps it in memory for display. Pinning a post-auth panel and restoring the session after a restart are separate follow-ups. Do not pin an authenticated panel, clear chat history or display Bond data based only on a browser redirect, Telegram sender ID or login-start URL.
- On a future verified Core login-completion event, the bot must delete *observed and deletable* prior messages best-effort (Telegram limits history deletion, including a 48-hour window), send and pin one new home panel, then edit that same panel. Never claim a whole chat was cleared. The Core callback and this post-auth transition are separate dependent work, not operational in this test UI.
- A restarted bot reuses the stored panel ID when available. If Telegram deleted or invalidated that message, a failure is logged rather than silently producing multiple panels. Operational recovery will require a reviewed explicit reset path.

## Special for seeds — verified Telegram Market channel (staged)

The initial Market integration is **not a purchase button**. It establishes
verified Telegram-to-Market customer identity and looks up whether that Market
customer's provider account is linked to an existing 0x1 Bond. It uses Market's
documented `POST /v1/auth/external` and `GET /v1/issuer-accounts/0x1`
contracts, with a short-lived signed `X-Market-Customer-Assertion` backed by
HMAC-SHA256. **The POST is an identity write:** Market may create its own
local customer account when that provider first authenticates. It does not
create a Bond, debit seeds or issue a subscription. This staged adapter is
currently unwired, so merely deploying PR #6 will not register users.

All settings are optional **only together**, for a trusted staging adapter:

~~~sh
# No credentials are needed for the existing isolated test bot.
# MARKET_CHANNEL_ORIGIN=https://0xda-market.nilx.one/
# MARKET_CHANNEL_PUBLIC_TOKEN=<market-channel-api-token>
# MARKET_CHANNEL_ASSERTION_SECRET=<market-channel-signing-secret-at-least-32-bytes>
~~~

Market's issuer directory owns its own public-handle grammar: an account may
be linked as `zero` or `0x0sky`; the bot does not impose a `0x` prefix.
A missing, invalid or unavailable directory response is not an absent
subscription or permission to buy.

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

## Walking to a paid Special without a map (staged)

A text client has no MapLibre, yet Core's [Special pickup](https://github.com/nilx-one/core/blob/master/docs/special-pickup.md)
contract needs `walk_route_available`: whether the Avaia can walk to the
pickup point. The bot answers it from the same self-hosted map archive the
Web client draws, with Core doing every rule:

~~~text
BOT_MAP_ARCHIVE_URL ── HTTP Range ──▶ src/pmtiles.rs (header, directories, gzip off)
  ──▶ z14 tiles around both ends ──▶ Core vector_tile::build_walk_graph_from_tiles
  ──▶ Core snap + reach ──▶ walk_route_available ──▶ Core special_pickup_choices
~~~

- `src/pmtiles.rs` is a bounded PMTiles v3 reader: MVT only, gzip or no
  compression, at most 4 MiB per range and 16 MiB inflated, redirects refused,
  `https` only (plain `http` only for `localhost`). Tile ids match the
  `pmtiles` npm reader.
- `src/walk.rs` reads the archive only for a **paid** item whose Avaia stands
  **inside** Core's 300 m radius, at most 16 tiles. A route counts when the
  roads join any place within Core's 30 m snap distance of one end to any
  within it of the other (up to 8 candidates each), so a disconnected stub
  next to the Avaia does not hide the street behind it.
- Any failure to read the archive **fails closed**: no walk suggestion; xPing
  stays offered exactly as Core decides.
- **Unwired.** `paid`, the Avaia's in-world position and the BusinessPoint
  must come from a verified Core Gateway response, which does not exist yet.
  Nothing here dispatches xPing, starts a walk, moves an Avaia or proves
  payment or presence.

~~~sh
# Optional, staged: the public map archive the Web client draws.
# BOT_MAP_ARCHIVE_URL=https://nilx.one/map/0.1.0/basemap.pmtiles
~~~

## Text-client feature scope

Every 0x1 product capability available in Web or native clients must also be usable directly in Telegram chat under the same Core permissions. This includes Bond/Avaia, inventory and items, subscriptions, organizations and eligible travel. Chat flows must work independently of the Mini App. It uses typed Core read/actions and parity tests, not a second set of rules or language-model inference. Work is tracked in [Core #123](https://github.com/nilx-one/core/issues/123) and [Core #124](https://github.com/nilx-one/core/issues/124).

AIS sign-in is a separate dependency ([bot #3](https://github.com/nilx-one/bot/issues/3)). Core must verify the same Bond + Telegram host + physical device before chat and Mini App join the **same logical product session**; distinct surfaces keep different scoped credentials, and external browsers/other devices still obey Single Active Client. This Core continuity mechanism is pending [Core #122](https://github.com/nilx-one/core/issues/122).

## Optional AIS login (separate dependent PR)

The explicit /login command asks the dedicated Core Telegram login host for a
short-lived, single-use OIDC transaction, and only then sends the user to the
AIS page. The bot never guesses an authorization URL, checks a password,
receives a callback token, links a Bond or activates a product session.

The Gateway must implement a service-authenticated
POST /api/v1/telegram/login/start request and return a URL for
https://auth.aiaiaiai.org/0x1 containing state, nonce, code+PKCE S256 and the
dedicated registered Telegram callback. It must bind the real Telegram sender,
persist/consume the transaction, check signed AIS subject continuity and require
explicit account-link confirmation before any Bond binding.

AIS must register the test workload `0x1-telegram-test-bot` with its Ed25519 public key and the single `telegram.login.start` scope for audience `0x1`. The bot exchanges a fresh signed assertion for each login start. This credential authorizes only the workload.

Optional test-only environment settings (all required together):

~~~sh
export BOT_LOGIN_START_URL=https://nilx.one/api/v1/telegram/login/start
export BOT_AIS_KEY_ID='<registered key id>'
export BOT_AIS_SIGNING_SEED='<private Ed25519 seed, unpadded base64url>'
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
export TELEGRAM_BOT_TOKEN='<separate-test-bot-token>'
cargo test --all-targets
cargo run
~~~

An empty token or an unsupported run mode returns a configuration error without opening a polling loop. This repository has no deployed production bot and does not change the existing Web process, Telegram message IDs, native identities or infrastructure.

## Verification

Full CI runs Rust format, Clippy and tests. A later deployable package requires distinct Validate for Deploy checks. Neither pipeline may deploy by itself.

Licensed MPL-2.0. See LICENSE and NOTICE.

---

© 2026 aiaiaiai · aiaiaiai.org
