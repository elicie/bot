#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use grok_bot_model::*;
use grok_bot_store::Store;

pub fn time(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value).unwrap().with_timezone(&Utc)
}

pub struct Fixture {
    pub root: tempfile::TempDir,
    pub data_dir: PathBuf,
    pub workspace: PathBuf,
    pub store: Arc<Store>,
    pub current: Arc<Mutex<DateTime<Utc>>>,
    pub bot: String,
}

impl Fixture {
    pub async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("data");
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let current = Arc::new(Mutex::new(time("2026-01-01T00:00:00.000Z")));
        let store = Store::open(&data_dir, clock(&current)).await.unwrap();
        let bot = store.read(|state| state.disk().bots[0].id.clone());
        Self { root, data_dir, workspace, store, current, bot }
    }

    pub fn clock(&self) -> Clock {
        clock(&self.current)
    }

    pub fn now(&self) -> DateTime<Utc> {
        *self.current.lock().unwrap()
    }

    pub fn iso_now(&self) -> String {
        iso(self.now())
    }

    pub fn set_time(&self, value: &str) {
        *self.current.lock().unwrap() = time(value);
    }

    pub async fn reopen(&self) -> anyhow::Result<Arc<Store>> {
        Store::open(&self.data_dir, self.clock()).await
    }

    pub fn state_file(&self) -> PathBuf {
        self.data_dir.join("state.json")
    }

    pub fn disk_text(&self) -> String {
        std::fs::read_to_string(self.state_file()).unwrap()
    }

    pub fn disk_json(&self) -> serde_json::Value {
        serde_json::from_str(&self.disk_text()).unwrap()
    }

    pub fn bot_id(&self) -> String {
        self.bot.clone()
    }

    pub fn input(&self, schedule: RoutineSchedule) -> RoutineInput {
        RoutineInput { bot_id: self.bot_id(), name: "Routine".into(), prompt: "Routine prompt".into(), schedule, enabled: true }
    }

    pub fn routine(&self, id: &str) -> Routine {
        self.store.read(|state| state.routine(id).unwrap().clone())
    }

    pub fn runs(&self) -> Vec<RoutineRun> {
        self.store.read(|state| state.disk().routine_runs.clone())
    }

    pub fn conversation(&self, id: &str) -> Conversation {
        self.store.read(|state| state.conversation(id).unwrap().clone())
    }

    /// Like the Node fixtures' `store.changed()` after editing a conversation run.
    pub async fn set_run(&self, id: &str, run: RunState) {
        self.store
            .mutate(|state| {
                state.conversation_mut(id)?.run = run;
                Ok(())
            })
            .await
            .unwrap();
    }

    pub async fn start(&self, id: &str) {
        let run = RunState { id: new_id(), status: RunStatus::Running, started_at: Some(self.iso_now()), ..RunState::idle() };
        self.set_run(id, run).await;
    }

    pub async fn finish(&self, id: &str) {
        let now = self.iso_now();
        self.store
            .mutate(|state| {
                let conversation = state.conversation_mut(id)?;
                conversation.run.status = RunStatus::Completed;
                conversation.run.finished_at = Some(now);
                Ok(())
            })
            .await
            .unwrap();
    }

    /// The store side of one `RoutineScheduler.process` pass: claim due routines (recording 409/413
    /// as the scheduling error), then start the oldest queued routine or delivery when idle.
    /// Returns the started conversation.
    pub async fn tick(&self) -> HttpResult<Option<String>> {
        let now = self.now();
        self.store
            .mutate(|state| {
                state.assert_writable()?;
                let error = match state.claim_due_routines(now) {
                    Ok(()) => None,
                    Err(error) if error.status == 409 || error.status == 413 => Some(error.message),
                    Err(error) => return Err(error),
                };
                state.set_routine_scheduling_error(error);
                Ok(())
            })
            .await?;
        let work = self.store.read(|state| {
            if state.active().is_some() {
                return None;
            }
            let disk = state.disk();
            let index = |id: &str| disk.conversations.iter().position(|conversation| conversation.id == id).unwrap_or(usize::MAX);
            let mut work: Vec<(String, String)> = disk
                .routine_runs
                .iter()
                .filter(|run| {
                    run.result.is_none() && run.execution_run_id.is_none() && state.conversation(&run.conversation_id).unwrap().run.status == RunStatus::Idle
                })
                .map(|run| (run.created_at.clone(), run.conversation_id.clone()))
                .chain(
                    disk.bot_deliveries
                        .iter()
                        .filter(|delivery| delivery.status == DeliveryStatus::Queued)
                        .map(|delivery| (delivery.created_at.clone(), delivery.recipient_conversation_id.clone())),
                )
                .collect();
            work.sort_by(|a, b| a.0.cmp(&b.0).then(index(&a.1).cmp(&index(&b.1))));
            work.into_iter().next().map(|(_, id)| id)
        });
        if let Some(id) = &work {
            let persisted = self.disk_json();
            assert_eq!(persisted["conversations"].as_array().unwrap().iter().find(|c| c["id"] == id.as_str()).unwrap()["run"]["status"], "idle");
            self.start(id).await;
        }
        Ok(work)
    }
}

fn clock(current: &Arc<Mutex<DateTime<Utc>>>) -> Clock {
    let current = current.clone();
    Arc::new(move || *current.lock().unwrap())
}

pub fn write_json(path: &Path, value: &serde_json::Value) -> String {
    let bytes = serde_json::to_string(value).unwrap();
    std::fs::write(path, &bytes).unwrap();
    bytes
}

pub async fn assert_load_rejected(fixture: &Fixture, bytes: &str) {
    let error = match fixture.reopen().await {
        Ok(_) => panic!("state should be rejected: {bytes}"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("existing state.json was not replaced"), "{error:#}");
    assert_eq!(fixture.disk_text(), bytes);
}
