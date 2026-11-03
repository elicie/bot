//! Backend composition and lifecycle for the web server and a future desktop host.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use grok_bot_model::Clock;
use grok_bot_store::Store;

use crate::http::{HttpContext, default_allowed_hosts, router};
use crate::queue::CommandQueue;
use crate::runtime::{Runtime, RuntimeOptions};
use crate::scheduler::{RoutineScheduler, SchedulerOptions};

pub struct BackendOptions {
    pub data_dir: PathBuf,
    /// Engine root prepared with `grok_bot_engine::host::prepare_process_environment`.
    pub engine_root: PathBuf,
    /// Explicit on-disk frontend override. Takes precedence over embedded files.
    pub web_root: Option<PathBuf>,
    pub embedded_web: Option<crate::web::EmbeddedWeb>,
    /// Extra Host names accepted besides `localhost`, `127.0.0.1`, `[::1]`.
    pub allowed_hosts: Vec<String>,
    pub now: Option<Clock>,
    pub scheduler: Option<SchedulerOptions>,
    pub runtime: Option<RuntimeOptions>,
}

pub struct Backend {
    pub store: Arc<Store>,
    pub runtime: Arc<Runtime>,
    pub scheduler: Arc<RoutineScheduler>,
    pub queue: CommandQueue,
    context: Arc<HttpContext>,
}

impl Backend {
    pub async fn new(options: BackendOptions) -> anyhow::Result<Self> {
        Self::compose(options, None).await
    }

    /// Desktop-only API authentication. Internal engine/webhook capabilities keep their own tokens.
    /// The web adapter's existing `new` and HTTP contracts are unchanged.
    pub async fn for_desktop(options: BackendOptions, token: crate::desktop_auth::DesktopToken) -> anyhow::Result<Self> {
        Self::compose(options, Some(token)).await
    }

    async fn compose(options: BackendOptions, token: Option<crate::desktop_auth::DesktopToken>) -> anyhow::Result<Self> {
        let now = options.now.unwrap_or_else(grok_bot_model::system_clock);
        let store = Store::open(&options.data_dir, now.clone()).await?;
        let queue = CommandQueue::new();
        let runtime_options = options.runtime.unwrap_or(RuntimeOptions {
            engine_root: options.engine_root.clone(),
            startup_timeout: Duration::from_secs(30),
            prompt_idle_timeout: Duration::from_secs(30),
            cancel_deadline: Duration::from_millis(800),
        });
        let runtime = Runtime::new(store.clone(), queue.clone(), runtime_options);
        let computer = crate::computer::ApplicationComputer::new(store.clone(), crate::computer::ComputerOptions::from_environment())?;
        runtime.set_computer(Arc::new(computer));
        runtime.set_relay(crate::provider_relay::start(store.clone()).await?);
        crate::chat_features::prune_generated_images(&store).await;
        runtime.initialize().await;
        let scheduler_options = options.scheduler.unwrap_or(SchedulerOptions { now, ..SchedulerOptions::default() });
        let scheduler = RoutineScheduler::new(store.clone(), runtime.clone(), queue.clone(), scheduler_options);
        let mut context =
            HttpContext::new(store.clone(), runtime.clone(), scheduler.clone(), queue.clone(), default_allowed_hosts(&options.allowed_hosts), options.web_root);
        context.embedded_web = options.embedded_web;
        context.desktop_token = token;
        let context = Arc::new(context);
        Ok(Self { store, runtime, scheduler, queue, context })
    }

    /// Bind, start the scheduler, and serve until `shutdown` resolves; then close in Node order
    /// (stop scheduling, drain commands, cancel runs, flush state).
    pub async fn serve(self, listener: tokio::net::TcpListener, shutdown: impl Future<Output = ()> + Send + 'static) -> anyhow::Result<()> {
        let bound = listener.local_addr()?;
        if self.context.desktop_token.is_some() && bound.ip() != std::net::Ipv4Addr::LOCALHOST {
            anyhow::bail!("The desktop backend must bind to 127.0.0.1");
        }
        let port = bound.port();
        self.context.port.store(port, Ordering::SeqCst);
        self.runtime.set_origin(format!("http://127.0.0.1:{port}"));
        self.scheduler.start();
        let app = router(self.context.clone()).into_make_service_with_connect_info::<SocketAddr>();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let context = self.context.clone();
        let (store, runtime, scheduler, queue) = (self.store.clone(), self.runtime.clone(), self.scheduler.clone(), self.queue.clone());
        let closer = tokio::spawn(async move {
            shutdown.await;
            context.closing.store(true, Ordering::SeqCst);
            // Probes run outside the app queue. Stop admission and cancel them immediately,
            // then wait for their cleanup independently, including disconnected HTTP callers.
            // SSE stays open until run cancellation and the final state flush below complete.
            context.mcp_tests.stop();
            scheduler.close().await;
            queue.run(async {}).await;
            context.mcp_tests.wait().await;
            runtime.shutdown().await;
            if let Some(computer) = runtime.computer() {
                let _ = computer.stop(None).await;
            }
            let _ = store.flush().await;
            context.stopping.send_replace(true);
            let _ = stop_tx.send(());
        });
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = stop_rx.await;
            })
            .await?;
        let _ = closer.await;
        Ok(())
    }
}
