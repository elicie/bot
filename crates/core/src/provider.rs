//! OpenAI-compatible provider check (`GET {baseUrl}/models`). Port of `checkProvider` in
//! `server/store.ts`. CONTRACT (files agent).

use grok_bot_model::ProviderCheck;

/// 6 s timeout, no redirects, 1 MiB body cap, `{ data: [{ id }] }` (max 10000), sorted unique ids.
/// When `model` is non-empty and missing from the list, `ok` is false.
pub async fn check_provider(base_url: &str, api_key: &str, model: &str) -> ProviderCheck {
    let _ = (base_url, api_key, model);
    todo!("files agent")
}
