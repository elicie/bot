use super::*;

#[tokio::test]
async fn expired_thumbnail_is_retired_when_busy_and_provider_images_survive() {
    let fixture = Fixture::start().await;
    let (root, service) = fixture.service(Limits { concurrent: 1, ..Limits::default() }).await;
    let provider_id = save_generated_image(root.path(), PNG).unwrap();
    let preview = service.preview(&fixture.url("/articles/final")).await.unwrap();
    let id = preview.image_url.unwrap().strip_prefix(ROUTE_PREFIX).unwrap().to_owned();
    let slot = service.slots.clone().acquire_owned().await.unwrap();
    service.cache.lock().unwrap().ready.values_mut().for_each(|entry| entry.expires = Instant::now());
    assert_eq!(service.preview(&fixture.url("/busy")).await.unwrap_err().status, 503);
    assert!(grok_bot_store::generated_images::read_generated_image(root.path(), &id).is_err());
    assert!(grok_bot_store::generated_images::read_generated_image(root.path(), &provider_id).is_ok());
    assert!(service.cache.lock().unwrap().ready.is_empty());
    drop(slot);
    service.preview(&fixture.url("/articles/final")).await.unwrap();
    service.store.mutate(|state| state.set_link_previews_enabled(false)).await.unwrap();
    assert_eq!(service.preview(&fixture.url("/articles/final")).await.unwrap_err().status, 403);
    assert_eq!(fixture.hits("/articles/final"), 2, "disabled cache hits cause no new download");
    service.clear();
    assert!(grok_bot_store::generated_images::read_generated_image(root.path(), &provider_id).is_ok());
}

#[tokio::test]
async fn redirect_dns_is_checked_for_html_and_images_and_the_budget_is_shared() {
    let fixture = Fixture::start().await;
    let (_root, mut service) = fixture.service(Limits::default()).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    Arc::get_mut(&mut service).unwrap().policy.lookup = Arc::new(move |host, _slot| {
        assert_eq!(host, "mixed.invalid");
        count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(vec!["8.8.8.8".parse().unwrap(), "10.0.0.1".parse().unwrap()]) })
    });
    assert_eq!(service.preview(&fixture.url("/redirect-dns-private")).await.unwrap_err().status, 403);
    let card = service.preview(&fixture.url("/thumb-dns-private")).await.unwrap();
    assert_eq!(card.title, "Keep text");
    assert!(card.image_url.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let card = service.preview(&fixture.url("/chain/0")).await.unwrap();
    assert!(card.image_url.is_none());
    assert_eq!(fixture.hits("/loop"), 2, "four HTML redirects leave one thumbnail redirect");
}

#[test]
fn pathological_metadata_has_independent_parser_and_linear_text_budgets() {
    let final_url = preview_url("https://public.invalid/").unwrap();
    let start = std::time::Instant::now();
    for source in [
        format!("<title>Safe</title>{}", "<div>".repeat(100_000)),
        format!("<title>Safe</title><meta property='og:title' content='{}'>", "&lt;div&gt;x".repeat(30_000)),
    ] {
        assert!(source.len() < 512 * 1024);
        let parsed = metadata(source.as_bytes(), &final_url);
        assert!(parsed.title.chars().count() <= 512);
    }
    let nested = "<div>x".repeat(50_000);
    assert!(plain_text(&nested, 512).chars().count() <= 512);
    assert!(parser_input(&nested, 64 * 1024, 2048).matches('<').count() <= 2048);
    assert!(start.elapsed() < Duration::from_secs(2), "structural limits avoid quadratic metadata work");
}

#[tokio::test]
async fn blocking_parser_and_save_work_keep_their_slot_after_timeout() {
    let slots = Arc::new(Semaphore::new(1));
    let slot = Arc::new(slots.clone().try_acquire_owned().unwrap());
    let (release, wait) = std::sync::mpsc::channel();
    let (started, did_start) = tokio::sync::oneshot::channel();
    let result = within(
        Instant::now() + Duration::from_millis(20),
        blocking(&slot, move || {
            let _ = started.send(());
            wait.recv().unwrap();
        }),
    )
    .await;
    did_start.await.unwrap();
    assert_eq!(result.unwrap_err().status, 504);
    drop(slot);
    assert_eq!(slots.available_permits(), 0, "detached blocking work still consumes its admission");
    release.send(()).unwrap();
    let permit = tokio::time::timeout(Duration::from_secs(1), slots.acquire()).await.unwrap().unwrap();
    drop(permit);
    assert_eq!(slots.available_permits(), 1);
}

#[tokio::test]
async fn blocking_dns_keeps_concurrency_bounded_after_the_preview_times_out() {
    let fixture = Fixture::start().await;
    let (_root, mut service) = fixture.service(Limits { concurrent: 1, total: Duration::from_millis(20), ..Limits::default() }).await;
    let (release, wait) = std::sync::mpsc::channel();
    let wait = Arc::new(Mutex::new(Some(wait)));
    Arc::get_mut(&mut service).unwrap().policy.lookup = Arc::new(move |_, slot| {
        let wait = wait.lock().unwrap().take().unwrap();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let _slot = slot;
                wait.recv().unwrap();
                Ok(vec!["8.8.8.8".parse().unwrap()])
            })
            .await
            .map_err(std::io::Error::other)?
        })
    });
    assert_eq!(service.preview("http://blocked-dns.invalid/").await.unwrap_err().status, 504);
    assert_eq!(service.slots.available_permits(), 0);
    assert_eq!(service.preview(&fixture.url("/no-work")).await.unwrap_err().status, 503);
    assert_eq!(fixture.hits("/no-work"), 0);
    release.send(()).unwrap();
    let permit = tokio::time::timeout(Duration::from_secs(1), service.slots.acquire()).await.unwrap().unwrap();
    drop(permit);
}

#[test]
fn ambient_proxy_environment_is_ignored_in_an_isolated_process() {
    const CHILD: &str = "GROK_BOT_LINK_PREVIEW_PROXY_CHILD";
    if std::env::var(CHILD).as_deref() == Ok("1") {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let fixture = Fixture::start().await;
            let (_root, service) = fixture.service(Limits::default()).await;
            let card = service.preview(&fixture.url("/ordinary")).await.unwrap();
            assert_eq!(card.title, "Ordinary title");
            assert_eq!(fixture.hits("/ordinary"), 1);
        });
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", "link_preview::tests::hardening::ambient_proxy_environment_is_ignored_in_an_isolated_process"])
        .env(CHILD, "1")
        .env("NO_PROXY", "")
        .env("no_proxy", "");
    for name in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
        child.env(name, "http://proxy-user:proxy-password@127.0.0.1:9");
    }
    assert!(child.output().unwrap().status.success(), "fixture requests must never use an ambient proxy");
}
