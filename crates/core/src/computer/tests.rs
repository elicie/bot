//! Port of `tests/computer.test.ts` and `tests/computer-api.test.ts`. The browser tests drive the
//! installed Google Chrome with its sandbox; they are skipped when Chrome is not installed.

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use futures::future::BoxFuture;
use grok_bot_model::{BotInput, HttpError, HttpResult, new_id};
use grok_bot_store::Store;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::network::{Destination, NetworkPolicy, Resolved, is_global_address};
use super::proxy::{ComputerProxy, authority_is_valid};
use super::*;

// ---------------------------------------------------------------- fixture HTTP servers

#[derive(Clone)]
struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }
}

enum Reply {
    Raw(String),
    Hang,
}

fn html(body: &str, extra: &str) -> Reply {
    Reply::Raw(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()))
}

fn text(status: &str, extra: &str, body: &str) -> Reply {
    Reply::Raw(format!("HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()))
}

struct Site {
    origin: String,
    requests: Arc<Mutex<Vec<Request>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Site {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Site {
    fn hits(&self) -> Vec<String> {
        self.requests.lock().unwrap().iter().map(|r| format!("{} {}", r.method, r.path)).collect()
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8_lossy(&buffer).into_owned();
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, path) = (first.next()?.to_owned(), first.next()?.to_owned());
    let headers = lines.take_while(|line| !line.is_empty()).filter_map(|line| line.split_once(':').map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))).collect();
    Some(Request { method, path, headers })
}

async fn site(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Site {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (recorded, handler) = (requests.clone(), Arc::new(handler));
    let task = tokio::spawn(async move {
        let mut hanging = Vec::new();
        while let Ok((mut stream, _)) = listener.accept().await {
            let (recorded, handler) = (recorded.clone(), handler.clone());
            hanging.push(tokio::spawn(async move {
                let Some(request) = read_request(&mut stream).await else { return };
                recorded.lock().unwrap().push(request.clone());
                match handler(&request) {
                    Reply::Raw(response) => {
                        let _ = stream.write_all(response.as_bytes()).await;
                        let _ = stream.shutdown().await;
                    }
                    Reply::Hang => {
                        let mut sink = [0u8; 64];
                        while matches!(stream.read(&mut sink).await, Ok(n) if n > 0) {}
                    }
                }
            }));
        }
    });
    Site { origin, requests, task }
}

// ---------------------------------------------------------------- network policy

fn denied(status: u16) -> impl Fn(&HttpError) -> bool {
    move |error| error.status == status
}

#[test]
fn network_policy_rejects_non_global_ipv4_ipv6_mapped_and_ambiguous_addresses() {
    for address in [
        "0.0.0.0", "10.1.2.3", "100.64.0.1", "127.0.0.1", "169.254.169.254", "172.16.1.1", "192.168.1.1", "192.0.2.1", "198.18.0.1", "198.51.100.1", "203.0.113.1", "224.0.0.1",
        "255.255.255.255", "::", "::1", "::ffff:127.0.0.1", "::ffff:7f00:1", "::ffff:10.0.0.1", "::ffff:8.8.8.8", "fc00::1", "fe80::1", "fe80::1%lo", "ff02::1", "64:ff9b::7f00:1",
        "2001:db8::1", "2002:7f00:1::1", "3fff::1", "127.1", "2130706433", "not-an-address",
    ] {
        assert!(!is_global_address(address), "{address}");
    }
    for address in ["1.1.1.1", "8.8.8.8", "93.184.216.34", "2606:4700:4700::1111", "2001:4860:4860::8888"] {
        assert!(is_global_address(address), "{address}");
    }
}

#[tokio::test]
async fn dns_validates_every_answer_and_revalidates_instead_of_trusting_an_earlier_lookup() {
    let answers: Arc<Mutex<Vec<IpAddr>>> = Arc::new(Mutex::new(vec!["93.184.216.34".parse().unwrap()]));
    let lookups = Arc::new(AtomicUsize::new(0));
    let (shared, counter) = (answers.clone(), lookups.clone());
    let lookup: network::Lookup = Arc::new(move |_host: String| {
        counter.fetch_add(1, Ordering::SeqCst);
        let answers = shared.lock().unwrap().clone();
        Box::pin(async move { Ok(answers) })
    });
    let policy = NetworkPolicy::new(&["https://public.example".into()], &[], Some(lookup)).unwrap();
    assert_eq!(policy.resolve("https://public.example/path").await.unwrap().address.to_string(), "93.184.216.34");
    for next in [vec!["93.184.216.34", "127.0.0.1"], vec!["10.0.0.1"], vec!["::ffff:127.0.0.1"], vec![]] {
        *answers.lock().unwrap() = next.iter().map(|a| a.parse().unwrap()).collect();
        assert!(policy.resolve("https://public.example/path").await.is_err_and(|e| denied(403)(&e)), "{next:?}");
    }
    assert_eq!(lookups.load(Ordering::SeqCst), 5);
    for url in ["http://public.example/path", "https://user:secret@public.example/path", "file:///etc/passwd"] {
        assert!(policy.resolve(url).await.is_err_and(|e| e.status == 403), "{url}");
    }
    let numeric = NetworkPolicy::new(&["http://127.1".into()], &[], None).unwrap();
    assert!(numeric.resolve("http://127.1/").await.is_err_and(|e| e.status == 403));
}

#[tokio::test]
async fn fixture_exceptions_are_loopback_only_origin_specific_and_never_an_action_input() {
    assert!(NetworkPolicy::new(&["https://example.com/path".into()], &[], None).is_err_and(|e| e.status == 400));
    assert!(NetworkPolicy::new(&["http://fixture.example".into()], &["http://fixture.example".into()], None).is_err_and(|e| e.status == 400));
    assert!(NetworkPolicy::new(&["http://127.0.0.1".into()], &["http://127.0.0.1".into()], None).is_err_and(|e| e.status == 400));
    let policy = NetworkPolicy::new(&["http://127.0.0.1:12345".into(), "http://127.0.0.1:12346".into()], &["http://127.0.0.1:12345".into()], None).unwrap();
    assert_eq!(policy.resolve("http://127.0.0.1:12345/path").await.unwrap().address.to_string(), "127.0.0.1");
    assert!(policy.resolve("http://127.0.0.1:12346/path").await.is_err_and(|e| e.status == 403));
    assert!(NetworkPolicy::new(&[], &[], None).is_err_and(|e| e.status == 400));
    assert!(NetworkPolicy::new(&vec!["https://example.com".to_owned(); 17], &[], None).is_err_and(|e| e.status == 400));
    assert_eq!(NetworkPolicy::new(&["https://Example.COM:443/".into(), "https://example.com".into()], &[], None).unwrap().origins, ["https://example.com"]);
}

#[test]
fn tunnel_authorities_are_host_and_port_only() {
    for valid in ["example.com:443", "[2606:4700::1111]:443", "93.184.216.34:8443"] {
        assert!(authority_is_valid(valid), "{valid}");
    }
    for invalid in ["example.com", "example.com:", "user@example.com:443", "example.com:443/x", "[::1:443", "exa mple.com:443"] {
        assert!(!authority_is_valid(invalid), "{invalid}");
    }
}

// ---------------------------------------------------------------- proxy

/// `policy.resolve` rewritten like the Node test: `public.example` is served by the loopback
/// fixture; `rebinding` makes every later lookup fail.
struct Rewriting {
    policy: NetworkPolicy,
    rebinding: AtomicBool,
}

impl Destination for Rewriting {
    fn resolve<'a>(&'a self, input: &'a str) -> BoxFuture<'a, HttpResult<Resolved>> {
        Box::pin(async move {
            if self.rebinding.load(Ordering::SeqCst) {
                return Err(HttpError::forbidden("Rebinding blocked."));
            }
            let target = self.policy.resolve(&input.replace("public.example", "127.0.0.1")).await?;
            Ok(Resolved { url: url::Url::parse(input).unwrap(), ..target })
        })
    }
}

async fn proxy_request(server: &str, credentials: Option<(&str, &str)>, method: &str, target: &str) -> std::io::Result<(u16, String)> {
    let address = server.trim_start_matches("http://");
    let mut stream = TcpStream::connect(address).await?;
    let mut request = format!("{method} {target} HTTP/1.1\r\nHost: {}\r\nCookie: secret=cookie\r\nAuthorization: Bearer secret\r\nConnection: close\r\n", url::Url::parse(target).map(|u| u.authority().to_owned()).unwrap_or_default());
    if let Some((user, password)) = credentials {
        request.push_str(&format!("Proxy-Authorization: Basic {}\r\n", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    let text = String::from_utf8_lossy(&response).into_owned();
    if text.is_empty() {
        return Err(std::io::Error::other("closed"));
    }
    let status = text.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    Ok((status, text.split_once("\r\n\r\n").map(|(_, b)| b.to_owned()).unwrap_or_default()))
}

#[tokio::test]
async fn the_loopback_proxy_authenticates_pins_the_validated_address_blocks_rebinding_and_closes_sockets() {
    let site = site(|_| text("200 OK", "", "pinned fixture")).await;
    let port = site.origin.rsplit(':').next().unwrap().to_owned();
    let destination = Arc::new(Rewriting { policy: NetworkPolicy::new(std::slice::from_ref(&site.origin), std::slice::from_ref(&site.origin), None).unwrap(), rebinding: AtomicBool::new(false) });
    let proxy = ComputerProxy::new(destination.clone());
    proxy.start().await.unwrap();
    let configuration = proxy.configuration().unwrap();
    let credentials = Some((configuration.username.as_str(), configuration.password.as_str()));
    let target = format!("http://public.example:{port}/pinned");
    assert_eq!(proxy_request(&configuration.server, None, "GET", &target).await.unwrap().0, 407);
    assert_eq!(proxy_request(&configuration.server, Some(("computer", "wrong")), "GET", &target).await.unwrap().0, 407);
    assert_eq!(proxy_request(&configuration.server, credentials, "GET", &target).await.unwrap(), (200, "pinned fixture".to_owned()));
    {
        let received = site.requests.lock().unwrap();
        assert_eq!(received[0].header("host"), Some(format!("public.example:{port}").as_str()));
        assert_eq!(received[0].header("cookie"), None);
        assert_eq!(received[0].header("authorization"), None);
        assert_eq!(received[0].header("proxy-authorization"), None);
    }
    assert_eq!(proxy_request(&configuration.server, credentials, "POST", &target).await.unwrap().0, 403);
    let probe = proxy.probe_url().unwrap();
    assert_eq!(proxy_request(&configuration.server, credentials, "GET", &probe).await.unwrap(), (200, proxy.probe_body()));
    destination.rebinding.store(true, Ordering::SeqCst);
    let (status, body) = proxy_request(&configuration.server, credentials, "GET", &target).await.unwrap();
    assert_eq!((status, body.as_str()), (403, super::proxy::DENIED_BODY));
    assert_eq!(site.requests.lock().unwrap().len(), 1);
    proxy.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(proxy_request(&configuration.server, credentials, "GET", &target).await.is_err());
    assert!(proxy.configuration().is_err());
}

#[tokio::test]
async fn the_proxy_refuses_downloads_and_strips_cookies_from_responses() {
    let site = site(|request| match request.path.as_str() {
        "/download" => text("200 OK", "Content-Disposition: attachment; filename=\"denied.txt\"\r\n", "not a file"),
        _ => text("200 OK", "Set-Cookie: fixture=private\r\nX-Fixture: kept\r\n", "page"),
    })
    .await;
    let policy = Arc::new(NetworkPolicy::new(std::slice::from_ref(&site.origin), std::slice::from_ref(&site.origin), None).unwrap());
    let proxy = ComputerProxy::new(policy);
    proxy.start().await.unwrap();
    let configuration = proxy.configuration().unwrap();
    let credentials = Some((configuration.username.as_str(), configuration.password.as_str()));
    let (status, body) = proxy_request(&configuration.server, credentials, "GET", &format!("{}/download", site.origin)).await.unwrap();
    assert_eq!((status, body.as_str()), (403, "Downloads are not allowed."));
    let address = configuration.server.trim_start_matches("http://");
    let mut stream = TcpStream::connect(address).await.unwrap();
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("computer:{}", configuration.password));
    stream.write_all(format!("GET {}/page HTTP/1.1\r\nProxy-Authorization: Basic {auth}\r\n\r\n", site.origin).as_bytes()).await.unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
    assert!(!raw.to_ascii_lowercase().contains("set-cookie"), "{raw}");
    assert!(raw.contains("X-Fixture: kept") && raw.ends_with("page"), "{raw}");
    let private = proxy_request(&configuration.server, credentials, "GET", "http://127.0.0.1:1/").await.unwrap();
    assert_eq!(private.0, 403);
    proxy.stop();
}

// ---------------------------------------------------------------- managed computer

fn start_input(origins: &[&str]) -> Value {
    json!({ "requestId": new_id(), "userInitiated": true, "origins": origins })
}

fn command(computer: &ManagedComputer, action: Value) -> Value {
    let status = computer.status();
    json!({ "requestId": new_id(), "epoch": status.epoch, "frameId": status.frame_id, "action": action })
}

#[tokio::test]
async fn the_computer_starts_stopped_and_requires_an_explicit_user_operation_with_public_origins() {
    let computer = ManagedComputer::new(ComputerOptions::default()).unwrap();
    assert_eq!(computer.status().state, ComputerState::Stopped);
    assert_eq!(computer.status().frame_id, None);
    let refused = computer.start(json!({ "requestId": new_id(), "userInitiated": false, "origins": ["https://example.com"] })).await;
    assert!(refused.is_err_and(|e| e.status == 400));
    assert!(computer.start(json!({ "requestId": new_id(), "userInitiated": true, "origins": ["https://example.com"], "extra": 1 })).await.is_err_and(|e| e.status == 400));
    let private = computer.start(start_input(&["http://127.0.0.1:12345"])).await;
    assert!(private.is_err_and(|e| e.status == 403));
    assert_eq!(computer.status().state, ComputerState::Stopped);
    let shot = computer.screenshot(json!({ "requestId": new_id(), "epoch": computer.status().epoch, "frameId": null })).await;
    assert!(shot.is_err_and(|e| e.status == 409));
    assert_eq!(computer.stop().await.state, ComputerState::Stopped);
    let request = start_input(&["http://127.0.0.1:12345"]);
    let _ = computer.start(request.clone()).await;
    assert!(computer.start(request).await.is_err_and(|e| e.status == 409 && e.message.contains("never replayed")));
}

fn chrome_available() -> bool {
    let available = cdp::chrome_executable().exists();
    if !available {
        eprintln!("skipping: Google Chrome is not installed at {}", cdp::chrome_executable().display());
    }
    available
}

/// Visible text next to everything the page-text walk must leave out: unrendered content, page
/// scripts, and form state (prefilled, typed, selected, and editable values).
const ARTICLE: &str = r#"<!doctype html><html><head><title>Article fixture</title><style>.gone{display:none}</style></head>
<body style="margin:24px;font:16px sans-serif">
<h1>Example   Domain</h1>
<p>This domain is for use in
   illustrative    examples in documents.</p>
<p>Second paragraph with <b>bold</b> and <a href="/more">a link</a>.</p>
<ul><li>First item</li><li>Second item</li></ul>
<pre>line one
  indented line two</pre>
<div class="gone">hidden by class</div>
<div hidden>hidden attribute</div>
<div style="visibility:hidden">invisible text <span style="visibility:visible">revealed child</span></div>
<div style="opacity:0">transparent text</div>
<details><summary>Summary line</summary>closed details body <p>closed details paragraph</p></details>
<noscript>noscript text</noscript>
<template>template text</template>
<script>window.scriptOnly = "script text";</script>
<div id="host">unslotted light text</div>
<label>Query <input id="query" aria-label="Query" type="text" value="prefilled-value"></label>
<textarea aria-label="Notes">textarea-default</textarea>
<select aria-label="Choice"><option>select-option-value</option></select>
<form><input type="password" value="hunter2-password"></form>
<div contenteditable="true">editable-content</div>
<script>
document.querySelector('#host').attachShadow({ mode: 'open' }).innerHTML = '<p>shadow paragraph</p>';
</script>
</body></html>"#;

/// Far more than the observation limit, with multi-byte characters at the cut.
fn long_page() -> String {
    let paragraphs: String = (0..2000).map(|i| format!("<p>Paragraph {i} 한글 😀 filler text.</p>")).collect();
    format!("<!doctype html><html><head><meta charset=\"utf-8\"><title>Long fixture</title></head><body>{paragraphs}</body></html>")
}

struct Fixture {
    computer: ManagedComputer,
    site: Site,
    private: Site,
    hang: Arc<tokio::sync::Notify>,
}

impl Fixture {
    async fn new() -> Self {
        let private = site(|_| text("200 OK", "", "private target")).await;
        let private_origin = private.origin.clone();
        let hang = Arc::new(tokio::sync::Notify::new());
        let signal = hang.clone();
        let site = site(move |request| match request.path.as_str() {
            "/hang" => {
                signal.notify_one();
                Reply::Hang
            }
            "/redirect" => text("302 Found", &format!("Location: {private_origin}/redirected\r\n"), ""),
            "/download" => text("200 OK", "Content-Disposition: attachment; filename=\"denied.txt\"\r\n", "not a file"),
            "/worker.js" => text("200 OK", "Content-Type: application/javascript\r\n", "self.addEventListener(\"install\",()=>{});"),
            "/popup" => text("200 OK", "", "popup must not load"),
            "/article" => html(ARTICLE, ""),
            "/long" => html(&long_page(), ""),
            _ => html(
                &format!(
                    r#"<!doctype html><html><head><title>Managed computer fixture</title></head>
      <body style="margin:24px;font:16px sans-serif"><h1>Public fixture</h1>
      <label>Query <input id="query" aria-label="Query" type="text"></label>
      <button id="count" onclick="this.textContent='Count '+(++window.count)">Count 0</button>
      <button onclick="this.textContent=document.querySelector('#query').value">Show query</button>
      <button onclick="window.open('/popup')">Popup</button>
      <a href="/download" download>Download</a>
      <input aria-label="Password" type="password"><input aria-label="Upload" type="file">
      <a href="{private_origin}/clicked">Private destination</a>
      <div style="height:1200px"></div><button>Bottom</button>
      <script>
      window.count=0;
      const attempts = [];
      try {{ const peer=new RTCPeerConnection(); peer.close(); attempts.push('rtc open'); }} catch {{ attempts.push('rtc blocked'); }}
      try {{ new WebSocket(location.origin.replace('http:', 'ws:')+'/ws'); attempts.push('ws open'); }} catch {{ attempts.push('ws blocked'); }}
      const state=document.createElement('button'); state.textContent=attempts.join(', '); document.body.prepend(state);
      fetch('{private_origin}/fetch').catch(()=>{{}});
      const image=new Image(); image.src='{private_origin}/image'; document.body.append(image);
      </script></body></html>"#
                ),
                "Set-Cookie: fixture=private; SameSite=Lax\r\n",
            ),
        })
        .await;
        let computer = ManagedComputer::new(ComputerOptions { allow_test_origins: vec![site.origin.clone()], test_browser: Some(TestBrowser { executable_path: None, chromium_sandbox: true }), lookup: None }).unwrap();
        Self { computer, site, private, hang }
    }

    async fn start(&self) -> ComputerStatus {
        self.computer.start(start_input(&[&self.site.origin])).await.expect("managed Chrome starts")
    }

    async fn click_label(&self, label: &str) -> HttpResult<ComputerObservation> {
        let observation = self.computer.perform(command(&self.computer, json!({ "type": "observe" }))).await?;
        let control = observation.frame.controls.iter().find(|c| c.label == label).unwrap_or_else(|| panic!("Expected visible control {label}: {:?}", observation.frame.controls));
        let (x, y) = (control.bounds.x + control.bounds.width / 2.0, control.bounds.y + control.bounds.height / 2.0);
        self.computer.perform(command(&self.computer, json!({ "type": "click", "x": x, "y": y }))).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_sandboxed_chrome_provides_png_frames_controls_typed_actions_stale_guards_and_at_most_once_execution() {
    if !chrome_available() {
        return;
    }
    let f = Fixture::new().await;
    let ready = f.start().await;
    assert_eq!(ready.state, ComputerState::Running);
    assert!(ready.sandboxed);
    assert_eq!(ready.viewport, VIEWPORT);
    let navigated = f.computer.perform(command(&f.computer, json!({ "type": "navigate", "url": f.site.origin }))).await.unwrap();
    assert_eq!(navigated.frame.title, "Managed computer fixture");
    assert!(navigated.frame.controls.iter().any(|c| c.label == "rtc blocked, ws blocked"), "{:?}", navigated.frame.controls);
    let status = f.computer.status();
    let shot = f.computer.screenshot(json!({ "requestId": new_id(), "epoch": status.epoch, "frameId": status.frame_id })).await.unwrap();
    assert_eq!(hex::encode(&shot.png[..8]), "89504e470d0a1a0a");
    assert_eq!(u32::from_be_bytes(shot.png[16..20].try_into().unwrap()), 960);
    assert_eq!(u32::from_be_bytes(shot.png[20..24].try_into().unwrap()), 600);
    assert!(!serde_json::to_string(&shot.frame).unwrap().contains("fixture=private"));
    let mut stale = command(&f.computer, json!({ "type": "observe" }));
    stale["frameId"] = json!(navigated.frame.id);
    assert!(f.computer.perform(stale).await.is_err_and(|e| e.status == 409));
    f.click_label("Query").await.unwrap();
    f.computer.perform(command(&f.computer, json!({ "type": "type", "text": "typed fixture text" }))).await.unwrap();
    let shown = f.click_label("Show query").await.unwrap();
    assert!(shown.frame.controls.iter().any(|c| c.label == "typed fixture text"), "{:?}", shown.frame.controls);
    let count = shown.frame.controls.iter().find(|c| c.label == "Count 0").unwrap();
    let click = command(&f.computer, json!({ "type": "click", "x": count.bounds.x + count.bounds.width / 2.0, "y": count.bounds.y + count.bounds.height / 2.0 }));
    let mut raced = click.clone();
    raced["requestId"] = json!(new_id());
    let (first, duplicate, raced) = tokio::join!(f.computer.perform(click.clone()), f.computer.perform(click.clone()), f.computer.perform(raced));
    assert!(duplicate.is_err_and(|e| e.status == 409));
    assert!(raced.is_err_and(|e| e.status == 409));
    assert!(first.unwrap().frame.controls.iter().any(|c| c.label == "Count 1"));
    let mut again = click.clone();
    again["requestId"] = json!(new_id());
    assert!(f.computer.perform(again).await.is_err_and(|e| e.status == 409));
    f.computer.perform(command(&f.computer, json!({ "type": "key", "key": "Tab" }))).await.unwrap();
    f.computer.perform(command(&f.computer, json!({ "type": "key", "key": "Shift+Tab" }))).await.unwrap();
    f.computer.perform(command(&f.computer, json!({ "type": "scroll", "deltaY": 700 }))).await.unwrap();
    assert!(f.private.hits().is_empty(), "{:?}", f.private.hits());
    let old_epoch = f.computer.status().epoch;
    f.computer.stop().await;
    assert_ne!(f.computer.status().epoch, old_epoch);
    f.start().await;
    let mut old = command(&f.computer, json!({ "type": "observe" }));
    old["epoch"] = json!(old_epoch);
    assert!(f.computer.perform(old).await.is_err_and(|e| e.status == 409));
    assert!(f.computer.perform(click).await.is_err_and(|e| e.status == 409));
    f.computer.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_chrome_blocks_private_redirects_subresources_sensitive_controls_downloads_uploads_and_unsafe_actions() {
    if !chrome_available() {
        return;
    }
    let f = Fixture::new().await;
    f.start().await;
    let loaded = f.computer.perform(command(&f.computer, json!({ "type": "navigate", "url": f.site.origin }))).await.unwrap();
    for label in ["Password", "Upload", "Download", "Private destination"] {
        assert!(loaded.frame.controls.iter().find(|c| c.label == label).is_some_and(|c| c.blocked), "{label}");
    }
    let password = loaded.frame.controls.iter().find(|c| c.label == "Password").unwrap();
    let click = command(&f.computer, json!({ "type": "click", "x": password.bounds.x + 2.0, "y": password.bounds.y + 2.0 }));
    assert!(f.computer.perform(click).await.is_err_and(|e| e.status == 403));
    assert_eq!(f.computer.status().frame_id, None);
    f.computer.perform(command(&f.computer, json!({ "type": "observe" }))).await.unwrap();
    f.click_label("Popup").await.unwrap();
    assert!(!f.site.hits().iter().any(|hit| hit.contains("/popup")));
    assert!(f.computer.perform(command(&f.computer, json!({ "type": "key", "key": "Control+L" }))).await.is_err_and(|e| e.status == 400));
    assert!(f.computer.perform(command(&f.computer, json!({ "type": "navigate", "url": "file:///etc/passwd" }))).await.is_err_and(|e| e.status == 403));
    f.computer.perform(command(&f.computer, json!({ "type": "observe" }))).await.unwrap();
    assert!(f.computer.perform(command(&f.computer, json!({ "type": "navigate", "url": f.private.origin }))).await.is_err_and(|e| e.status == 403));
    f.computer.perform(command(&f.computer, json!({ "type": "observe" }))).await.unwrap();
    assert!(f.computer.perform(command(&f.computer, json!({ "type": "navigate", "url": format!("{}/redirect", f.site.origin) }))).await.is_err());
    assert!(f.private.hits().is_empty(), "{:?}", f.private.hits());
    f.computer.stop().await;
}

#[test]
fn page_text_is_bounded_by_characters_without_splitting_one_and_keeps_only_line_breaks_and_tabs() {
    assert_eq!(bounded_text("short\ttext\nkept", 100), ("short\ttext\nkept".to_owned(), false));
    assert_eq!(bounded_text("a\u{0}b\u{7f}c\rd", 100), ("a b c d".to_owned(), false));
    assert_eq!(bounded_text("한글😀abc", 3), ("한글😀".to_owned(), true));
    assert_eq!(bounded_text("word     next", 6), ("word".to_owned(), true));
    assert_eq!(bounded_text("exact", 5), ("exact".to_owned(), false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_chrome_observations_carry_visible_page_text_without_hidden_text_or_form_values_and_truncate_long_pages() {
    if !chrome_available() {
        return;
    }
    let f = Fixture::new().await;
    f.start().await;
    let article = f.computer.perform(command(&f.computer, json!({ "type": "navigate", "url": format!("{}/article", f.site.origin) }))).await.unwrap();
    assert_eq!(article.frame.title, "Article fixture");
    f.click_label("Query").await.unwrap();
    f.computer.perform(command(&f.computer, json!({ "type": "key", "key": "End" }))).await.unwrap();
    f.computer.perform(command(&f.computer, json!({ "type": "type", "text": "typed-value" }))).await.unwrap();
    let observed = f.computer.perform(command(&f.computer, json!({ "type": "observe" }))).await.unwrap();
    let text = &observed.frame.page_text;
    assert!(!observed.frame.text_truncated);
    assert!(
        text.starts_with("Example Domain\n\nThis domain is for use in illustrative examples in documents.\n\nSecond paragraph with bold and a link.\n\nFirst item\nSecond item\nline one\n  indented line two"),
        "{text:?}"
    );
    for visible in ["revealed child", "Summary line", "shadow paragraph", "Query"] {
        assert!(text.contains(visible), "{visible}: {text:?}");
    }
    for absent in [
        "hidden by class", "hidden attribute", "invisible text", "transparent text", "closed details", "noscript text", "template text", "script text", "unslotted light text",
        "prefilled-value", "typed-value", "textarea-default", "select-option-value", "hunter2", "editable-content",
    ] {
        assert!(!text.contains(absent), "{absent}: {text:?}");
    }
    let frame = serde_json::to_value(&observed.frame).unwrap();
    assert_eq!(frame["pageText"], json!(text));
    assert_eq!(frame["textTruncated"], json!(false));

    let long = f.computer.perform(command(&f.computer, json!({ "type": "navigate", "url": format!("{}/long", f.site.origin) }))).await.unwrap();
    assert!(long.frame.text_truncated);
    assert!(long.frame.page_text.starts_with("Paragraph 0 한글 😀 filler text.\n\nParagraph 1 "), "{:?}", long.frame.page_text.chars().take(80).collect::<String>());
    let count = long.frame.page_text.chars().count();
    assert!((PAGE_TEXT_LIMIT - 10..=PAGE_TEXT_LIMIT).contains(&count), "{count}");
    assert!(!long.frame.page_text.contains('\u{fffd}'));
    f.computer.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_aborts_an_awaiting_navigation_and_queued_actions_and_destroys_the_upstream_connection() {
    if !chrome_available() {
        return;
    }
    let f = Fixture::new().await;
    f.start().await;
    let navigation = command(&f.computer, json!({ "type": "navigate", "url": format!("{}/hang", f.site.origin) }));
    let mut queued = navigation.clone();
    queued["requestId"] = json!(new_id());
    queued["action"] = json!({ "type": "observe" });
    let (first, second) = (f.computer.clone(), f.computer.clone());
    let pending = tokio::spawn(async move { first.perform(navigation).await });
    tokio::time::timeout(Duration::from_secs(10), f.hang.notified()).await.expect("navigation reaches the hanging fixture");
    let queued = tokio::spawn(async move { second.perform(queued).await });
    let stopped = f.computer.stop().await;
    assert_eq!(stopped.state, ComputerState::Stopped);
    assert!(!stopped.busy);
    assert_eq!(stopped.frame_id, None);
    assert!(pending.await.unwrap().is_err_and(|e| e.status == 409));
    assert!(queued.await.unwrap().is_err_and(|e| e.status == 409));
    assert_eq!(f.computer.stop().await.state, ComputerState::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chrome_launch_failures_are_explicit_and_never_fall_back_to_an_unsandboxed_browser() {
    let site = site(|_| text("200 OK", "", "fixture")).await;
    let computer = ManagedComputer::new(ComputerOptions {
        allow_test_origins: vec![site.origin.clone()],
        test_browser: Some(TestBrowser { executable_path: Some("/nonexistent/managed-computer-test-chrome".into()), chromium_sandbox: true }),
        lookup: None,
    })
    .unwrap();
    let failed = computer.start(start_input(&[&site.origin])).await;
    assert!(failed.is_err_and(|e| e.status == 503));
    let status = computer.status();
    assert_eq!(status.state, ComputerState::Stopped);
    assert!(!status.sandboxed);
    assert!(status.error.as_deref().is_some_and(|e| e.contains("sandbox")));
    computer.stop().await;
}

// ---------------------------------------------------------------- application computer

#[derive(Default)]
struct Fake {
    status: Mutex<Option<ComputerStatus>>,
    launched: AtomicUsize,
    performed: AtomicUsize,
    fail: bool,
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

fn stopped_status() -> ComputerStatus {
    ComputerStatus { state: ComputerState::Stopped, epoch: new_id(), frame_id: None, viewport: VIEWPORT, origins: vec![], url: None, busy: false, sandboxed: false, error: None }
}

impl ComputerService for Fake {
    fn status(&self) -> ComputerStatus {
        self.status.lock().unwrap().get_or_insert_with(stopped_status).clone()
    }
    fn start(&self, _: Value) -> BoxFuture<'_, HttpResult<ComputerStatus>> {
        Box::pin(async move {
            self.launched.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(HttpError::new(500, "A stopped request reached the service"));
            }
            let status = ComputerStatus { state: ComputerState::Running, epoch: new_id(), frame_id: Some(new_id()), sandboxed: true, ..stopped_status() };
            *self.status.lock().unwrap() = Some(status.clone());
            Ok(status)
        })
    }
    fn stop(&self) -> BoxFuture<'_, ComputerStatus> {
        Box::pin(async move {
            let status = {
                let mut current = self.status.lock().unwrap();
                let status = ComputerStatus { state: ComputerState::Stopped, frame_id: None, ..current.clone().unwrap_or_else(stopped_status) };
                *current = Some(status.clone());
                status
            };
            let release = self.release.lock().unwrap().take();
            if let Some(release) = release {
                let _ = release.await;
            }
            status
        })
    }
    fn perform(&self, _: Value) -> BoxFuture<'_, HttpResult<ComputerObservation>> {
        Box::pin(async move {
            self.performed.fetch_add(1, Ordering::SeqCst);
            Err(HttpError::new(500, "A stopped action reached the service"))
        })
    }
    fn screenshot(&self, _: Value) -> BoxFuture<'_, HttpResult<ComputerScreenshot>> {
        Box::pin(async move {
            let status = self.status();
            let frame = ComputerFrame { id: status.frame_id.clone().unwrap(), epoch: status.epoch.clone(), url: "about:blank".into(), title: "Fixture".into(), viewport: VIEWPORT, controls: vec![], mime_type: "image/png".into(), page_text: String::new(), text_truncated: false };
            Ok(ComputerScreenshot { status, frame, png: b"fixture PNG".to_vec() })
        })
    }
}

async fn store() -> (Arc<Store>, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path(), grok_bot_model::system_clock()).await.unwrap();
    (store, root)
}

fn first_bot(store: &Store) -> String {
    store.read(|state| state.disk().bots[0].id.clone())
}

#[tokio::test]
async fn stopping_the_application_computer_invalidates_starts_and_actions_already_queued_before_stop() {
    let (store, _root) = store().await;
    let fake = Arc::new(Fake { fail: true, ..Fake::default() });
    let computer = ApplicationComputer::with_service(store.clone(), fake.clone());
    let owner = first_bot(&store);
    let generation = computer.generation();
    let start = computer.start(&owner, start_input(&["https://example.com"]), generation);
    let action = computer.perform(&owner, json!({ "requestId": new_id(), "epoch": new_id(), "frameId": null, "action": { "type": "observe" } }), generation);
    computer.stop(Some(&owner)).await.unwrap();
    assert!(start.await.is_err_and(|e| e.message.contains("stopped or replaced")));
    assert!(action.await.is_err_and(|e| e.message.contains("stopped or replaced")));
    assert_eq!(fake.launched.load(Ordering::SeqCst), 0);
    assert_eq!(fake.performed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_command_admitted_before_stop_cannot_start_a_browser_after_the_command_queue_drains() {
    let (store, _root) = store().await;
    let computer = ApplicationComputer::with_service(store.clone(), Arc::new(Fake::default()));
    let owner = first_bot(&store);
    let generation = computer.generation();
    computer.stop(Some(&owner)).await.unwrap();
    assert!(computer.start(&owner, start_input(&["https://example.com"]), generation).await.is_err_and(|e| e.message.contains("stopped or replaced")));
    assert_eq!(computer.status(&owner).unwrap().state, ComputerState::Stopped);
    assert!(computer.status(&new_id()).is_err_and(|e| e.status == 404));
}

#[tokio::test]
async fn a_replacement_owner_stays_bound_when_an_older_stop_completes_after_a_new_start() {
    let (store, _root) = store().await;
    let other = store.mutate(|state| state.add_bot(BotInput { name: "Other".into(), color: Some("#75a9c0".into()), instructions: Some(String::new()), shape: None, pinned: None })).await.unwrap();
    let fake = Arc::new(Fake::default());
    let computer = Arc::new(ApplicationComputer::with_service(store.clone(), fake.clone()));
    let owner = first_bot(&store);
    computer.start(&owner, start_input(&["https://example.com"]), computer.generation()).await.unwrap();
    let (release, cleanup) = tokio::sync::oneshot::channel();
    *fake.release.lock().unwrap() = Some(cleanup);
    let stopping = {
        let (computer, owner) = (computer.clone(), owner.clone());
        tokio::spawn(async move { computer.stop(Some(&owner)).await })
    };
    while fake.status().state != ComputerState::Stopped {
        tokio::task::yield_now().await;
    }
    computer.start(&other.id, start_input(&["https://example.com"]), computer.generation()).await.unwrap();
    let _ = release.send(());
    stopping.await.unwrap().unwrap();
    let status = fake.status();
    assert!(computer.status(&owner).is_err_and(|e| e.message.contains("Another bot owns")));
    assert!(computer.preview(&owner, &status.epoch, status.frame_id.as_deref().unwrap()).is_err_and(|e| e.message.contains("Another bot owns")));
    assert_eq!(computer.status(&other.id).unwrap().state, ComputerState::Running);
    assert_eq!(computer.preview(&other.id, &status.epoch, status.frame_id.as_deref().unwrap()).unwrap().as_slice(), b"fixture PNG");
    assert!(computer.preview(&other.id, &status.epoch, &new_id()).is_err_and(|e| e.message.contains("stale")));
}
