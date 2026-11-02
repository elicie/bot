//! `ApplicationComputer`: the single managed browser bound to one bot at a time.
//!
//! Starts and actions run one at a time and carry the cancellation generation they were admitted
//! under; Stop increments the generation, so work queued before a Stop never reaches the browser.
//! The latest PNG frame is cached locally and served only for its exact epoch and frame.

use std::sync::{Arc, Mutex, MutexGuard};

use grok_bot_model::{HttpError, HttpResult, new_id};
use grok_bot_store::Store;
use serde_json::{Value, json};

use super::{ComputerObservation, ComputerOptions, ComputerService, ComputerState, ComputerStatus, ManagedComputer};

struct CachedFrame {
    epoch: String,
    frame_id: String,
    png: Arc<Vec<u8>>,
}

#[derive(Default)]
struct Inner {
    owner_id: Option<String>,
    owner_token: Option<u64>,
    next_token: u64,
    frame: Option<CachedFrame>,
    version: u64,
}

pub struct ApplicationComputer {
    store: Arc<Store>,
    service: Arc<dyn ComputerService>,
    inner: Mutex<Inner>,
    tail: tokio::sync::Mutex<()>,
}

impl ApplicationComputer {
    pub fn new(store: Arc<Store>, options: ComputerOptions) -> HttpResult<Self> {
        Ok(Self::with_service(store, Arc::new(ManagedComputer::new(options)?)))
    }

    /// An application computer over another browser service (tests).
    pub fn with_service(store: Arc<Store>, service: Arc<dyn ComputerService>) -> Self {
        Self { store, service, inner: Mutex::new(Inner::default()), tail: tokio::sync::Mutex::new(()) }
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The cancellation generation; capture it when a request is admitted.
    pub fn generation(&self) -> u64 {
        self.inner().version
    }

    fn current(&self, generation: u64) -> HttpResult<()> {
        if generation != self.inner().version {
            return Err(HttpError::conflict("The managed browser was stopped or replaced. Submit a new request."));
        }
        Ok(())
    }

    fn owner(&self, id: &str) -> HttpResult<()> {
        self.store.read(|state| state.bot(id).map(|_| ()))?;
        let owner = self.inner().owner_id.clone();
        if owner.is_some_and(|owner| owner != id) && self.service.status().state != ComputerState::Stopped {
            return Err(HttpError::conflict("Another bot owns the managed browser. Stop that browser before switching owners."));
        }
        Ok(())
    }

    pub fn status(&self, id: &str) -> HttpResult<ComputerStatus> {
        self.owner(id)?;
        Ok(self.service.status())
    }

    pub async fn start(&self, id: &str, input: Value, generation: u64) -> HttpResult<ComputerStatus> {
        let _turn = self.tail.lock().await;
        self.current(generation)?;
        self.owner(id)?;
        if self.service.status().state != ComputerState::Stopped {
            return Err(HttpError::conflict("Stop the current browser before starting another."));
        }
        {
            let mut inner = self.inner();
            inner.owner_id = Some(id.to_owned());
            inner.next_token += 1;
            inner.owner_token = Some(inner.next_token);
            inner.frame = None;
        }
        let status = self.service.start(input).await?;
        self.current(generation)?;
        self.capture(id, generation, &status.epoch).await?;
        Ok(self.service.status())
    }

    /// Stop the browser. Not serialized: it interrupts a slow start or action.
    pub async fn stop(&self, id: Option<&str>) -> HttpResult<ComputerStatus> {
        if let Some(id) = id {
            self.owner(id)?;
        }
        let (generation, token) = {
            let mut inner = self.inner();
            inner.version += 1;
            inner.frame = None;
            (inner.version, inner.owner_token)
        };
        let value = self.service.stop().await;
        let mut inner = self.inner();
        if inner.version == generation && inner.owner_token == token {
            inner.owner_id = None;
            inner.owner_token = None;
        }
        Ok(value)
    }

    async fn capture(&self, id: &str, generation: u64, epoch: &str) -> HttpResult<ComputerObservation> {
        self.current(generation)?;
        self.owner(id)?;
        let status = self.service.status();
        if status.epoch != epoch {
            return Err(HttpError::conflict("The managed browser session changed. Observe again."));
        }
        let value = self.service.screenshot(json!({ "requestId": new_id(), "epoch": epoch, "frameId": status.frame_id })).await?;
        self.current(generation)?;
        self.inner().frame = Some(CachedFrame { epoch: value.frame.epoch.clone(), frame_id: value.frame.id.clone(), png: Arc::new(value.png) });
        Ok(ComputerObservation { status: value.status, frame: value.frame })
    }

    /// The cached PNG of exactly this epoch and frame.
    pub fn preview(&self, id: &str, epoch: &str, frame_id: &str) -> HttpResult<Arc<Vec<u8>>> {
        self.owner(id)?;
        let status = self.service.status();
        let inner = self.inner();
        let frame = match &inner.frame {
            Some(frame) if status.state == ComputerState::Running => frame,
            _ => return Err(HttpError::conflict("Start the browser before viewing a frame.")),
        };
        if epoch != status.epoch || Some(frame_id) != status.frame_id.as_deref() || epoch != frame.epoch || frame_id != frame.frame_id {
            return Err(HttpError::conflict("The browser preview frame is stale. Refresh its status."));
        }
        Ok(frame.png.clone())
    }

    /// Perform one action (`{ requestId, epoch, frameId, action }`) and capture the next frame.
    pub async fn perform(&self, id: &str, input: Value, generation: u64) -> HttpResult<ComputerObservation> {
        let _turn = self.tail.lock().await;
        self.current(generation)?;
        self.owner(id)?;
        let epoch = input.get("epoch").and_then(Value::as_str).unwrap_or_default().to_owned();
        self.service.perform(input).await?;
        self.capture(id, generation, &epoch).await
    }
}
