# AGENTS.md — nilx-one/bot

## Client and protocol

- This repository is the **fourth 0x1 product client and the first text client**, not a launcher or a subset of the Web Mini App.
- A player can read product state and use Core-supported capabilities in a conversation, without MapLibre or a native application. A command becomes a typed Core request and a textual projection; the chat transport never owns the game rule.
- Telegram Mini App remains in nilx-one/web as a **distinct Web presentation**. Bot chat and Mini App are two surfaces of one **logical Telegram client** only after Core verifies the same Bond, host, physical device and one-use session continuity; independent runtime deployment does not imply separate active clients.
- Do not automatically launch the production Mini App or share Web cookies/bot tokens. `/open` requests a Core-authorized in-Telegram surface transition, **never** a new active session. Until the Core continuity protocol is implemented, report unavailable. Distinct devices or Safari/Discord require confirmed Single Active Client handoff.
- Only aiaiaiai Identity Service can authenticate and link a real identity. Telegram usernames, IDs and forwarded text do not authenticate Bond authority. Core owns session activation, Bond/Avaia state, inventory and relationship truth.
- Do not connect to product SQLite. Use least-privilege, versioned Core Gateway requests only after the corresponding server implementation and authorization rules are reviewed.
- Never claim BondChain completion from a delivered message, bot acknowledgement, AI output, or client presentation.
- Reject forwarded, anonymous/group, bot and unverified updates from product command execution. Unknown/disabled commands return text and never fabricate success.
- Until the Gateway/identity/session contracts are verified, commands requiring Core are unavailable, **not rerouted to another client**.
- No production polling, token reuse, session migration or deploy without explicit cutover approval. Exactly one polling owner per production Telegram bot token.

## GitHub delivery

- Never commit directly to the default branch. Start from its latest state. One task per branch/PR; dependent stages stay separated.
- Full CI: format, Clippy, tests and code contracts on exact head. Validate for Deploy only when a deployable artifact is ready; do not deploy in CI.
- Merge and deploy require separate explicit authorization. No duplicate work or unverifiable completion claims.
- Commit author and coauthor trailers describe actual contributors; never fabricate identities, tools or joint authorship.
- Authored source/config files use the repository-specific copyright header and MPL-2.0 SPDX identifier; preserve third-party notices.

---

© 2026 aiaiaiai · aiaiaiai.org
