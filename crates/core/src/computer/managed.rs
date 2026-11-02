//! `ManagedComputer`: one private, sandboxed Chrome behind the loopback proxy. Port of
//! `server/computer.ts`.
//!
//! Every start gets a new epoch, a fresh 0700 profile directory, a new proxy with new credentials,
//! and a verified Chrome: the command line must carry the protection flags, Linux must report an
//! active seccomp sandbox, and probes must show that traffic uses the proxy and that Chrome cannot
//! resolve names itself. Any failure stops the session; there is no fallback.
//!
//! Requests carry one-use UUIDs. Actions name the current epoch and frame; a frame is consumed by
//! the operation that names it, so a replayed or raced action fails as stale. Stop aborts every
//! pending operation at once.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use base64::Engine as _;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use grok_bot_model::{HttpError, HttpResult, new_id};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::{broadcast, mpsc, watch};

use super::cdp::{self, CdpError, Chrome, Event};
use super::network::{Lookup, NetworkPolicy};
use super::proxy::{ComputerProxy, DENIED_BODY};
use super::{
    COMPUTER_KEYS, ComputerControl, ComputerFrame, ComputerObservation, ComputerScreenshot, ComputerService, ComputerState, ComputerStatus, PAGE_TEXT_LIMIT, PAGE_TEXT_NODE_LIMIT, VIEWPORT,
    VIEWPORT_HEIGHT, VIEWPORT_WIDTH, bounded_text, is_uuid, safe_label, scripts, test_mode, utf16_len,
};

const OPERATION_TIMEOUT: Duration = Duration::from_secs(20);
const NAVIGATION_TIMEOUT: Duration = Duration::from_secs(12);
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REQUESTS: usize = 10_000;
const MAX_PENDING: usize = 8;
const ISOLATED_WORLD: &str = "grok-managed-computer";

const PROTECTION_ARGUMENTS: [&str; 11] = [
    "--enable-automation",
    "--proxy-bypass-list=<-loopback>",
    "--disable-quic",
    "--disable-http2",
    "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
    "--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE 127.0.0.1",
    "--disable-background-networking",
    "--disable-component-update",
    "--disable-sync",
    "--no-first-run",
    "--no-default-browser-check",
];

/// Further Chrome defaults Playwright applied to automated launches.
const LAUNCH_ARGUMENTS: [&str; 24] = [
    "--disable-field-trial-config",
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-back-forward-cache",
    "--disable-breakpad",
    "--disable-client-side-phishing-detection",
    "--disable-component-extensions-with-background-pages",
    "--disable-default-apps",
    "--disable-dev-shm-usage",
    "--disable-extensions",
    "--disable-features=Translate,MediaRouter,OptimizationHints,DialMediaRouteProvider,InterestFeedContentSuggestions,AutofillServerCommunication",
    "--disable-hang-monitor",
    "--disable-ipc-flooding-protection",
    "--disable-prompt-on-repost",
    "--disable-renderer-backgrounding",
    "--force-color-profile=srgb",
    "--metrics-recording-only",
    "--password-store=basic",
    "--use-mock-keychain",
    "--no-service-autorun",
    "--headless",
    "--hide-scrollbars",
    "--mute-audio",
    "--lang=en-US",
];

const UNSANDBOXED_ARGUMENTS: [&str; 3] = ["--no-sandbox", "--disable-setuid-sandbox", "--disable-seccomp-filter-sandbox"];

const LAUNCH_FAILED: &str = "Managed Chrome could not start with the required sandbox and network protections. Install a supported Chrome and enable the host sandbox. No insecure fallback was attempted.";
const INVALID_REQUEST: &str = "Invalid computer request. Use a UUID request ID and the current epoch and frame.";

/// Test-only browser override (`testBrowser` of the Node options).
#[derive(Clone, Debug, Default)]
pub struct TestBrowser {
    pub executable_path: Option<PathBuf>,
    /// `false` launches Chrome with `--no-sandbox` (tests only).
    pub chromium_sandbox: bool,
}

#[derive(Clone, Default)]
pub struct ComputerOptions {
    /// Loopback `http://127.0.0.1:<port>` origins treated as reachable (tests only).
    pub allow_test_origins: Vec<String>,
    pub test_browser: Option<TestBrowser>,
    /// Replaces the system resolver (tests only).
    pub lookup: Option<Lookup>,
}

impl ComputerOptions {
    /// Options for the server binary. `GROK_BOT_COMPUTER_TEST_ORIGINS` (comma-separated loopback
    /// fixture origins) is read only in [`test_mode`]; otherwise it is ignored.
    pub fn from_environment() -> Self {
        let origins: Vec<String> = std::env::var("GROK_BOT_COMPUTER_TEST_ORIGINS").unwrap_or_default().split(',').map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned).collect();
        if origins.is_empty() {
            return Self::default();
        }
        if !test_mode() {
            eprintln!("GROK_BOT_COMPUTER_TEST_ORIGINS is ignored outside test builds.");
            return Self::default();
        }
        Self { allow_test_origins: origins, ..Self::default() }
    }

    fn has_overrides(&self) -> bool {
        !self.allow_test_origins.is_empty() || self.test_browser.is_some() || self.lookup.is_some()
    }
}

/// Internal failure: an HTTP error shown as-is, or any other failure that callers replace with
/// their generic message (`HttpError` vs other errors in the Node implementation).
enum Fault {
    Http(HttpError),
    Other,
}

impl From<HttpError> for Fault {
    fn from(error: HttpError) -> Self {
        Self::Http(error)
    }
}

impl From<CdpError> for Fault {
    fn from(_: CdpError) -> Self {
        Self::Other
    }
}

type Op<T> = Result<T, Fault>;

fn stopped_error() -> HttpError {
    HttpError::conflict("The computer session was stopped or replaced.")
}

fn invalid() -> HttpError {
    HttpError::bad_request(INVALID_REQUEST)
}

/// The browser page this session drives.
struct Page {
    chrome: Arc<Chrome>,
    session: String,
    target: String,
    frame: String,
    browser_context: String,
    events: broadcast::Sender<Event>,
}

impl Page {
    async fn send(&self, method: &str, params: Value) -> Op<Value> {
        Ok(self.chrome.send(Some(&self.session), method, params).await?)
    }
}

struct Session {
    epoch: String,
    policy: Arc<NetworkPolicy>,
    proxy: Arc<ComputerProxy>,
    abort: watch::Sender<bool>,
    sandboxed: bool,
    directory: Mutex<Option<PathBuf>>,
    chrome: Mutex<Option<Arc<Chrome>>>,
    page: OnceLock<Arc<Page>>,
    /// The managed page's target, known before it is attached.
    main_target: OnceLock<String>,
    /// Set when browser-wide auto-attach (popup closing) is being enabled.
    popups_guarded: std::sync::atomic::AtomicBool,
    /// Exact URLs allowed past the request guard while the launch is being verified.
    verifying: Mutex<HashSet<String>>,
    tail: tokio::sync::Mutex<()>,
    pending: AtomicUsize,
    navigation: AtomicU64,
    frame: Mutex<Option<ComputerFrame>>,
    url: Mutex<String>,
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
    cleanup: tokio::sync::OnceCell<()>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Session {
    fn aborted(&self) -> bool {
        *self.abort.borrow()
    }

    fn frame_id(&self) -> Option<String> {
        lock(&self.frame).as_ref().map(|frame| frame.id.clone())
    }

    fn visible_url(&self) -> String {
        let url = lock(&self.url).clone();
        if self.policy.allows(&url) { url } else { "about:blank".into() }
    }

    fn page(&self) -> Op<Arc<Page>> {
        self.page.get().cloned().ok_or(Fault::Other)
    }

    fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        let handle = tokio::spawn(task).abort_handle();
        let mut tasks = lock(&self.tasks);
        tasks.retain(|task| !task.is_finished());
        if self.aborted() {
            handle.abort();
        } else {
            tasks.push(handle);
        }
    }

    async fn cleanup(&self) {
        self.cleanup
            .get_or_init(|| async {
                self.proxy.stop();
                for task in std::mem::take(&mut *lock(&self.tasks)) {
                    task.abort();
                }
                let chrome = lock(&self.chrome).take();
                if let Some(chrome) = chrome {
                    chrome.close().await;
                }
                let directory = lock(&self.directory).take();
                if let Some(directory) = directory {
                    let _ = tokio::fs::remove_dir_all(directory).await;
                }
            })
            .await;
    }
}

/// `interruptible`: the work, unless the session stops first or 20 s pass.
async fn interruptible<T>(session: &Session, work: impl Future<Output = Op<T>>) -> Op<T> {
    let mut abort = session.abort.subscribe();
    if *abort.borrow() {
        return Err(stopped_error().into());
    }
    tokio::select! {
        biased;
        _ = abort.wait_for(|aborted| *aborted) => Err(stopped_error().into()),
        _ = tokio::time::sleep(OPERATION_TIMEOUT) => Err(HttpError::new(504, "The computer operation timed out.").into()),
        result = work => result,
    }
}

struct Inner {
    state: ComputerState,
    epoch: String,
    session: Option<Arc<Session>>,
    used_requests: HashSet<String>,
    error: Option<String>,
    stopping: Option<Shared<BoxFuture<'static, ComputerStatus>>>,
}

struct Core {
    options: ComputerOptions,
    inner: Mutex<Inner>,
}

/// The managed browser service. Cheap to clone; clones share one browser.
#[derive(Clone)]
pub struct ManagedComputer {
    core: Arc<Core>,
}

struct FrameRequest {
    request_id: String,
    epoch: String,
    frame_id: Option<String>,
}

#[derive(Clone, Debug)]
enum Action {
    Navigate(String),
    Click(f64, f64),
    Type(String),
    Key(&'static str),
    Scroll(i64),
    Observe,
}

fn object(input: &Value, keys: &[&str]) -> HttpResult<Map<String, Value>> {
    let object = input.as_object().ok_or_else(invalid)?;
    if object.keys().any(|key| !keys.contains(&key.as_str())) || keys.iter().any(|key| !object.contains_key(*key)) {
        return Err(invalid());
    }
    Ok(object.clone())
}

fn uuid_field(object: &Map<String, Value>, key: &str) -> HttpResult<String> {
    object.get(key).and_then(Value::as_str).filter(|value| is_uuid(value)).map(str::to_owned).ok_or_else(invalid)
}

fn frame_request(object: &Map<String, Value>) -> HttpResult<FrameRequest> {
    let frame_id = match object.get("frameId") {
        Some(Value::Null) => None,
        Some(Value::String(value)) if is_uuid(value) => Some(value.clone()),
        _ => return Err(invalid()),
    };
    Ok(FrameRequest { request_id: uuid_field(object, "requestId")?, epoch: uuid_field(object, "epoch")?, frame_id })
}

fn parse_action(input: &Value) -> HttpResult<Action> {
    let kind = input.get("type").and_then(Value::as_str).ok_or_else(invalid)?;
    Ok(match kind {
        "navigate" => {
            let action = object(input, &["type", "url"])?;
            let url = action["url"].as_str().filter(|url| utf16_len(url) <= 4096).ok_or_else(invalid)?;
            Action::Navigate(url.to_owned())
        }
        "click" => {
            let action = object(input, &["type", "x", "y"])?;
            let x = action["x"].as_f64().filter(|x| x.is_finite() && *x >= 0.0 && *x < f64::from(VIEWPORT_WIDTH)).ok_or_else(invalid)?;
            let y = action["y"].as_f64().filter(|y| y.is_finite() && *y >= 0.0 && *y < f64::from(VIEWPORT_HEIGHT)).ok_or_else(invalid)?;
            Action::Click(x, y)
        }
        "type" => {
            let action = object(input, &["type", "text"])?;
            let text = action["text"].as_str().ok_or_else(invalid)?;
            let control = |c: char| matches!(c as u32, 0..=8 | 0x0b | 0x0c | 0x0e..=0x1f | 0x7f);
            if !(1..=2000).contains(&utf16_len(text)) || text.chars().any(control) {
                return Err(invalid());
            }
            Action::Type(text.to_owned())
        }
        "key" => {
            let action = object(input, &["type", "key"])?;
            let key = action["key"].as_str().ok_or_else(invalid)?;
            Action::Key(COMPUTER_KEYS.iter().find(|candidate| **candidate == key).copied().ok_or_else(invalid)?)
        }
        "scroll" => {
            let action = object(input, &["type", "deltaY"])?;
            let delta = action["deltaY"].as_f64().filter(|d| d.fract() == 0.0 && (-1200.0..=1200.0).contains(d) && *d != 0.0).ok_or_else(invalid)?;
            Action::Scroll(delta as i64)
        }
        "observe" => {
            object(input, &["type"])?;
            Action::Observe
        }
        _ => return Err(invalid()),
    })
}

/// Playwright US keyboard layout entries for the allowed keys: (key, code, keyCode, text).
fn key_definition(key: &str) -> (&'static str, &'static str, i64, &'static str) {
    match key {
        "Enter" => ("Enter", "Enter", 13, "\r"),
        "Tab" => ("Tab", "Tab", 9, ""),
        "Escape" => ("Escape", "Escape", 27, ""),
        "Backspace" => ("Backspace", "Backspace", 8, ""),
        "Delete" => ("Delete", "Delete", 46, ""),
        "ArrowUp" => ("ArrowUp", "ArrowUp", 38, ""),
        "ArrowDown" => ("ArrowDown", "ArrowDown", 40, ""),
        "ArrowLeft" => ("ArrowLeft", "ArrowLeft", 37, ""),
        "ArrowRight" => ("ArrowRight", "ArrowRight", 39, ""),
        "Home" => ("Home", "Home", 36, ""),
        "End" => ("End", "End", 35, ""),
        "PageUp" => ("PageUp", "PageUp", 33, ""),
        "PageDown" => ("PageDown", "PageDown", 34, ""),
        _ => (" ", "Space", 32, " "),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct View {
    title: String,
    controls: Vec<ComputerControl>,
    text: String,
    text_truncated: bool,
}

#[derive(Deserialize)]
struct TargetCheck {
    editable: bool,
    blocked: bool,
}

impl ManagedComputer {
    pub fn new(options: ComputerOptions) -> HttpResult<Self> {
        if options.has_overrides() && !test_mode() {
            return Err(HttpError::forbidden("Browser test overrides are available only to tests."));
        }
        Ok(Self {
            core: Arc::new(Core {
                options,
                inner: Mutex::new(Inner { state: ComputerState::Stopped, epoch: new_id(), session: None, used_requests: HashSet::new(), error: None, stopping: None }),
            }),
        })
    }

    pub fn status(&self) -> ComputerStatus {
        self.core.status()
    }

    pub async fn start(&self, input: Value) -> HttpResult<ComputerStatus> {
        self.core.start(input).await
    }

    pub async fn stop(&self) -> ComputerStatus {
        self.core.terminate(None).await
    }

    pub async fn perform(&self, input: Value) -> HttpResult<ComputerObservation> {
        let object = object(&input, &["requestId", "epoch", "frameId", "action"])?;
        let request = frame_request(&object)?;
        let action = parse_action(&object["action"])?;
        let observe = matches!(action, Action::Observe);
        let core = Arc::clone(&self.core);
        self.core
            .enqueue(request, observe, move |session| {
                Box::pin(async move {
                    core.apply(&session, action).await?;
                    let (frame, _) = core.capture(&session).await?;
                    Ok(ComputerObservation { status: core.status(), frame })
                })
            })
            .await
    }

    pub async fn screenshot(&self, input: Value) -> HttpResult<ComputerScreenshot> {
        let object = object(&input, &["requestId", "epoch", "frameId"])?;
        let request = frame_request(&object)?;
        let core = Arc::clone(&self.core);
        self.core
            .enqueue(request, true, move |session| {
                Box::pin(async move {
                    let (frame, png) = core.capture(&session).await?;
                    Ok(ComputerScreenshot { status: core.status(), frame, png })
                })
            })
            .await
    }
}

impl ComputerService for ManagedComputer {
    fn status(&self) -> ComputerStatus {
        ManagedComputer::status(self)
    }
    fn start(&self, input: Value) -> BoxFuture<'_, HttpResult<ComputerStatus>> {
        Box::pin(ManagedComputer::start(self, input))
    }
    fn stop(&self) -> BoxFuture<'_, ComputerStatus> {
        Box::pin(ManagedComputer::stop(self))
    }
    fn perform(&self, input: Value) -> BoxFuture<'_, HttpResult<ComputerObservation>> {
        Box::pin(ManagedComputer::perform(self, input))
    }
    fn screenshot(&self, input: Value) -> BoxFuture<'_, HttpResult<ComputerScreenshot>> {
        Box::pin(ManagedComputer::screenshot(self, input))
    }
}

impl Core {
    fn inner(&self) -> MutexGuard<'_, Inner> {
        lock(&self.inner)
    }

    fn status_of(inner: &Inner) -> ComputerStatus {
        let session = inner.session.as_ref();
        let running = inner.state == ComputerState::Running;
        ComputerStatus {
            state: inner.state,
            epoch: inner.epoch.clone(),
            frame_id: if running { session.and_then(|s| s.frame_id()) } else { None },
            viewport: VIEWPORT,
            origins: match (inner.state, session) {
                (ComputerState::Running | ComputerState::Starting, Some(session)) => session.policy.origins.clone(),
                _ => Vec::new(),
            },
            url: if running { session.map(|s| s.visible_url()) } else { None },
            busy: matches!(inner.state, ComputerState::Starting | ComputerState::Stopping) || session.is_some_and(|s| s.pending.load(Ordering::SeqCst) > 0),
            sandboxed: running && session.is_some_and(|s| s.sandboxed),
            error: inner.error.clone(),
        }
    }

    fn status(&self) -> ComputerStatus {
        Self::status_of(&self.inner())
    }

    fn consume(&self, request_id: &str) -> HttpResult<()> {
        let id = request_id.to_lowercase();
        let mut inner = self.inner();
        if inner.used_requests.contains(&id) {
            return Err(HttpError::conflict("This computer request ID was already used. Requests are never replayed."));
        }
        if inner.used_requests.len() >= MAX_REQUESTS {
            return Err(HttpError::new(429, "This computer reached its request limit. Create a new service instance."));
        }
        inner.used_requests.insert(id);
        Ok(())
    }

    fn is_current(&self, session: &Arc<Session>) -> bool {
        self.inner().session.as_ref().is_some_and(|current| Arc::ptr_eq(current, session))
    }

    fn assert_live(&self, session: &Arc<Session>) -> Op<()> {
        if !self.is_current(session) || session.aborted() {
            return Err(stopped_error().into());
        }
        Ok(())
    }

    async fn start(self: &Arc<Self>, input: Value) -> HttpResult<ComputerStatus> {
        let request = object(&input, &["requestId", "userInitiated", "origins"])?;
        let request_id = uuid_field(&request, "requestId")?;
        if request.get("userInitiated") != Some(&Value::Bool(true)) {
            return Err(invalid());
        }
        let origins: Vec<String> = match request.get("origins") {
            Some(Value::Array(items)) if (1..=16).contains(&items.len()) => items.iter().map(|item| item.as_str().map(str::to_owned)).collect::<Option<_>>().ok_or_else(invalid)?,
            _ => return Err(invalid()),
        };
        self.consume(&request_id)?;
        let session = {
            let mut inner = self.inner();
            if inner.state != ComputerState::Stopped {
                return Err(HttpError::conflict("Stop the current computer before starting another."));
            }
            let policy = Arc::new(NetworkPolicy::new(&origins, &self.options.allow_test_origins, self.options.lookup.clone())?);
            let proxy = ComputerProxy::new(policy.clone());
            let session = Arc::new(Session {
                epoch: new_id(),
                policy,
                proxy,
                abort: watch::Sender::new(false),
                sandboxed: self.options.test_browser.as_ref().is_none_or(|browser| browser.chromium_sandbox),
                directory: Mutex::new(None),
                chrome: Mutex::new(None),
                page: OnceLock::new(),
                main_target: OnceLock::new(),
                popups_guarded: false.into(),
                verifying: Mutex::new(HashSet::new()),
                tail: tokio::sync::Mutex::new(()),
                pending: AtomicUsize::new(0),
                navigation: AtomicU64::new(0),
                frame: Mutex::new(None),
                url: Mutex::new("about:blank".into()),
                tasks: Mutex::new(Vec::new()),
                cleanup: tokio::sync::OnceCell::new(),
            });
            inner.session = Some(session.clone());
            inner.epoch = session.epoch.clone();
            inner.state = ComputerState::Starting;
            inner.error = None;
            session
        };
        match self.launch(&session).await {
            Ok(()) => Ok(self.status()),
            Err(fault) => {
                if session.aborted() {
                    return Err(stopped_error());
                }
                let error = match fault {
                    Fault::Http(error) => error,
                    Fault::Other => HttpError::unavailable(LAUNCH_FAILED),
                };
                self.terminate(Some(error.message.clone())).await;
                Err(error)
            }
        }
    }

    async fn launch(self: &Arc<Self>, session: &Arc<Session>) -> Op<()> {
        for origin in &session.policy.origins {
            interruptible(session, async { Ok(session.policy.resolve(origin).await?) }).await?;
        }
        interruptible(session, async { Ok(session.proxy.start().await?) }).await?;
        // A short base keeps Chrome's singleton socket path (inside the profile, which is also its
        // TMPDIR) under the Unix socket limit; the engine points this process's TMPDIR deep into
        // the data directory.
        let base = if std::path::Path::new("/tmp").is_dir() { PathBuf::from("/tmp") } else { std::env::temp_dir() };
        let directory = tempfile::Builder::new().prefix("grok-managed-computer-").tempdir_in(base).map_err(|_| Fault::Other)?.keep();
        *lock(&session.directory) = Some(directory.clone());
        self.assert_live(session)?;
        let proxy = session.proxy.configuration()?;
        let executable = self.options.test_browser.as_ref().and_then(|browser| browser.executable_path.clone()).unwrap_or_else(cdp::chrome_executable);
        let mut arguments: Vec<String> = LAUNCH_ARGUMENTS.iter().chain(PROTECTION_ARGUMENTS.iter()).map(|a| (*a).to_owned()).collect();
        if !session.sandboxed {
            arguments.push("--no-sandbox".into());
        }
        arguments.push(format!("--proxy-server={}", proxy.server));
        arguments.push(format!("--user-data-dir={}", directory.display()));
        arguments.push("--remote-debugging-pipe".into());
        arguments.push("--no-startup-window".into());
        let (chrome, events) = cdp::launch(&executable, &arguments, &directory).map_err(|_| Fault::Other)?;
        let chrome = Arc::new(chrome);
        *lock(&session.chrome) = Some(chrome.clone());
        self.assert_live(session)?;
        let (page_events, _) = broadcast::channel(4096);
        self.listen(session, events, page_events.clone());
        interruptible(session, async { tokio::time::timeout(LAUNCH_TIMEOUT, chrome.send(None, "Browser.getVersion", json!({}))).await.map_err(|_| Fault::Other)?.map_err(Fault::from) }).await?;
        let context = interruptible(session, async { Ok(chrome.send(None, "Target.createBrowserContext", json!({ "disposeOnDetach": true })).await?) }).await?;
        let browser_context = context["browserContextId"].as_str().ok_or(Fault::Other)?.to_owned();
        let created = interruptible(session, async { Ok(chrome.send(None, "Target.createTarget", json!({ "url": "about:blank", "browserContextId": browser_context })).await?) }).await?;
        let target = created["targetId"].as_str().ok_or(Fault::Other)?.to_owned();
        session.main_target.set(target.clone()).map_err(|_| Fault::Other)?;
        let attached = interruptible(session, async { Ok(chrome.send(None, "Target.attachToTarget", json!({ "targetId": target, "flatten": true })).await?) }).await?;
        let page_session = attached["sessionId"].as_str().ok_or(Fault::Other)?.to_owned();
        let tree = interruptible(session, async { Ok(chrome.send(Some(&page_session), "Page.getFrameTree", json!({})).await?) }).await?;
        let frame = tree["frameTree"]["frame"]["id"].as_str().ok_or(Fault::Other)?.to_owned();
        let page = Arc::new(Page { chrome: chrome.clone(), session: page_session, target, frame, browser_context, events: page_events });
        if session.page.set(page.clone()).is_err() {
            return Err(Fault::Other);
        }
        interruptible(session, self.prepare_page(session, &page)).await?;
        self.verify_launch(session, &page).await?;
        interruptible(session, self.install_guards(session, &page)).await?;
        interruptible(session, async { goto(&page, "about:blank", NAVIGATION_TIMEOUT).await.map(|_| ()) }).await?;
        self.assert_live(session)?;
        {
            let mut inner = self.inner();
            if !inner.session.as_ref().is_some_and(|current| Arc::ptr_eq(current, session)) || session.aborted() {
                return Err(stopped_error().into());
            }
            inner.state = ComputerState::Running;
        }
        interruptible(session, self.capture(session)).await?;
        Ok(())
    }

    /// Context options of the Node launch: viewport, locale, timezone, reduced motion, downloads
    /// denied, service workers bypassed, request interception with proxy authentication.
    async fn prepare_page(&self, _session: &Arc<Session>, page: &Arc<Page>) -> Op<()> {
        let chrome = &page.chrome;
        chrome.send(None, "Browser.setDownloadBehavior", json!({ "behavior": "deny", "browserContextId": page.browser_context, "eventsEnabled": false })).await?;
        page.send("Page.enable", json!({})).await?;
        page.send("Page.setLifecycleEventsEnabled", json!({ "enabled": true })).await?;
        page.send("Network.enable", json!({})).await?;
        page.send("Network.setBypassServiceWorker", json!({ "bypass": true })).await?;
        page.send("Fetch.enable", json!({ "patterns": [{ "urlPattern": "*", "requestStage": "Request" }], "handleAuthRequests": true })).await?;
        page.send(
            "Emulation.setDeviceMetricsOverride",
            json!({ "width": VIEWPORT_WIDTH, "height": VIEWPORT_HEIGHT, "deviceScaleFactor": 1, "mobile": false, "screenWidth": VIEWPORT_WIDTH, "screenHeight": VIEWPORT_HEIGHT }),
        )
        .await?;
        page.send("Emulation.setFocusEmulationEnabled", json!({ "enabled": true })).await?;
        page.send("Emulation.setLocaleOverride", json!({ "locale": "en-US" })).await?;
        page.send("Emulation.setTimezoneOverride", json!({ "timezoneId": "UTC" })).await?;
        page.send("Emulation.setEmulatedMedia", json!({ "features": [{ "name": "prefers-reduced-motion", "value": "reduce" }] })).await?;
        page.send("Page.setInterceptFileChooserDialog", json!({ "enabled": true })).await?;
        Ok(())
    }

    async fn verify_launch(&self, session: &Arc<Session>, page: &Arc<Page>) -> Op<()> {
        let chrome = &page.chrome;
        let line = interruptible(session, async { Ok(chrome.send(None, "Browser.getBrowserCommandLine", json!({})).await?) }).await?;
        let arguments: Vec<&str> = line["arguments"].as_array().map(|items| items.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let server = format!("--proxy-server={}", session.proxy.configuration()?.server);
        if !PROTECTION_ARGUMENTS.iter().all(|flag| arguments.contains(flag))
            || !arguments.contains(&server.as_str())
            || arguments.iter().filter(|arg| arg.starts_with("--proxy-bypass-list=")).any(|arg| *arg != "--proxy-bypass-list=<-loopback>")
            || (session.sandboxed && arguments.iter().any(|arg| UNSANDBOXED_ARGUMENTS.contains(arg)))
        {
            return Err(HttpError::unavailable("Chrome did not enable the required sandbox and network protections.").into());
        }
        interruptible(session, async { Ok(chrome.send(None, "Browser.setDownloadBehavior", json!({ "behavior": "deny", "eventsEnabled": false })).await?) }).await?;
        if session.sandboxed && cfg!(target_os = "linux") {
            interruptible(session, goto(page, "chrome://sandbox", Duration::from_secs(8))).await?;
            let adequate = interruptible(session, async {
                for _ in 0..40 {
                    let report = evaluate(page, scripts::BODY_TEXT).await?;
                    let report = report.as_str().unwrap_or_default();
                    if report.contains("You are adequately sandboxed.") && seccomp_enabled(report) {
                        return Ok(true);
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Ok(false)
            })
            .await?;
            if !adequate {
                return Err(HttpError::unavailable("Chrome could not verify an active Linux sandbox.").into());
            }
        }
        let probe = session.proxy.probe_url()?;
        let invalid_name = format!("http://unsupported-dns-{}.invalid/", new_id());
        lock(&session.verifying).extend([probe.clone(), invalid_name.clone()]);
        interruptible(session, goto(page, &probe, Duration::from_secs(8))).await?;
        let body = interruptible(session, body_text(page)).await?;
        if body != session.proxy.probe_body() {
            return Err(HttpError::unavailable("Chrome did not use the mandatory loopback proxy.").into());
        }
        let dns = HttpError::unavailable("Chrome did not apply the mandatory proxy DNS policy.");
        match interruptible(session, goto(page, &invalid_name, Duration::from_secs(4))).await {
            Ok(_) => {}
            Err(Fault::Http(error)) if error.status == 409 => return Err(error.into()),
            Err(_) => return Err(dns.into()),
        }
        let denied = interruptible(session, body_text(page)).await?;
        if denied != DENIED_BODY {
            return Err(dns.into());
        }
        lock(&session.verifying).clear();
        Ok(())
    }

    async fn install_guards(&self, session: &Arc<Session>, page: &Arc<Page>) -> Op<()> {
        let init = scripts::init(&session.policy.origins);
        page.send("Page.addScriptToEvaluateOnNewDocument", json!({ "source": init })).await?;
        // Child frames and workers start paused and run only after their own guards are installed;
        // other pages (popups) are closed before they run.
        page.send("Target.setAutoAttach", json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true })).await?;
        session.popups_guarded.store(true, Ordering::SeqCst);
        page.chrome.send(None, "Target.setAutoAttach", json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true })).await?;
        Ok(())
    }

    /// Dispatch browser events for this session until the browser disconnects.
    fn listen(self: &Arc<Self>, session: &Arc<Session>, mut events: mpsc::UnboundedReceiver<Event>, page_events: broadcast::Sender<Event>) {
        let core = Arc::downgrade(self);
        let weak = Arc::downgrade(session);
        let mut closed = page_events.subscribe();
        session.spawn(async move {
            while let Some(event) = events.recv().await {
                let Some(session) = weak.upgrade() else { return };
                handle_event(&session, &event, &page_events);
                drop(session);
            }
            if let (Some(core), Some(session)) = (core.upgrade(), weak.upgrade())
                && core.is_current(&session)
                && !session.aborted()
            {
                drop(core.terminate(Some("The managed Chrome process disconnected.".into())));
            }
        });
        let core = Arc::downgrade(self);
        let weak = Arc::downgrade(session);
        session.spawn(async move {
            loop {
                match closed.recv().await {
                    Ok(event) if event.method == "Page.closed" => break,
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
            if let (Some(core), Some(session)) = (core.upgrade(), weak.upgrade())
                && core.is_current(&session)
                && !session.aborted()
            {
                drop(core.terminate(Some("The managed computer page closed.".into())));
            }
        });
    }

    fn terminate(self: &Arc<Self>, error: Option<String>) -> Shared<BoxFuture<'static, ComputerStatus>> {
        let mut inner = self.inner();
        if let Some(stopping) = &inner.stopping {
            return stopping.clone();
        }
        let Some(session) = inner.session.clone() else {
            let status = Self::status_of(&inner);
            return futures::future::ready(status).boxed().shared();
        };
        inner.state = ComputerState::Stopping;
        inner.epoch = new_id();
        inner.error = error;
        *lock(&session.frame) = None;
        session.abort.send_replace(true);
        let core = Arc::clone(self);
        let stopping = async move {
            session.cleanup().await;
            let mut inner = core.inner();
            if inner.session.as_ref().is_some_and(|current| Arc::ptr_eq(current, &session)) {
                inner.session = None;
                inner.state = ComputerState::Stopped;
            }
            inner.stopping = None;
            Self::status_of(&inner)
        }
        .boxed()
        .shared();
        inner.stopping = Some(stopping.clone());
        // Cleanup completes even when no caller waits for it.
        tokio::spawn(stopping.clone());
        stopping
    }

    async fn enqueue<T: Send + 'static>(self: &Arc<Self>, request: FrameRequest, observe: bool, work: impl FnOnce(Arc<Session>) -> BoxFuture<'static, Op<T>> + Send + 'static) -> HttpResult<T> {
        self.consume(&request.request_id)?;
        let session = {
            let inner = self.inner();
            match &inner.session {
                Some(session) if inner.state == ComputerState::Running && request.epoch == session.epoch => session.clone(),
                _ => return Err(stopped_error()),
            }
        };
        if request.frame_id != session.frame_id() {
            return Err(HttpError::conflict("The computer frame is stale. Use the latest status and screenshot."));
        }
        if request.frame_id.is_none() && !observe {
            return Err(HttpError::conflict("Observe the computer before performing another action."));
        }
        if session.pending.load(Ordering::SeqCst) >= MAX_PENDING {
            return Err(HttpError::new(429, "Too many computer operations are queued."));
        }
        struct Pending(Arc<Session>);
        impl Drop for Pending {
            fn drop(&mut self) {
                self.0.pending.fetch_sub(1, Ordering::SeqCst);
            }
        }
        session.pending.fetch_add(1, Ordering::SeqCst);
        let _pending = Pending(session.clone());
        let core = Arc::clone(self);
        let queued = session.clone();
        // The queued operation owns its turn until it settles, even when the caller stops waiting.
        let task = tokio::spawn(async move {
            let session = queued;
            let _turn = session.tail.lock().await;
            core.assert_live(&session)?;
            if request.frame_id != session.frame_id() {
                return Err(HttpError::conflict("The computer frame is stale. Observe the current frame before acting.").into());
            }
            *lock(&session.frame) = None;
            let result = interruptible(&session, work(session.clone())).await;
            if result.is_err() {
                *lock(&session.frame) = None;
            }
            result
        });
        let result = interruptible(&session, async { task.await.unwrap_or(Err(Fault::Other)) }).await;
        result.map_err(|fault| {
            if session.aborted() {
                return stopped_error();
            }
            match fault {
                Fault::Http(error) => error,
                Fault::Other => HttpError::new(502, "The computer operation failed. Stop and restart the computer if no current frame is available."),
            }
        })
    }

    async fn target(&self, session: &Arc<Session>, point: Option<(f64, f64)>) -> Op<TargetCheck> {
        let page = session.page()?;
        let value = interruptible(session, evaluate(&page, &scripts::target(point, &session.policy.origins))).await?;
        serde_json::from_value(value).map_err(|_| Fault::Other)
    }

    async fn apply(&self, session: &Arc<Session>, action: Action) -> Op<()> {
        let page = session.page()?;
        match action {
            Action::Navigate(url) => {
                let target = interruptible(session, async { Ok(session.policy.resolve(&url).await?) }).await?;
                let status = interruptible(session, goto(&page, target.url.as_str(), NAVIGATION_TIMEOUT)).await?;
                if !session.policy.allows(&lock(&session.url).clone()) {
                    return Err(HttpError::forbidden("Navigation left the selected public origins.").into());
                }
                if status.is_some_and(|status| status >= 400) {
                    return Err(HttpError::new(502, "The destination returned an error or was blocked by the network policy.").into());
                }
            }
            Action::Click(x, y) => {
                let target = self.target(session, Some((x, y))).await?;
                if target.blocked {
                    return Err(HttpError::forbidden("Credentials, uploads, downloads, popups, and unapproved destinations are blocked.").into());
                }
                interruptible(session, click(&page, x, y)).await?;
            }
            Action::Type(text) => {
                let target = self.target(session, None).await?;
                if !target.editable || target.blocked {
                    return Err(HttpError::forbidden("Typing requires a focused non-sensitive text control.").into());
                }
                interruptible(session, async { page.send("Input.insertText", json!({ "text": text })).await.map(|_| ()) }).await?;
            }
            Action::Key(key) => {
                let target = self.target(session, None).await?;
                if target.blocked && !["Tab", "Shift+Tab", "Escape"].contains(&key) {
                    return Err(HttpError::forbidden("Keyboard input to sensitive controls is blocked.").into());
                }
                interruptible(session, press(&page, key)).await?;
            }
            Action::Scroll(delta) => {
                let (x, y) = (f64::from(VIEWPORT_WIDTH) / 2.0, f64::from(VIEWPORT_HEIGHT) / 2.0);
                interruptible(session, async {
                    page.send("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y, "button": "none" })).await?;
                    page.send("Input.dispatchMouseEvent", json!({ "type": "mouseWheel", "x": x, "y": y, "deltaX": 0, "deltaY": delta })).await?;
                    Ok(())
                })
                .await?;
            }
            Action::Observe => {}
        }
        Ok(())
    }

    async fn capture(&self, session: &Arc<Session>) -> Op<(ComputerFrame, Vec<u8>)> {
        self.assert_live(session)?;
        let page = session.page()?;
        for _ in 0..3 {
            let navigation = session.navigation.load(Ordering::SeqCst);
            let view = interruptible(session, evaluate(&page, &scripts::capture(&session.policy.origins, PAGE_TEXT_LIMIT * 2, PAGE_TEXT_NODE_LIMIT))).await?;
            let view: View = serde_json::from_value(view).map_err(|_| Fault::Other)?;
            let png = interruptible(session, screenshot(&page)).await?;
            self.assert_live(session)?;
            if navigation != session.navigation.load(Ordering::SeqCst) {
                continue;
            }
            let (page_text, cut) = bounded_text(&view.text, PAGE_TEXT_LIMIT);
            let frame = ComputerFrame {
                id: new_id(),
                epoch: session.epoch.clone(),
                url: session.visible_url(),
                title: safe_label(&view.title, 200),
                viewport: VIEWPORT,
                controls: view.controls,
                mime_type: "image/png".into(),
                page_text,
                text_truncated: view.text_truncated || cut,
            };
            *lock(&session.frame) = Some(frame.clone());
            return Ok((frame, png));
        }
        Err(HttpError::conflict("The page changed during observation. Stop and restart the computer before acting.").into())
    }
}

/// `/Seccomp-BPF sandbox\s+Yes/`
fn seccomp_enabled(report: &str) -> bool {
    report.match_indices("Seccomp-BPF sandbox").any(|(index, label)| {
        let rest = &report[index + label.len()..];
        let trimmed = rest.trim_start();
        trimmed.len() < rest.len() && trimmed.starts_with("Yes")
    })
}

fn handle_event(session: &Arc<Session>, event: &Event, page_events: &broadcast::Sender<Event>) {
    let page = session.page.get().cloned();
    let on_page = page.as_ref().is_some_and(|page| event.session.as_deref() == Some(page.session.as_str()));
    match event.method.as_str() {
        "Fetch.requestPaused" => {
            let (session_ref, event) = (Arc::clone(session), event.clone());
            session.spawn(async move { guard_request(&session_ref, &event).await });
        }
        "Fetch.authRequired" => {
            let (session_ref, event) = (Arc::clone(session), event.clone());
            session.spawn(async move { authenticate(&session_ref, &event).await });
        }
        "Target.attachedToTarget" => {
            let (session_ref, event) = (Arc::clone(session), event.clone());
            session.spawn(async move { attached(&session_ref, &event).await });
        }
        "Page.javascriptDialogOpening" => {
            if let Some(chrome) = lock(&session.chrome).clone() {
                let target = event.session.clone();
                session.spawn(async move {
                    let _ = chrome.send(target.as_deref(), "Page.handleJavaScriptDialog", json!({ "accept": false })).await;
                });
            }
        }
        "Target.detachedFromTarget" | "Target.targetDestroyed" | "Target.targetCrashed" => {
            if let Some(page) = &page {
                let closed = if event.method == "Target.detachedFromTarget" {
                    event.session.is_none() && event.params["sessionId"].as_str() == Some(page.session.as_str())
                } else {
                    event.params["targetId"].as_str() == Some(page.target.as_str())
                };
                if closed {
                    let _ = page_events.send(Event { method: "Page.closed".into(), params: Value::Null, session: None });
                }
            }
        }
        "Inspector.detached" | "Inspector.targetCrashed" if on_page => {
            let _ = page_events.send(Event { method: "Page.closed".into(), params: Value::Null, session: None });
        }
        "Page.frameNavigated" if on_page => {
            let frame = &event.params["frame"];
            if frame.get("parentId").is_none() {
                let url = format!("{}{}", frame["url"].as_str().unwrap_or("about:blank"), frame["urlFragment"].as_str().unwrap_or(""));
                *lock(&session.url) = url;
                session.navigation.fetch_add(1, Ordering::SeqCst);
                *lock(&session.frame) = None;
            }
        }
        "Page.navigatedWithinDocument" if on_page && page.as_ref().is_some_and(|page| event.params["frameId"].as_str() == Some(page.frame.as_str())) => {
            *lock(&session.url) = event.params["url"].as_str().unwrap_or("about:blank").to_owned();
            session.navigation.fetch_add(1, Ordering::SeqCst);
            *lock(&session.frame) = None;
        }
        _ => {}
    }
    if on_page {
        let _ = page_events.send(event.clone());
    }
}

/// `context.route('**/*')` of the Node implementation, for the page and every child target.
async fn guard_request(session: &Arc<Session>, event: &Event) {
    let Some(chrome) = lock(&session.chrome).clone() else { return };
    let params = &event.params;
    let request_id = params["requestId"].as_str().unwrap_or_default().to_owned();
    let request = &params["request"];
    let url = request["url"].as_str().unwrap_or_default();
    let target = event.session.as_deref();
    let page = session.page.get().cloned();
    let fail = || chrome.send(target, "Fetch.failRequest", json!({ "requestId": request_id, "errorReason": "BlockedByClient" }));
    if !session.aborted() && lock(&session.verifying).contains(url) {
        let _ = chrome.send(target, "Fetch.continueRequest", json!({ "requestId": request_id })).await;
        return;
    }
    let method = request["method"].as_str().unwrap_or_default();
    let kind = params["resourceType"].as_str().unwrap_or_default();
    let main_frame = page.as_ref().is_some_and(|page| target == Some(page.session.as_str()) && params["frameId"].as_str() == Some(page.frame.as_str()));
    let blocked = session.aborted()
        || !session.policy.allows(url)
        || !matches!(method, "GET" | "HEAD" | "OPTIONS")
        || request["hasPostData"].as_bool().unwrap_or(false)
        || request.get("postData").is_some()
        || matches!(kind, "WebSocket" | "Other")
        || (kind == "Document" && !main_frame);
    if blocked || session.policy.resolve(url).await.is_err() || session.aborted() {
        let _ = fail().await;
        return;
    }
    let headers: Vec<Value> = request["headers"]
        .as_object()
        .map(|headers| {
            headers
                .iter()
                .filter(|(name, _)| !["authorization", "cookie", "proxy-authorization"].contains(&name.to_ascii_lowercase().as_str()))
                .map(|(name, value)| json!({ "name": name, "value": value.as_str().unwrap_or_default() }))
                .collect()
        })
        .unwrap_or_default();
    let _ = chrome.send(target, "Fetch.continueRequest", json!({ "requestId": request_id, "headers": headers })).await;
}

/// Answer proxy authentication challenges with this session's credentials; refuse site challenges.
async fn authenticate(session: &Arc<Session>, event: &Event) {
    let Some(chrome) = lock(&session.chrome).clone() else { return };
    let request_id = event.params["requestId"].as_str().unwrap_or_default();
    let response = match session.proxy.configuration() {
        Ok(proxy) if event.params["authChallenge"]["source"].as_str() == Some("Proxy") && !session.aborted() => {
            json!({ "response": "ProvideCredentials", "username": proxy.username, "password": proxy.password })
        }
        _ => json!({ "response": "CancelAuth" }),
    };
    let _ = chrome.send(event.session.as_deref(), "Fetch.continueWithAuth", json!({ "requestId": request_id, "authChallengeResponse": response })).await;
}

/// New targets start paused. Pages other than the managed page are closed without running;
/// frames and workers run only after the request guard (and, for frames, the init script) is in
/// place; other kinds (service and shared workers) stay paused.
async fn attached(session: &Arc<Session>, event: &Event) {
    let Some(chrome) = lock(&session.chrome).clone() else { return };
    let child = event.params["sessionId"].as_str().unwrap_or_default().to_owned();
    let info = &event.params["targetInfo"];
    let target = info["targetId"].as_str().unwrap_or_default().to_owned();
    let kind = info["type"].as_str().unwrap_or_default();
    let main = session.main_target.get().is_some_and(|main| *main == target);
    match kind {
        // The managed page's own attachment (before browser-wide auto-attach) and its duplicate.
        "page" if main => {
            let own = session.page.get().is_none_or(|page| page.session == child);
            if session.popups_guarded.load(Ordering::SeqCst) && !own {
                let _ = chrome.send(None, "Target.detachFromTarget", json!({ "sessionId": child })).await;
            }
        }
        "page" => {
            let _ = chrome.send(None, "Target.closeTarget", json!({ "targetId": target })).await;
        }
        "iframe" | "worker" => {
            let child = Some(child.as_str());
            let mut ready = chrome.send(child, "Fetch.enable", json!({ "patterns": [{ "urlPattern": "*", "requestStage": "Request" }], "handleAuthRequests": true })).await.is_ok();
            if kind == "iframe" {
                ready = ready
                    && chrome.send(child, "Page.enable", json!({})).await.is_ok()
                    && chrome.send(child, "Page.addScriptToEvaluateOnNewDocument", json!({ "source": scripts::init(&session.policy.origins) })).await.is_ok()
                    && chrome.send(child, "Network.setBypassServiceWorker", json!({ "bypass": true })).await.is_ok()
                    && chrome.send(child, "Target.setAutoAttach", json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true })).await.is_ok();
            }
            if ready && !session.aborted() {
                let _ = chrome.send(child, "Runtime.runIfWaitingForDebugger", json!({})).await;
            }
        }
        _ => {}
    }
}

/// `page.goto(url, { waitUntil: 'domcontentloaded' })`: the main document's HTTP status.
async fn goto(page: &Page, url: &str, timeout: Duration) -> Op<Option<u16>> {
    let mut events = page.events.subscribe();
    let work = async {
        let result = page.send("Page.navigate", json!({ "url": url })).await?;
        if result["errorText"].as_str().is_some_and(|text| !text.is_empty()) {
            return Err(Fault::Other);
        }
        let Some(loader) = result["loaderId"].as_str().map(str::to_owned) else {
            return Ok(None);
        };
        let mut status = None;
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return Err(Fault::Other),
            };
            let params = &event.params;
            match event.method.as_str() {
                "Network.responseReceived" if params["requestId"].as_str() == Some(loader.as_str()) => {
                    status = params["response"]["status"].as_u64().map(|s| s as u16);
                }
                "Page.lifecycleEvent" if params["frameId"].as_str() == Some(page.frame.as_str()) && params["loaderId"].as_str() == Some(loader.as_str()) && params["name"] == "DOMContentLoaded" => {
                    return Ok(status);
                }
                "Page.frameNavigated" if params["frame"].get("parentId").is_none() && params["frame"]["loaderId"].as_str().is_some_and(|id| id != loader) => {
                    return Err(Fault::Other);
                }
                "Page.closed" => return Err(Fault::Other),
                _ => {}
            }
        }
    };
    tokio::time::timeout(timeout, work).await.unwrap_or(Err(Fault::Other))
}

/// Evaluate an expression in a fresh isolated world of the main frame.
async fn evaluate(page: &Page, expression: &str) -> Op<Value> {
    let world = page.send("Page.createIsolatedWorld", json!({ "frameId": page.frame, "worldName": ISOLATED_WORLD, "grantUniveralAccess": false })).await?;
    let context = world["executionContextId"].as_i64().ok_or(Fault::Other)?;
    let result = page.send("Runtime.evaluate", json!({ "expression": expression, "contextId": context, "returnByValue": true, "awaitPromise": true })).await?;
    if result.get("exceptionDetails").is_some() {
        return Err(Fault::Other);
    }
    Ok(result["result"]["value"].clone())
}

async fn body_text(page: &Page) -> Op<String> {
    for _ in 0..50 {
        if let Value::String(text) = evaluate(page, scripts::BODY_TEXT).await? {
            return Ok(text);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(Fault::Other)
}

async fn screenshot(page: &Page) -> Op<Vec<u8>> {
    let result = tokio::time::timeout(Duration::from_secs(6), page.send("Page.captureScreenshot", json!({ "format": "png", "captureBeyondViewport": false, "fromSurface": true })))
        .await
        .map_err(|_| Fault::Other)??;
    let data = result["data"].as_str().ok_or(Fault::Other)?;
    base64::engine::general_purpose::STANDARD.decode(data).map_err(|_| Fault::Other)
}

async fn click(page: &Page, x: f64, y: f64) -> Op<()> {
    page.send("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y, "button": "none" })).await?;
    page.send("Input.dispatchMouseEvent", json!({ "type": "mousePressed", "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1 })).await?;
    page.send("Input.dispatchMouseEvent", json!({ "type": "mouseReleased", "x": x, "y": y, "button": "left", "buttons": 0, "clickCount": 1 })).await?;
    Ok(())
}

async fn press(page: &Page, key: &str) -> Op<()> {
    let (name, shift) = match key.strip_prefix("Shift+") {
        Some(rest) => (rest, true),
        None => (key, false),
    };
    let modifiers = if shift { 8 } else { 0 };
    if shift {
        page.send("Input.dispatchKeyEvent", json!({ "type": "rawKeyDown", "key": "Shift", "code": "ShiftLeft", "windowsVirtualKeyCode": 16, "modifiers": 8 })).await?;
    }
    let (key, code, key_code, text) = key_definition(name);
    let down = if text.is_empty() {
        json!({ "type": "rawKeyDown", "key": key, "code": code, "windowsVirtualKeyCode": key_code, "modifiers": modifiers })
    } else {
        json!({ "type": "keyDown", "key": key, "code": code, "windowsVirtualKeyCode": key_code, "modifiers": modifiers, "text": text, "unmodifiedText": text })
    };
    page.send("Input.dispatchKeyEvent", down).await?;
    page.send("Input.dispatchKeyEvent", json!({ "type": "keyUp", "key": key, "code": code, "windowsVirtualKeyCode": key_code, "modifiers": modifiers })).await?;
    if shift {
        page.send("Input.dispatchKeyEvent", json!({ "type": "keyUp", "key": "Shift", "code": "ShiftLeft", "windowsVirtualKeyCode": 16, "modifiers": 0 })).await?;
    }
    Ok(())
}
