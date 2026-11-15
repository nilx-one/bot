# AGENTS.md — nilx-one/bot

## Architecture and protocol

- A Telegram chat bot and its Telegram Mini App are two surfaces of one intended logical 0x1 Telegram client. The Mini App's Web source remains in nilx-one/web.
- Transport adapters never define Bond, Avaia, Interaction, BondChain, completion, consent, Relationship, game writes or client activation. Those belong to nilx-one/core.
- Only aiaiaiai Identity Service can verify/link provider identity. Telegram usernames, chat IDs and forwarded text do not authenticate Bond authority.
- Do not connect directly to the product database. Use typed least-privilege Core Gateway contracts after they are implemented and reviewed.
- Until the Core shared-logical-client session contract exists, bot commands may not mutate gameplay or bypass Single Active Client.
- No production Telegram token, webhook, or poller before explicit cutover authorization. Never have two polling owners of the same token.
- Preserve the current bot's Open/Status/Admin slot IDs and replay guards when eventually migrating. Delivery receipts are not BondChain facts.

## GitHub delivery

- Never commit to the default branch directly. Start from latest main. One independent task per feature/ or fix/ branch and PR.
- Inspect existing repository instructions and related PRs before modifying code. Keep changes small, tested and rollback-friendly.
- Draft PR while incomplete, then Full CI on the exact head. Validate for Deploy only when applicable; reuse CI outputs and do not deploy in CI.
- Merge and deploy require separate explicit authorization under this project's delivery policy; never assume merge implies deploy.
- Attribute commits to the actual GitHub account; do not fabricate 0x0sky/x0skai co-authorship or tool contributions. Preserve genuine author and required coauthor trailers.
- Source and config files use `© 2026 aiaiaiai · aiaiaiai.org` and `SPDX-License-Identifier: MPL-2.0` when supported. Never overwrite third-party licenses.
- Do not claim CI, runtime availability, image activation or migration success without evidence.

---

© 2026 aiaiaiai · aiaiaiai.org
