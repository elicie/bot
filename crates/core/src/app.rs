//! Backend composition. Port of `createBackend` in `server/app.ts`; used by the web server binary
//! and, later, the Tauri desktop app.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
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
    pub web_root: Option<PathBuf>,
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
        let now = options.now.unwrap_or_else(grok_bot_model::system_clock);
        let store = Store::open(&options.data_dir, now.clone()).await?;
        let queue = CommandQueue::new();
        let runtime_options = options.runtime.unwrap_or(RuntimeOptions {
            engine_root: options.engine_root.clone(),
            startup_timeout: Duration::from_secs(30),
            prompt_idle_timeout: Duration::from_secs(30),
            cancel_deadline: Duration::from_millis(800),
            worker_timeout: Duration::from_secs(5 * 60),
        });
        let runtime = Runtime::new(store.clone(), queue.clone(), runtime_options);
        let computer = crate::computer::ApplicationComputer::new(store.clone(), crate::computer::ComputerOptions::from_environment())?;
        runtime.set_computer(Arc::new(computer));
        runtime.set_relay(crate::provider_relay::start(store.clone()).await?);
        crate::chat_features::prune_generated_images(&store).await;
        runtime.initialize().await;
        let scheduler_options = options.scheduler.unwrap_or(SchedulerOptions { now, ..SchedulerOptions::default() });
        let scheduler = RoutineScheduler::new(store.clone(), runtime.clone(), queue.clone(), scheduler_options);
        let context = Arc::new(HttpContext {
            store: store.clone(),
            runtime: runtime.clone(),
            scheduler: scheduler.clone(),
            queue: queue.clone(),
            allowed_hosts: default_allowed_hosts(&options.allowed_hosts),
            port: AtomicU16::new(0),
            closing: AtomicBool::new(false),
            web_root: options.web_root,
            mcp_test_stopping: tokio::sync::watch::Sender::new(false),
            stopping: tokio::sync::watch::Sender::new(false),
        });
        Ok(Self { store, runtime, scheduler, queue, context })
    }

    /// Bind, start the scheduler, and serve until `shutdown` resolves; then close in Node order
    /// (stop scheduling, drain commands, cancel runs, flush state).
    pub async fn serve(self, listener: tokio::net::TcpListener, shutdown: impl Future<Output = ()> + Send + 'static) -> anyhow::Result<()> {
        let port = listener.local_addr()?.port();
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
            // Probes hold the command queue. Cancel them before draining it, while leaving SSE
            // open until run cancellation and the final state flush below have completed.
            context.mcp_test_stopping.send_replace(true);
            scheduler.close().await;
            queue.run(async {}).await;
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
