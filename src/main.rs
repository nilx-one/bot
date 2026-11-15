// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! Test-only composition root. No production poller or Core write paths.

use nilx_one_bot::providers::telegram::run_test_bot;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("nilx_one_bot=info")),
        )
        .init();

    // Explicitly refuse production mode, even if a token is accidentally
    // injected. Cutover requires a separately reviewed implementation.
    assert_eq!(
        std::env::var("BOT_RUN_MODE").as_deref(),
        Ok("test"),
        "only BOT_RUN_MODE=test is supported before the authorized cutover"
    );

    let token = std::env::var("BOT_TEST_TOKEN")
        .expect("BOT_TEST_TOKEN is required: use a separate Telegram test bot");
    assert!(!token.trim().is_empty(), "BOT_TEST_TOKEN must not be empty");
    run_test_bot(token).await;
}
