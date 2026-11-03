//! Opt-in, public-only link cards. Each hop resolves once, validates *every* answer, and gives
//! reqwest a resolver containing only the validated address. Redirects and thumbnails use the same
//! policy, deadline and redirect budget; no remote image URL is ever returned to the browser.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{ConnectInfo, Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, put};
use axum::{Json, Router};
use futures::future::BoxFuture;
use grok_bot_model::{HttpError, HttpResult};
use grok_bot_store::Store;
use grok_bot_store::generated_images::{ROUTE_PREFIX, discard_generated_image, save_generated_image};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::time::{Instant, timeout_at};
use url::{Host, Url};

use crate::computer::network::is_global_ip;
use crate::http::{ApiResult, Ctx, JsonBody, Remote, bad_request, state_reply};

type Lookup = Arc<dyn Fn(String, Arc<OwnedSemaphorePermit>) -> BoxFuture<'static, std::io::Result<Vec<IpAddr>>> + Send + Sync>;
type Pending = watch::Receiver<Option<HttpResult<LinkPreview>>>;
// Cache time is separate from network deadlines so expiry can be exercised without wall-clock sleeps.
type CacheClock = Arc<dyn Fn() -> Instant + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct LinkPreview {
    url: String,
    title: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_url: Option<String>,
}

#[derive(Clone)]
struct Limits {
    total: Duration,
    ttl: Duration,
    failure_ttl: Duration,
    entries: usize,
    concurrent: usize,
    html_bytes: usize,
    image_bytes: usize,
    redirects: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            total: Duration::from_secs(8),
            ttl: Duration::from_secs(15 * 60),
            failure_ttl: Duration::from_secs(30),
            entries: 128,
            concurrent: 8,
            html_bytes: 512 * 1024,
            image_bytes: 2 * 1024 * 1024,
            redirects: 5,
        }
    }
}

struct Cached {
    result: HttpResult<LinkPreview>,
    expires: Instant,
    used: u64,
}

#[derive(Default)]
struct Cache {
    ready: HashMap<String, Cached>,
    pending: HashMap<String, Pending>,
    clock: u64,
}

impl Cache {
    fn prune(&mut self, now: Instant, retired: &mut Vec<LinkPreview>) {
        self.ready.retain(|_, entry| {
            if entry.expires > now {
                true
            } else {
                if let Ok(preview) = &entry.result {
                    retired.push(preview.clone());
                }
                false
            }
        });
    }

    fn evict(&mut self, capacity: usize, retired: &mut Vec<LinkPreview>) {
        while self.ready.len() >= capacity {
            let Some(key) = self.ready.iter().min_by_key(|(_, entry)| entry.used).map(|(key, _)| key.clone()) else { break };
            if let Some(Cached { result: Ok(preview), .. }) = self.ready.remove(&key) {
                retired.push(preview);
            }
        }
    }
}

struct Policy {
    lookup: Lookup,
    fixtures: HashSet<String>,
}

impl Policy {
    fn new() -> Self {
        Self {
            lookup: Arc::new(|host, slot| {
                Box::pin(async move {
                    tokio::task::spawn_blocking(move || {
                        // libc DNS itself is not cancellable. Keep the admitted job's slot until
                        // it actually returns, even when the preview deadline has already elapsed.
                        let _slot = slot;
                        Ok((host.as_str(), 0).to_socket_addrs()?.take(65).map(|answer| answer.ip()).collect())
                    })
                    .await
                    .map_err(std::io::Error::other)?
                })
            }),
            fixtures: environment_fixtures(),
        }
    }

    async fn resolve(&self, url: &Url, slot: &Arc<OwnedSemaphorePermit>) -> HttpResult<PinnedDns> {
        let (host, answers) = match url.host() {
            Some(Host::Ipv4(ip)) => (ip.to_string(), vec![IpAddr::V4(ip)]),
            Some(Host::Ipv6(ip)) => (ip.to_string(), vec![IpAddr::V6(ip)]),
            Some(Host::Domain(host)) => {
                let answers = (self.lookup)(host.to_owned(), slot.clone()).await.map_err(|_| failed())?;
                (host.to_owned(), answers)
            }
            None => return Err(unsafe_destination()),
        };
        let fixture = self.fixtures.contains(&url.origin().ascii_serialization());
        if answers.is_empty() || answers.len() > 64 || !answers.iter().all(|ip| is_global_ip(*ip) || (fixture && ip.is_loopback())) {
            return Err(unsafe_destination());
        }
        Ok(PinnedDns { host, address: SocketAddr::new(answers[0], url.port_or_known_default().ok_or_else(unsafe_destination)?) })
    }
}

/// No system resolver fallback exists, even if the HTTP stack unexpectedly requests another name.
/// A fresh client per hop also prevents connection-pool reuse from bypassing a later DNS check.
struct PinnedDns {
    host: String,
    address: SocketAddr,
}

impl Resolve for PinnedDns {
    fn resolve(&self, name: Name) -> Resolving {
        let address = (name.as_str() == self.host).then_some(self.address);
        Box::pin(async move {
            match address {
                Some(address) => Ok(Box::new(std::iter::once(address)) as Addrs),
                None => Err(std::io::Error::other("unvalidated DNS name").into()),
            }
        })
    }
}

pub(crate) struct LinkPreviewService {
    store: Arc<Store>,
    policy: Policy,
    limits: Limits,
    slots: Arc<Semaphore>,
    cache: Mutex<Cache>,
    cache_now: CacheClock,
}

impl LinkPreviewService {
    pub(crate) fn new(store: Arc<Store>) -> Self {
        let limits = Limits::default();
        Self {
            store,
            policy: Policy::new(),
            slots: Arc::new(Semaphore::new(limits.concurrent)),
            limits,
            cache: Mutex::new(Cache::default()),
            cache_now: Arc::new(Instant::now),
        }
    }

    fn enabled(&self) -> HttpResult<()> {
        if self.store.read(|state| state.disk().link_previews_enabled) { Ok(()) } else { Err(HttpError::forbidden("Link previews are disabled.")) }
    }

    /// Ready entries and admitted work are separately bounded. Identical requests share one job;
    /// dropping a requesting HTTP connection does not leave an unpolled in-flight cache entry.
    async fn preview(self: &Arc<Self>, input: &str) -> HttpResult<LinkPreview> {
        self.enabled()?;
        let url = preview_url(input)?;
        let key = url.as_str().to_owned();
        let mut retired = Vec::new();
        let (ready, pending) = {
            let mut cache = self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.prune((self.cache_now)(), &mut retired);
            cache.clock += 1;
            let used = cache.clock;
            if let Some(entry) = cache.ready.get_mut(&key) {
                entry.used = used;
                (Some(entry.result.clone()), None)
            } else if let Some(pending) = cache.pending.get(&key) {
                (None, Some(pending.clone()))
            } else {
                let slot = match self.slots.clone().try_acquire_owned() {
                    Ok(slot) => slot,
                    Err(_) => {
                        drop(cache);
                        self.retire(retired);
                        return Err(HttpError::unavailable("Link previews are busy. Try again shortly."));
                    }
                };
                let (send, pending) = watch::channel(None);
                cache.pending.insert(key.clone(), pending.clone());
                let service = self.clone();
                tokio::spawn(async move {
                    let slot = Arc::new(slot);
                    let deadline = Instant::now() + service.limits.total;
                    let result = service.download(url, deadline, &slot).await;
                    let result = match service.enabled() {
                        Ok(()) => result,
                        Err(error) => {
                            if let Ok(preview) = result {
                                service.retire(vec![preview]);
                            }
                            Err(error)
                        }
                    };
                    let mut retired = Vec::new();
                    {
                        let mut cache = service.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        cache.pending.remove(&key);
                        let now = (service.cache_now)();
                        cache.prune(now, &mut retired);
                        if service.enabled().is_ok() {
                            cache.evict(service.limits.entries, &mut retired);
                            cache.clock += 1;
                            let used = cache.clock;
                            let ttl = if result.is_ok() { service.limits.ttl } else { service.limits.failure_ttl };
                            cache.ready.insert(key, Cached { result: result.clone(), expires: now + ttl, used });
                        } else if let Ok(preview) = &result {
                            retired.push(preview.clone());
                        }
                    }
                    service.retire(retired);
                    send.send_replace(Some(result));
                });
                (None, Some(pending))
            }
        };
        self.retire(retired);
        let result = match ready {
            Some(result) => result,
            None => {
                let mut pending = pending.expect("a cache miss has an admitted job");
                pending.wait_for(|value| value.is_some()).await.map_err(|_| failed())?.as_ref().expect("completed preview").clone()
            }
        };
        self.enabled()?; // Includes cache hits and a disable that happened while this request waited.
        result
    }

    fn clear(&self) {
        let retired = {
            let mut cache = self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.ready.drain().filter_map(|(_, entry)| entry.result.ok()).collect()
        };
        self.retire(retired);
    }

    fn retire(&self, previews: Vec<LinkPreview>) {
        let mut ids: HashSet<String> = previews.into_iter().filter_map(|preview| preview.image_url?.strip_prefix(ROUTE_PREFIX).map(str::to_owned)).collect();
        if ids.is_empty() {
            return;
        }
        // Accepted user work owns copied local image references even before its message starts.
        self.store.read(|state| {
            let disk = state.disk();
            let texts =
                disk.conversations.iter().flat_map(|c| &c.messages).map(|message| message.text.as_str()).chain(disk.work_queue.iter().filter_map(|work| {
                    match &work.kind {
                        grok_bot_model::WorkKind::User { text, .. } => Some(text.as_str()),
                        _ => None,
                    }
                }));
            for text in texts {
                for referenced in grok_bot_store::generated_images::referenced_ids(text) {
                    ids.remove(referenced);
                }
            }
        });
        for id in ids {
            discard_generated_image(self.store.data_dir(), &id);
        }
    }

    async fn download(&self, url: Url, deadline: Instant, slot: &Arc<OwnedSemaphorePermit>) -> HttpResult<LinkPreview> {
        let mut redirects = self.limits.redirects;
        let (final_url, bytes) = within(deadline, self.fetch(url, self.limits.html_bytes, true, &mut redirects, slot)).await?;
        let metadata = within(deadline, blocking(slot, move || metadata(&bytes, &final_url))).await?;
        let mut preview = LinkPreview { url: metadata.url.clone(), title: metadata.title, description: metadata.description, image_url: None };
        let mut thumbnail = None;
        if let Some(image) = metadata.image {
            // An unusable, private, oversized, redirected, or slow thumbnail leaves a text card.
            if let Ok(saved) = within(deadline, async {
                let (_, bytes) = self.fetch(image, self.limits.image_bytes, false, &mut redirects, slot).await?;
                self.enabled()?;
                let data_dir = self.store.data_dir().to_path_buf();
                blocking(slot, move || {
                    let id = save_generated_image(&data_dir, &bytes).map_err(|_| failed())?;
                    Ok(Thumbnail { data_dir, id: Some(id) })
                })
                .await?
            })
            .await
            {
                thumbnail = Some(saved);
            }
        }
        self.enabled()?;
        preview.image_url = thumbnail.map(Thumbnail::keep);
        Ok(preview)
    }

    async fn fetch(&self, mut url: Url, limit: usize, html: bool, redirects: &mut usize, slot: &Arc<OwnedSemaphorePermit>) -> HttpResult<(Url, Vec<u8>)> {
        loop {
            self.enabled()?;
            let pinned = self.policy.resolve(&url, slot).await?;
            self.enabled()?; // DNS may have been slow; disabling must prevent the subsequent hop.
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .referer(false)
                .no_gzip()
                .no_brotli()
                .no_deflate()
                .no_zstd()
                .http1_only()
                .pool_max_idle_per_host(0)
                .timeout(self.limits.total)
                .dns_resolver(Arc::new(pinned))
                .build()
                .map_err(|_| failed())?;
            self.enabled()?;
            // Nothing from the incoming browser request is forwarded. This fresh client has no
            // cookie store, default authorization, ambient proxy, or Referer state.
            let mut response = client
                .get(url.clone())
                .header(header::ACCEPT, if html { "text/html, application/xhtml+xml" } else { "image/png, image/jpeg, image/gif, image/webp" })
                .header(header::ACCEPT_ENCODING, "identity")
                .send()
                .await
                .map_err(|_| failed())?;
            if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                if *redirects == 0 {
                    return Err(HttpError::new(502, "The link redirected too many times."));
                }
                *redirects -= 1;
                let location = response.headers().get(header::LOCATION).and_then(|value| value.to_str().ok()).ok_or_else(failed)?;
                valid_reference(location)?;
                url = preview_url(url.join(location).map_err(|_| unsafe_destination())?.as_str())?;
                continue;
            }
            if !response.status().is_success() {
                return Err(failed());
            }
            let headers = response.headers();
            if headers.get(header::CONTENT_ENCODING).is_some_and(|value| value.as_bytes() != b"identity") {
                return Err(failed()); // Do not admit compressed bodies or decompression bombs.
            }
            if html {
                let mime = headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()).and_then(|value| value.split(';').next()).unwrap_or("");
                if !mime.trim().eq_ignore_ascii_case("text/html") && !mime.trim().eq_ignore_ascii_case("application/xhtml+xml") {
                    return Err(HttpError::new(422, "The link is not an HTML page."));
                }
            }
            if response.content_length().is_some_and(|length| length > limit as u64) {
                return Err(HttpError::too_large("The link preview response is too large."));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| failed())? {
                if chunk.len() > limit.saturating_sub(bytes.len()) {
                    return Err(HttpError::too_large("The link preview response is too large."));
                }
                bytes.extend_from_slice(&chunk);
            }
            return Ok((url, bytes));
        }
    }
}

/// Cleans up a save that outlives the deadline (a blocking filesystem operation cannot be aborted).
struct Thumbnail {
    data_dir: PathBuf,
    id: Option<String>,
}

impl Thumbnail {
    fn keep(mut self) -> String {
        format!("{ROUTE_PREFIX}{}", self.id.take().expect("saved thumbnail"))
    }
}

impl Drop for Thumbnail {
    fn drop(&mut self) {
        if let Some(id) = &self.id {
            discard_generated_image(&self.data_dir, id);
        }
    }
}

/// Blocking tasks cannot be aborted. Their copy of the admitted-work permit prevents timeout or
/// client disconnect from accumulating parser/filesystem work after the waiting future is dropped.
async fn blocking<T: Send + 'static>(slot: &Arc<OwnedSemaphorePermit>, work: impl FnOnce() -> T + Send + 'static) -> HttpResult<T> {
    let slot = slot.clone();
    tokio::task::spawn_blocking(move || {
        let _slot = slot;
        work()
    })
    .await
    .map_err(|_| failed())
}

async fn within<T>(deadline: Instant, future: impl Future<Output = HttpResult<T>>) -> HttpResult<T> {
    let timed_out = || HttpError::new(504, "The link preview timed out.");
    if Instant::now() >= deadline {
        return Err(timed_out());
    }
    timeout_at(deadline, future).await.map_err(|_| timed_out())?
}

fn failed() -> HttpError {
    HttpError::new(502, "The link preview could not be downloaded.")
}

fn unsafe_destination() -> HttpError {
    HttpError::forbidden("Link previews require a public HTTP or HTTPS destination.")
}

fn valid_reference(input: &str) -> HttpResult<()> {
    if input.is_empty() || input.len() > 4096 || input.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\') {
        return Err(HttpError::bad_request("Use a valid HTTP or HTTPS URL without credentials."));
    }
    Ok(())
}

fn preview_url(input: &str) -> HttpResult<Url> {
    valid_reference(input)?;
    let mut url = Url::parse(input).map_err(|_| HttpError::bad_request("Use an absolute HTTP or HTTPS URL."))?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() || !url.username().is_empty() || url.password().is_some() || url.port() == Some(0) {
        return Err(unsafe_destination());
    }
    url.set_fragment(None);
    Ok(url)
}

struct Metadata {
    url: String,
    title: String,
    description: String,
    image: Option<Url>,
}

fn metadata(bytes: &[u8], final_url: &Url) -> Metadata {
    // Metadata normally occurs in the head. Bound parser work independently of the network cap:
    // both HTML5 tree building and maliciously nested/escaped meta values can otherwise cost much
    // more CPU than their byte count suggests.
    let source = String::from_utf8_lossy(bytes);
    let html = Html::parse_document(parser_input(&source, 64 * 1024, 2048));
    let mut og_title = None;
    let mut og_description = None;
    let mut description = None;
    let mut image = None;
    for meta in html.select(&Selector::parse("meta").expect("static selector")) {
        let Some(content) = meta.value().attr("content").filter(|value| !value.trim().is_empty()) else { continue };
        let name = meta.value().attr("property").or_else(|| meta.value().attr("name")).unwrap_or("");
        match name.to_ascii_lowercase().as_str() {
            "og:title" if og_title.is_none() => og_title = Some(content),
            "og:description" if og_description.is_none() => og_description = Some(content),
            "description" if description.is_none() => description = Some(content),
            "og:image" if image.is_none() => image = Some(content),
            _ => {}
        }
    }
    let title = html.select(&Selector::parse("title").expect("static selector")).next().map(|title| title.text().collect::<String>()).unwrap_or_default();
    Metadata {
        url: final_url.as_str().to_owned(), // Ignore canonical/og:url and <base> supplied by the page.
        title: plain_text(og_title.unwrap_or(&title), 512),
        description: plain_text(og_description.or(description).unwrap_or(""), 2048),
        image: image.and_then(|image| {
            valid_reference(image).ok()?;
            preview_url(final_url.join(image).ok()?.as_str()).ok()
        }),
    }
}

fn parser_input(input: &str, bytes: usize, tags: usize) -> &str {
    let mut end = input.len().min(bytes);
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    let prefix = &input[..end];
    prefix.match_indices('<').nth(tags).map_or(prefix, |(at, _)| &prefix[..at])
}

fn plain_text(input: &str, max: usize) -> String {
    let fragment = Html::parse_fragment(parser_input(input, 8192, 256));
    let mut text = String::new();
    let mut pending = vec![fragment.tree.root()];
    while let Some(node) = pending.pop() {
        if node.value().as_element().is_some_and(|el| matches!(el.name(), "script" | "style" | "template")) {
            continue;
        }
        if let Some(value) = node.value().as_text() {
            text.push_str(&value.text);
        }
        pending.extend(node.children().rev()); // Linear traversal, skipping entire unsafe subtrees.
    }
    let text: String =
        text.chars().filter(|c| !c.is_control() || c.is_whitespace()).filter(|c| !matches!(*c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')).collect();
    text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(max).collect()
}

/// Debug e2e fixtures are exact, bare loopback HTTP origins, never hostnames or wildcard ports.
/// Release builds cannot enable this path, regardless of their environment.
fn environment_fixtures() -> HashSet<String> {
    if !cfg!(debug_assertions) || std::env::var("NODE_ENV").as_deref() != Ok("test") {
        return HashSet::new();
    }
    std::env::var("GROK_BOT_LINK_PREVIEW_TEST_ORIGINS").ok().and_then(|value| fixture_origins(&value)).unwrap_or_default()
}

fn fixture_origins(input: &str) -> Option<HashSet<String>> {
    let values: Vec<String> = serde_json::from_str(input).ok()?;
    if values.len() > 16 {
        return None;
    }
    values
        .into_iter()
        .map(|value| {
            let url = preview_url(&value).ok()?;
            let loopback = matches!(url.host(), Some(Host::Ipv4(ip)) if ip == std::net::Ipv4Addr::LOCALHOST)
                || matches!(url.host(), Some(Host::Ipv6(ip)) if ip.is_loopback());
            (url.scheme() == "http" && loopback && url.port().is_some_and(|port| port != 0) && value == url.origin().ascii_serialization()).then_some(value)
        })
        .collect()
}

pub(crate) fn routes() -> Router<Ctx> {
    Router::new().route("/link-preview", get(preview)).route("/link-preview/settings", put(settings))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewQuery {
    url: String,
}

async fn preview(
    State(ctx): State<Ctx>,
    headers: axum::http::HeaderMap,
    query: Result<Query<PreviewQuery>, axum::extract::rejection::QueryRejection>,
) -> ApiResult {
    // This GET has an outbound-network side effect. The custom header prevents cross-site image
    // tags/navigation from triggering it, even though ordinary read-only API GETs need no header.
    if headers.get("x-grok-bot").map(|value| value.as_bytes()) != Some(b"1") {
        return Err(HttpError::forbidden("Link previews require X-Grok-Bot: 1.").into());
    }
    ctx.link_previews.enabled()?;
    let Query(input) = query.map_err(|_| bad_request("url: Provide one absolute HTTP or HTTPS URL."))?;
    let result = ctx.link_previews.preview(&input.url).await?;
    ctx.link_previews.enabled()?;
    Ok(Json(result).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Enabled {
    enabled: bool,
}

async fn settings(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, JsonBody(input): JsonBody<Enabled>) -> ApiResult {
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|state| state.set_link_previews_enabled(input.enabled)).await }).await?;
    if !input.enabled {
        ctx.link_previews.clear();
    }
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

#[cfg(test)]
mod tests;
