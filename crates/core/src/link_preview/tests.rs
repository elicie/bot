//! All network requests in these tests terminate at in-process loopback fixtures. Resolver and
//! fixture injections live here, never in a production constructor or API.

use super::*;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::Response;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod hardening;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfixture";

fn test_slot() -> Arc<OwnedSemaphorePermit> {
    Arc::new(Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap())
}

#[derive(Default)]
struct Requests {
    hits: Mutex<HashMap<String, usize>>,
    headers: Mutex<Vec<HeaderMap>>,
}

struct Fixture {
    origin: String,
    requests: Arc<Requests>,
    task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start() -> Self {
        let requests = Arc::new(Requests::default());
        let app = Router::new().fallback(get(serve_fixture)).with_state(requests.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { origin, requests, task }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }

    fn hits(&self, path: &str) -> usize {
        *self.requests.hits.lock().unwrap().get(path).unwrap_or(&0)
    }

    async fn service(&self, limits: Limits) -> (tempfile::TempDir, Arc<LinkPreviewService>) {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path(), grok_bot_model::system_clock()).await.unwrap();
        store.mutate(|state| state.set_link_previews_enabled(true)).await.unwrap();
        let service = LinkPreviewService {
            store,
            policy: Policy { fixtures: HashSet::from([self.origin.clone()]), ..Policy::new() },
            slots: Arc::new(Semaphore::new(limits.concurrent)),
            limits,
            cache: Mutex::new(Cache::default()),
            cache_now: Arc::new(Instant::now),
        };
        (root, Arc::new(service))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn page(body: impl Into<Body>) -> Response {
    Response::builder().header(header::CONTENT_TYPE, "text/html; charset=utf-8").body(body.into()).unwrap()
}

fn redirect(to: &str) -> Response {
    Response::builder().status(302).header(header::LOCATION, to).body(Body::empty()).unwrap()
}

async fn serve_fixture(State(requests): State<Arc<Requests>>, uri: Uri, headers: HeaderMap) -> Response {
    *requests.hits.lock().unwrap().entry(uri.path().to_owned()).or_default() += 1;
    requests.headers.lock().unwrap().push(headers);
    match uri.path() {
        "/start" => redirect("/articles/final"),
        "/articles/final" => page(
            r#"<title>Fallback title</title><meta name="description" content="Fallback description">
               <meta property="og:title" content="A &amp; B &lt;img src=x onerror=alert(1)&gt;">
               <meta property="og:description" content="Nice &lt;b&gt;preview&lt;/b&gt;">
               <meta property="og:image" content="../image"><meta property="og:url" content="http://127.0.0.1/secret">
               <base href="http://127.0.0.1/ignored/"><script>not metadata</script>"#,
        ),
        "/image" => Response::builder().header(header::CONTENT_TYPE, "text/html").body(Body::from(PNG)).unwrap(), // Sniff bytes, not MIME.
        "/svg" => Response::builder().header(header::CONTENT_TYPE, "image/png").body(Body::from("<svg onload=alert(1) />")).unwrap(),
        "/redirect-private" => redirect("http://127.0.0.2:9/private"),
        "/redirect-dns-private" => redirect("http://mixed.invalid:9/private"),
        "/thumb-dns-private" => page(r#"<title>Keep text</title><meta property="og:image" content="/redirect-dns-private">"#),
        path if path.starts_with("/chain/") => {
            let hop: usize = path.trim_start_matches("/chain/").parse().unwrap();
            if hop < 4 { redirect(&format!("/chain/{}", hop + 1)) } else { page(r#"<title>Keep text</title><meta property="og:image" content="/loop">"#) }
        }
        "/redirect-private-v6" => redirect("http://[::1]:9/private"),
        "/redirect-relative" => redirect("/redirect-private"),
        "/redirect-userinfo" => redirect("http://user:password@127.0.0.1/"),
        "/loop" => redirect("/loop"),
        "/cookie" => {
            Response::builder().status(302).header(header::LOCATION, "/articles/final").header(header::SET_COOKIE, "secret=canary").body(Body::empty()).unwrap()
        }
        "/thumb-private" => page(r#"<title>Keep text</title><meta property="og:image" content="http://127.0.0.2:9/private">"#),
        "/thumb-redirect-private" => page(r#"<title>Keep text</title><meta property="og:image" content="/redirect-private">"#),
        "/thumb-svg" => page(r#"<title>Keep text</title><meta property="og:image" content="/svg">"#),
        "/thumb-huge" => page(r#"<title>Keep text</title><meta property="og:image" content="/huge-chunked">"#),
        "/thumb-huge-length" => page(r#"<title>Keep text</title><meta property="og:image" content="/huge-length">"#),
        "/huge-length" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html")
            .header(header::CONTENT_LENGTH, (4 * 1024 * 1024).to_string())
            .body(Body::empty())
            .unwrap(),
        "/huge-chunked" => {
            let chunks = futures::stream::iter((0..513).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![b'x'; 4096]))));
            page(Body::from_stream(chunks))
        }
        "/slow" => {
            tokio::time::sleep(Duration::from_millis(120)).await;
            page("<title>Slow</title>")
        }
        "/slow-redirect" => {
            tokio::time::sleep(Duration::from_millis(45)).await;
            redirect("/slow")
        }
        "/thumb-slow" => page(r#"<title>Keep text</title><meta property="og:image" content="/slow">"#),
        "/delayed-thumb" => {
            tokio::time::sleep(Duration::from_millis(45)).await;
            page(r#"<title>Keep text</title><meta property="og:image" content="/slow">"#)
        }
        "/deadline-slow" => {
            tokio::time::sleep(Duration::from_millis(2500)).await;
            page("<title>Slow deadline fixture</title>")
        }
        "/deadline-redirect" => {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            redirect("/deadline-slow")
        }
        "/deadline-thumbnail" => {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            page(r#"<title>Keep text</title><meta property="og:image" content="/deadline-image">"#)
        }
        "/deadline-image" => {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            Response::builder().header(header::CONTENT_TYPE, "image/png").body(Body::from(PNG)).unwrap()
        }
        "/compressed" => {
            Response::builder().header(header::CONTENT_TYPE, "text/html").header(header::CONTENT_ENCODING, "gzip").body(Body::from("not gzip")).unwrap()
        }
        _ => page("<title> Ordinary title </title><meta name=description content=' A normal  description '>"),
    }
}

#[test]
fn unsafe_literals_schemes_and_ambiguous_urls_are_rejected() {
    for url in [
        "file:///etc/passwd",
        "data:text/html,x",
        "ftp://example.invalid/",
        "javascript:alert(1)",
        "http://user:pass@example.invalid/",
        "http://user@example.invalid/",
        "http://example.invalid:0/",
        "http://example.invalid/has space",
        "http://example.invalid/\\evil",
        "http://example.invalid/\n",
        "http://[fe80::1%25eth0]/",
        "http://[::ffff:127.0.0.1]/\\",
        "",
        "//example.invalid/",
    ] {
        assert!(preview_url(url).is_err(), "{url}");
    }
    assert_eq!(preview_url("HTTPS://Example.Invalid/path#fragment").unwrap().as_str(), "https://example.invalid/path");
}

#[tokio::test]
async fn rejects_all_special_use_literal_ranges_without_dns_or_network() {
    let policy = Policy { fixtures: HashSet::new(), lookup: Arc::new(|_, _| panic!("literals must not invoke DNS")) };
    for host in [
        "0.0.0.0",
        "0.1.2.3",
        "10.1.2.3",
        "100.64.0.1",
        "127.0.0.1",
        "127.1",
        "2130706433",
        "0x7f000001",
        "169.254.169.254",
        "172.16.0.1",
        "192.0.0.9",
        "192.0.2.1",
        "192.88.99.1",
        "192.168.1.1",
        "198.18.0.1",
        "198.51.100.1",
        "203.0.113.1",
        "224.1.1.1",
        "240.1.1.1",
        "255.255.255.255",
        "[::]",
        "[::1]",
        "[::ffff:8.8.8.8]",
        "[64:ff9b::808:808]",
        "[100::1]",
        "[2001::1]",
        "[2001:db8::1]",
        "[2002::1]",
        "[3fff::1]",
        "[fc00::1]",
        "[fe80::1]",
        "[ff02::1]",
    ] {
        let url = preview_url(&format!("http://{host}/")).unwrap();
        assert_eq!(policy.resolve(&url, &test_slot()).await.err().unwrap().status, 403, "{host}");
    }
    for host in ["8.8.8.8", "1.1.1.1", "[2606:4700:4700::1111]"] {
        assert!(policy.resolve(&preview_url(&format!("http://{host}/")).unwrap(), &test_slot()).await.is_ok());
    }
}

#[tokio::test]
async fn dns_mixed_empty_and_rebinding_answers_are_fail_closed() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let policy = Policy {
        fixtures: HashSet::new(),
        lookup: Arc::new(move |name, _slot| {
            count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(match name.as_str() {
                    "mixed.invalid" => vec!["8.8.8.8".parse().unwrap(), "127.0.0.1".parse().unwrap()],
                    "mixed-v6.invalid" => vec!["2606:4700::1".parse().unwrap(), "fd00::1".parse().unwrap()],
                    "empty.invalid" => Vec::new(),
                    _ => vec!["8.8.8.8".parse().unwrap(), "1.1.1.1".parse().unwrap()],
                })
            })
        }),
    };
    for host in ["mixed.invalid", "mixed-v6.invalid", "empty.invalid"] {
        assert_eq!(policy.resolve(&preview_url(&format!("https://{host}/")).unwrap(), &test_slot()).await.err().unwrap().status, 403);
    }
    let pinned = policy.resolve(&preview_url("https://public.invalid/").unwrap(), &test_slot()).await.unwrap();
    for _ in 0..3 {
        let answers: Vec<_> = pinned.resolve("public.invalid".parse().unwrap()).await.unwrap().collect();
        assert_eq!(answers, ["8.8.8.8:443".parse::<SocketAddr>().unwrap()]);
    }
    assert!(pinned.resolve("other.invalid".parse().unwrap()).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 4, "the connector never re-resolves a validated name");
}

#[tokio::test]
async fn actual_http_connector_uses_pinned_address_once_per_hop() {
    let fixture = Fixture::start().await;
    let (_root, mut service) = fixture.service(Limits::default()).await;
    let port = Url::parse(&fixture.origin).unwrap().port().unwrap();
    let origin = format!("http://fixture.invalid:{port}");
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let service_mut = Arc::get_mut(&mut service).unwrap();
    service_mut.policy.fixtures.insert(origin.clone()); // Unit-only name-to-loopback transport.
    service_mut.policy.lookup = Arc::new(move |host, _slot| {
        assert_eq!(host, "fixture.invalid");
        let call = count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(vec![if call == 0 { "127.0.0.1" } else { "10.0.0.1" }.parse().unwrap()]) })
    });
    assert_eq!(service.preview(&format!("{origin}/ordinary")).await.unwrap().title, "Ordinary title");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.hits("/ordinary"), 1);
    // A different request performs fresh validation and rejects the rebound answer, never connects.
    assert_eq!(service.preview(&format!("{origin}/different")).await.unwrap_err().status, 403);
    assert_eq!(fixture.hits("/different"), 0);
}

#[tokio::test]
async fn redirects_metadata_and_thumbnails_use_safe_local_contract() {
    let fixture = Fixture::start().await;
    let (root, service) = fixture.service(Limits::default()).await;
    let card = service.preview(&fixture.url("/start#ignored")).await.unwrap();
    assert_eq!(card.url, fixture.url("/articles/final"));
    assert_eq!(card.title, "A & B");
    assert_eq!(card.description, "Nice preview");
    let local = card.image_url.as_deref().unwrap();
    let id = local.strip_prefix(ROUTE_PREFIX).unwrap();
    assert!(uuid::Uuid::parse_str(id).is_ok());
    assert_eq!(grok_bot_store::generated_images::read_generated_image(root.path(), id).unwrap(), (PNG.to_vec(), "image/png"));
    assert_eq!(fixture.hits("/image"), 1, "relative OG image uses final URL, not hostile base element");
    assert!(!serde_json::to_string(&card).unwrap().contains("onerror"));

    for path in ["/thumb-private", "/thumb-redirect-private", "/thumb-svg", "/thumb-huge", "/thumb-huge-length"] {
        let card = service.preview(&fixture.url(path)).await.unwrap();
        assert_eq!(card.title, "Keep text", "{path}");
        assert!(card.image_url.is_none(), "{path}");
        assert!(serde_json::to_value(card).unwrap().get("imageUrl").is_none());
    }
    for path in ["/redirect-private", "/redirect-private-v6", "/redirect-relative", "/redirect-userinfo"] {
        assert_eq!(service.preview(&fixture.url(path)).await.unwrap_err().status, 403, "{path}");
    }
    assert_eq!(service.preview(&fixture.url("/loop")).await.unwrap_err().status, 502);
    assert_eq!(fixture.hits("/loop"), 6);
    assert_eq!(fixture.hits("/private"), 0);
}

#[tokio::test]
async fn cookies_referer_authorization_and_compressed_bodies_are_not_used() {
    let fixture = Fixture::start().await;
    let (_root, service) = fixture.service(Limits::default()).await;
    service.preview(&fixture.url("/cookie")).await.unwrap();
    for headers in fixture.requests.headers.lock().unwrap().iter() {
        for forbidden in ["cookie", "authorization", "proxy-authorization", "referer", "origin"] {
            assert!(!headers.contains_key(forbidden), "{forbidden}");
        }
        assert_eq!(headers[header::ACCEPT_ENCODING], "identity");
    }
    assert_eq!(service.preview(&fixture.url("/compressed")).await.unwrap_err().status, 502);
}

#[tokio::test]
async fn content_length_and_chunked_responses_are_byte_bounded() {
    let fixture = Fixture::start().await;
    let (_root, service) = fixture.service(Limits::default()).await;
    assert_eq!(service.preview(&fixture.url("/huge-chunked")).await.unwrap_err().status, 413);
    // Use a raw fixture to send a large advertised body without sending it, proving early rejection.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut chunk = [0; 1024];
            let count = socket.read(&mut chunk).await.unwrap();
            assert!(count > 0, "client sent a complete request before the fixture responds");
            request.extend_from_slice(&chunk[..count]);
            if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                break;
            }
            assert!(request.len() <= 4096, "fixture request headers are bounded");
        }
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 99999999\r\n\r\n").await.unwrap();
        let mut tail = [0; 1];
        assert_eq!(socket.read(&mut tail).await.unwrap_or(0), 0, "client rejects headers without downloading body");
    });
    let (_root, mut raw_service) = fixture.service(Limits::default()).await;
    Arc::get_mut(&mut raw_service).unwrap().policy.fixtures.insert(origin.clone());
    assert_eq!(raw_service.preview(&origin).await.unwrap_err().status, 413);
    peer.await.unwrap();
}

#[tokio::test]
async fn one_deadline_covers_dns_redirects_html_and_thumbnail() {
    let fixture = Fixture::start().await;
    // Wide phase gaps preserve the shared-deadline assertion under parallel Chrome/engine tests;
    // sub-100ms total deadlines mostly measure CI scheduler contention rather than this contract.
    let limits = Limits { total: Duration::from_millis(1500), ..Limits::default() };
    let (_root, service) = fixture.service(limits).await;
    for path in ["/deadline-slow", "/deadline-redirect"] {
        let start = Instant::now();
        assert!(matches!(service.preview(&fixture.url(path)).await.unwrap_err().status, 502 | 504));
        assert!(start.elapsed() < Duration::from_millis(2250));
    }
    let start = Instant::now();
    let card = service.preview(&fixture.url("/deadline-thumbnail")).await.unwrap();
    assert_eq!(card.title, "Keep text");
    assert!(card.image_url.is_none(), "a fresh thumbnail deadline would incorrectly keep the image");
    assert!(start.elapsed() < Duration::from_millis(2000), "thumbnail does not receive a fresh timeout");

    let (_root, mut service) = fixture.service(Limits { total: Duration::from_millis(150), ..Limits::default() }).await;
    Arc::get_mut(&mut service).unwrap().policy.lookup = Arc::new(|_, _slot| {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            Ok(vec!["8.8.8.8".parse().unwrap()])
        })
    });
    let start = Instant::now();
    assert_eq!(service.preview("http://slow-dns.invalid/").await.unwrap_err().status, 504);
    assert!(start.elapsed() < Duration::from_millis(800));
}

#[tokio::test]
async fn bounded_ttl_lru_failure_cache_and_repeat_coalescing() {
    let fixture = Fixture::start().await;
    let limits = Limits { ttl: Duration::from_millis(30), entries: 2, ..Limits::default() };
    let (root, mut service) = fixture.service(limits).await;
    let cache_time = Arc::new(Mutex::new(Instant::now()));
    let now = cache_time.clone();
    Arc::get_mut(&mut service).unwrap().cache_now = Arc::new(move || *now.lock().unwrap());
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let service = service.clone();
        let url = fixture.url("/slow");
        tasks.push(tokio::spawn(async move { service.preview(&url).await.unwrap() }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(fixture.hits("/slow"), 1);
    service.preview(&fixture.url("/slow#fragment")).await.unwrap();
    assert_eq!(fixture.hits("/slow"), 1);
    // Advance only cache time: the boundary and later LRU checks do not depend on HTTP scheduling.
    *cache_time.lock().unwrap() += service.limits.ttl - Duration::from_nanos(1);
    service.preview(&fixture.url("/slow")).await.unwrap();
    assert_eq!(fixture.hits("/slow"), 1, "success remains cached immediately before expiry");
    *cache_time.lock().unwrap() += Duration::from_nanos(1);
    service.preview(&fixture.url("/slow")).await.unwrap();
    assert_eq!(fixture.hits("/slow"), 2);

    let preview = service.preview(&fixture.url("/articles/final")).await.unwrap();
    let id = preview.image_url.unwrap().strip_prefix(ROUTE_PREFIX).unwrap().to_owned();
    service.preview(&fixture.url("/a")).await.unwrap();
    service.preview(&fixture.url("/a")).await.unwrap(); // Make a the newest entry.
    service.preview(&fixture.url("/b")).await.unwrap(); // Evicts the thumbnail card.
    assert!(grok_bot_store::generated_images::read_generated_image(root.path(), &id).is_err());
    assert_eq!(service.cache.lock().unwrap().ready.len(), 2);
    for _ in 0..2 {
        assert_eq!(service.preview(&fixture.url("/redirect-private")).await.unwrap_err().status, 403);
    }
    assert_eq!(fixture.hits("/redirect-private"), 1, "errors also have a bounded short TTL");
    *cache_time.lock().unwrap() += service.limits.failure_ttl;
    assert_eq!(service.preview(&fixture.url("/redirect-private")).await.unwrap_err().status, 403);
    assert_eq!(fixture.hits("/redirect-private"), 2, "failed entries expire exactly at their deadline");
}

#[tokio::test]
async fn concurrency_is_bounded_and_disable_stops_cached_and_new_downloads() {
    let fixture = Fixture::start().await;
    let (_root, service) = fixture.service(Limits { concurrent: 1, ..Limits::default() }).await;
    let slow = {
        let service = service.clone();
        let url = fixture.url("/slow");
        tokio::spawn(async move { service.preview(&url).await })
    };
    // Synchronize on the server receiving the request rather than assuming the task was scheduled.
    tokio::time::timeout(Duration::from_secs(1), async {
        while fixture.hits("/slow") == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(service.preview(&fixture.url("/other")).await.unwrap_err().status, 503);
    assert_eq!(fixture.hits("/other"), 0);
    service.store.mutate(|state| state.set_link_previews_enabled(false)).await.unwrap();
    assert_eq!(slow.await.unwrap().unwrap_err().status, 403);
    assert_eq!(service.preview(&fixture.url("/slow")).await.unwrap_err().status, 403);
    assert_eq!(fixture.hits("/slow"), 1);
    assert!(service.cache.lock().unwrap().ready.is_empty());
}

#[tokio::test]
async fn disable_during_dns_or_html_prevents_the_next_hop_and_thumbnail() {
    let fixture = Fixture::start().await;
    let (_root, service) = fixture.service(Limits::default()).await;
    let pending = {
        let service = service.clone();
        let url = fixture.url("/delayed-thumb");
        tokio::spawn(async move { service.preview(&url).await })
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        while fixture.hits("/delayed-thumb") == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    service.store.mutate(|state| state.set_link_previews_enabled(false)).await.unwrap();
    assert_eq!(pending.await.unwrap().unwrap_err().status, 403);
    assert_eq!(fixture.hits("/slow"), 0);

    let (_root, mut service) = fixture.service(Limits::default()).await;
    let store = service.store.clone();
    Arc::get_mut(&mut service).unwrap().policy.lookup = Arc::new(move |_, _slot| {
        let store = store.clone();
        Box::pin(async move {
            store.mutate(|state| state.set_link_previews_enabled(false)).await.unwrap();
            Ok(vec!["8.8.8.8".parse().unwrap()])
        })
    });
    assert_eq!(service.preview("http://dns-disable.invalid/").await.unwrap_err().status, 403);
}

#[test]
fn metadata_is_plain_bounded_text_and_never_trusts_page_canonical_or_base() {
    let final_url = preview_url("https://public.invalid/articles/a").unwrap();
    let raw = format!(
        r#"<title> Title &amp; More </title><meta name="DESCRIPTION" content=" &lt;script&gt;bad()&lt;/script&gt;Hello &lt;b&gt;world&lt;/b&gt; &#x202e; ">
        <base href="http://127.0.0.1/"><meta property="og:url" content="javascript:bad()"><meta property="og:image" content="../thumb">
        <meta property="og:title" content="{}">"#,
        "x".repeat(1000)
    );
    let parsed = metadata(raw.as_bytes(), &final_url);
    assert_eq!(parsed.url, final_url.as_str());
    assert_eq!(parsed.title.len(), 512);
    assert_eq!(parsed.description, "Hello world");
    assert_eq!(parsed.image.unwrap().as_str(), "https://public.invalid/thumb");
    for image in ["data:image/png,x", "javascript:bad()", "http://user:pass@public.invalid/a", "file:///etc/passwd"] {
        let parsed = metadata(format!("<title>&lt;b&gt;literal&lt;/b&gt;</title><meta property='og:image' content='{image}'>").as_bytes(), &final_url);
        assert_eq!(parsed.title, "literal");
        assert!(parsed.image.is_none());
    }
}

#[test]
fn fixture_origins_require_exact_bare_loopback_http_nonzero_ports() {
    assert_eq!(fixture_origins(r#"["http://127.0.0.1:1234","http://[::1]:2345"]"#).unwrap().len(), 2);
    for input in [
        r#"["http://localhost:1234"]"#,
        r#"["http://127.0.0.2:1234"]"#,
        r#"["https://127.0.0.1:1234"]"#,
        r#"["http://127.0.0.1"]"#,
        r#"["http://127.0.0.1:0"]"#,
        r#"["http://127.0.0.1:1234/"]"#,
        r#"["http://127.0.0.1:1234/path"]"#,
        r#"["http://127.0.0.1:1234#x"]"#,
        r#"["http://8.8.8.8:1234"]"#,
        r#"["http://127.0.0.1:1234","http://localhost:1234"]"#,
        "not JSON",
    ] {
        assert!(fixture_origins(input).is_none(), "{input}");
    }
}
