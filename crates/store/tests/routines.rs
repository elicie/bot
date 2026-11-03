//! Store-level port of `tests/routines.test.ts` (scheduler-loop behavior is covered by the
//! coordinator; `Fixture::tick` performs the store side of one scheduler pass).

mod common;

use common::{Fixture, assert_load_rejected, time, write_json};
use grok_bot_model::*;
use grok_bot_store::schedule::{latest_routine_time, next_routine_time, validate_routine_input};

fn interval(minutes: u32) -> RoutineSchedule {
    RoutineSchedule::Interval { minutes }
}

fn once(at: &str) -> RoutineSchedule {
    RoutineSchedule::Once { at: at.into() }
}

fn daily(time: &str, timezone: &str) -> RoutineSchedule {
    RoutineSchedule::Daily { time: time.into(), timezone: timezone.into() }
}

#[tokio::test]
async fn version_one_migration_retains_bots_settings_secrets_messages_and_sessions() {
    let f = Fixture::new().await;
    let workspace = f.workspace.to_string_lossy().into_owned();
    let settings = SettingsInput {
        base_url: "http://127.0.0.1:12345/v1".into(),
        model: "saved-model".into(),
        workspace,
        api_key: Some("legacy-private-key".into()),
        clear_api_key: None,
        max_concurrent_runs: None,
    };
    f.store.mutate(|s| s.update_settings(settings)).await.unwrap();
    let bot = f
        .store
        .mutate(|s| {
            s.add_bot(BotInput {
                name: "Saved bot".into(),
                color: Some("#123456".into()),
                instructions: Some("Saved instructions".into()),
                shape: None,
                pinned: None,
            })
        })
        .await
        .unwrap();
    let conversation = f.store.mutate(|s| s.add_conversation(&bot.id)).await.unwrap();
    let now = f.iso_now();
    f.store
        .mutate(|s| {
            let c = s.conversation_mut(&conversation.id)?;
            c.session_id = Some("saved-session".into());
            c.session_fingerprint = Some("a".repeat(64));
            c.messages.push(Message {
                id: new_id(),
                role: Role::User,
                text: "Saved message".into(),
                created_at: now,
                tools: vec![],
                bot_exchange_ids: None,
                source_bot_id: None,
                attachments: None,
                speaker_bot_id: None,
                report_delivery_ids: None,
            });
            c.run = RunState { id: "saved-run".into(), status: RunStatus::Completed, ..RunState::idle() };
            Ok(())
        })
        .await
        .unwrap();
    let mut before = f.disk_json();
    before.as_object_mut().unwrap().remove("routines");
    before.as_object_mut().unwrap().remove("routineRuns");
    write_json(&f.state_file(), &before);
    let reopened = f.reopen().await.unwrap();
    let after = f.disk_json();
    assert_eq!(after["bots"], before["bots"]);
    assert_eq!(after["settings"], before["settings"]);
    assert_eq!(after["conversations"], before["conversations"]);
    assert_eq!(after["routines"], serde_json::json!([]));
    assert_eq!(after["routineRuns"], serde_json::json!([]));
    assert!(after["revision"].as_u64() > before["revision"].as_u64());
    assert!(!serde_json::to_string(&reopened.snapshot()).unwrap().contains("legacy-private-key"));
}

#[tokio::test]
async fn routine_disk_references_unique_ids_and_single_pending_claims_are_validated_without_replacing_state() {
    let f = Fixture::new().await;
    let second = f
        .store
        .mutate(|s| s.add_bot(BotInput { name: "Other bot".into(), color: Some("#654321".into()), instructions: None, shape: None, pinned: None }))
        .await
        .unwrap();
    let routine = f.store.mutate(|s| s.add_routine(f.input(interval(1)))).await.unwrap();
    let run = f.store.mutate(|s| s.run_routine(&routine.id, f.now())).await.unwrap();
    let good = f.disk_json();
    let second_id = second.id.clone();
    type Corruption = Box<dyn Fn(&mut serde_json::Value)>;
    let corruptions: Vec<Corruption> = vec![
        Box::new(|s| {
            let copy = s["routines"][0].clone();
            s["routines"].as_array_mut().unwrap().push(copy);
        }),
        Box::new(|s| s["routines"][0]["id"] = s["bots"][0]["id"].clone()),
        Box::new(|s| s["routineRuns"][0]["id"] = s["conversations"][0]["id"].clone()),
        Box::new(|s| s["routines"][0]["botId"] = new_id().into()),
        Box::new(move |s| s["routineRuns"][0]["botId"] = second_id.clone().into()),
        Box::new(|s| s["routineRuns"][0]["conversationId"] = new_id().into()),
        Box::new(|s| s["routineRuns"][0]["routineId"] = s["bots"][0]["id"].clone()),
        Box::new(|s| s["routineRuns"][0]["executionRunId"] = new_id().into()),
        Box::new(|s| {
            let mut copy = s["routineRuns"][0].clone();
            copy["id"] = new_id().into();
            s["routineRuns"].as_array_mut().unwrap().push(copy);
        }),
        Box::new(|s| s["routines"] = serde_json::json!([])),
        Box::new(|s| {
            s["routines"][0].as_object_mut().unwrap().remove("nextRunAt");
        }),
        Box::new(|s| {
            let id = new_id();
            let mut conversation = s["conversations"][0].clone();
            conversation["id"] = id.clone().into();
            s["conversations"].as_array_mut().unwrap().push(conversation);
            let mut copy = s["routineRuns"][0].clone();
            copy["id"] = new_id().into();
            copy["conversationId"] = id.into();
            s["routineRuns"].as_array_mut().unwrap().push(copy);
        }),
        // Schema-level checks Node's zod `diskSchema` performed.
        Box::new(|s| s["routines"][0]["extra"] = true.into()),
        Box::new(|s| s["routines"][0]["schedule"] = serde_json::json!({ "kind": "interval", "minutes": 0 })),
        Box::new(|s| s["routineRuns"][0]["createdAt"] = "yesterday".into()),
        Box::new(|s| s["bots"][0]["id"] = "not-a-uuid".into()),
        Box::new(|s| s["bots"] = serde_json::json!([])),
    ];
    for corrupt in corruptions {
        let mut state = good.clone();
        corrupt(&mut state);
        let bytes = write_json(&f.state_file(), &state);
        assert_load_rejected(&f, &bytes).await;
    }
    assert_eq!(run.routine_id, routine.id);
}

#[tokio::test]
async fn schedule_validation_enforces_intervals_real_timezones_nonempty_fields_and_future_one_time_edits() {
    let f = Fixture::new().await;
    for schedule in [
        interval(0),
        interval(10081),
        daily("24:00", "UTC"),
        daily("9:00", "UTC"),
        daily("09:00", "Mars/Olympus"),
        daily("09:00", "+04:00"),
        daily("09:00", ""),
        once("tomorrow"),
        once("2026-01-02T12:00:00"),
    ] {
        let error = validate_routine_input(RoutineInput { schedule: schedule.clone(), ..f.input(interval(1)) }).unwrap_err();
        assert_eq!(error.status, 400, "{schedule:?}");
    }
    let blank = validate_routine_input(RoutineInput { name: "  ".into(), ..f.input(interval(1)) }).unwrap_err();
    assert_eq!(blank.message, "name: Too small: expected string to have >=1 characters");
    let blank = validate_routine_input(RoutineInput { prompt: "  ".into(), ..f.input(interval(1)) }).unwrap_err();
    assert_eq!(blank.message, "prompt: Too small: expected string to have >=1 characters");
    let combined = validate_routine_input(RoutineInput {
        bot_id: "x".into(),
        name: String::new(),
        prompt: String::new(),
        schedule: daily("24:00", "Nope"),
        enabled: true,
    })
    .unwrap_err();
    assert_eq!(
        combined.message,
        "botId: Invalid UUID; name: Too small: expected string to have >=1 characters; prompt: Too small: expected string to have >=1 characters; schedule.time: Use HH:mm in 24-hour time.; schedule.timezone: Choose a valid IANA timezone."
    );
    let normalized =
        validate_routine_input(RoutineInput { name: " Name ".into(), schedule: daily("09:00", " america/new_york "), ..f.input(interval(1)) }).unwrap();
    assert_eq!(normalized.name, "Name");
    assert_eq!(normalized.schedule, daily("09:00", "america/new_york"));
    assert!(validate_routine_input(f.input(daily("09:00", "US/Eastern"))).is_ok());

    let now = f.iso_now();
    assert_eq!(f.store.mutate(|s| s.add_routine(f.input(once(&now)))).await.unwrap_err().status, 400);
    let missing = RoutineInput { bot_id: new_id(), ..f.input(interval(1)) };
    assert_eq!(f.store.mutate(|s| s.add_routine(missing)).await.unwrap_err().status, 404);
    let routine = f.store.mutate(|s| s.add_routine(f.input(interval(10080)))).await.unwrap();
    let past = RoutineInput { enabled: false, ..f.input(once("2025-01-01T00:00:00Z")) };
    assert_eq!(f.store.mutate(|s| s.update_routine(&routine.id, past)).await.unwrap_err().status, 400);
    assert_eq!(f.routine(&routine.id).schedule, interval(10080));
}

#[tokio::test]
async fn one_time_deadline_claims_persist_before_start_disable_the_schedule_and_start_once() {
    let f = Fixture::new().await;
    let routine = f.store.mutate(|s| s.add_routine(f.input(once("2026-01-01T00:01:00Z")))).await.unwrap();
    assert_eq!(routine.next_run_at.as_deref(), Some("2026-01-01T00:01:00.000Z"));
    f.set_time("2026-01-01T00:00:59.999Z");
    assert_eq!(f.tick().await.unwrap(), None);
    f.set_time("2026-01-01T00:01:00Z");
    let started = f.tick().await.unwrap().unwrap();
    for _ in 0..20 {
        assert_eq!(f.tick().await.unwrap(), None);
    }
    let snapshot = f.store.snapshot();
    assert_eq!(snapshot.routine_runs.len(), 1);
    assert_eq!(snapshot.routine_runs[0].trigger, RoutineTrigger::Scheduled);
    assert_eq!(snapshot.routine_runs[0].scheduled_for, "2026-01-01T00:01:00.000Z");
    assert_eq!(snapshot.routine_runs[0].prompt, routine.prompt);
    assert_eq!(snapshot.conversations[0].run.status, RunStatus::Running);
    assert!(!snapshot.routines[0].enabled);
    assert_eq!(snapshot.routines[0].next_run_at, None);
    f.finish(&started).await;
    f.set_time("2026-01-02T00:00:00Z");
    assert_eq!(f.tick().await.unwrap(), None);
}

#[tokio::test]
async fn missed_interval_slots_coalesce_and_retain_their_phase() {
    let f = Fixture::new().await;
    let routine = f.store.mutate(|s| s.add_routine(f.input(interval(1)))).await.unwrap();
    f.set_time("2026-01-01T00:20:30Z");
    let started = f.tick().await.unwrap().unwrap();
    assert_eq!(f.runs().len(), 1);
    assert_eq!(f.runs()[0].scheduled_for, "2026-01-01T00:01:00.000Z");
    assert_eq!(f.routine(&routine.id).next_run_at.as_deref(), Some("2026-01-01T00:21:00.000Z"));
    f.set_time("2026-01-01T00:40:30Z");
    assert_eq!(f.tick().await.unwrap(), None);
    assert_eq!(f.routine(&routine.id).next_run_at.as_deref(), Some("2026-01-01T00:41:00.000Z"));
    f.set_time("2026-01-01T01:00:30Z");
    f.finish(&started).await;
    assert_eq!(f.tick().await.unwrap(), None);
    assert_eq!(f.routine(&routine.id).next_run_at.as_deref(), Some("2026-01-01T01:01:00.000Z"));
    f.set_time("2026-01-01T01:01:00Z");
    assert!(f.tick().await.unwrap().is_some());
    assert_eq!(f.runs().len(), 2);
}

#[test]
fn daily_iana_schedules_handle_spring_gaps_and_fall_repeats_without_two_daily_runs() {
    let spring = daily("02:30", "America/New_York");
    let fall = daily("01:30", "America/New_York");
    let next = |schedule: &RoutineSchedule, now: &str| next_routine_time(schedule, time(now), None).unwrap();
    assert_eq!(next(&spring, "2026-03-08T06:59:00Z"), "2026-03-08T07:30:00.000Z");
    assert_eq!(next(&spring, "2026-03-08T07:30:00Z"), "2026-03-09T06:30:00.000Z");
    assert_eq!(next(&fall, "2026-11-01T04:00:00Z"), "2026-11-01T05:30:00.000Z");
    assert_eq!(next(&fall, "2026-11-01T05:30:00Z"), "2026-11-02T06:30:00.000Z");
    assert_eq!(next(&fall, "2026-11-01T06:15:00Z"), "2026-11-02T06:30:00.000Z");
    assert_eq!(next(&daily("09:15", "Asia/Kathmandu"), "2026-01-01T00:00:00Z"), "2026-01-01T03:30:00.000Z");
    assert_eq!(latest_routine_time(&spring, "2026-03-08T07:30:00Z", time("2026-03-08T07:30:00Z")), time("2026-03-08T07:30:00Z").timestamp_millis());
    // Zones whose transition skips local midnight skip that day's occurrence, like cron-parser.
    assert_eq!(next(&daily("00:30", "America/Santiago"), "2026-09-05T12:00:00Z"), "2026-09-07T03:30:00.000Z");
    // A midnight repeat fires at its second occurrence.
    assert_eq!(next(&daily("00:30", "America/Havana"), "2026-10-31T12:00:00Z"), "2026-11-01T05:30:00.000Z");
    // cron-parser exhausts its loop limit here; the Node route answered 500.
    assert_eq!(next_routine_time(&daily("03:00", "Australia/Lord_Howe"), time("2026-04-03T16:46:00Z"), None).unwrap_err().status, 500);
    // Interval and once semantics.
    assert_eq!(next_routine_time(&interval(5), time("2026-01-01T00:05:30Z"), Some("2026-01-01T00:05:00.000Z")).unwrap(), "2026-01-01T00:10:00.000Z");
    assert_eq!(next_routine_time(&interval(5), time("2026-01-01T00:00:00Z"), Some("2026-01-01T00:05:00.000Z")).unwrap(), "2026-01-01T00:10:00.000Z");
    assert_eq!(next_routine_time(&interval(5), time("2026-01-01T00:00:01Z"), None).unwrap(), "2026-01-01T00:05:01.000Z");
    assert_eq!(next_routine_time(&once("2026-01-05T09:00:00+09:00"), time("2026-01-01T00:00:00Z"), None).unwrap(), "2026-01-05T00:00:00.000Z");
    assert_eq!(latest_routine_time(&interval(5), "2026-01-01T00:05:00.000Z", time("2026-01-01T00:21:00Z")), time("2026-01-01T00:20:00Z").timestamp_millis());
}

#[tokio::test]
async fn daily_scheduler_ticks_fire_through_spring_gaps_and_once_across_fall_repeats() {
    let cases = [
        ("2026-03-08T05:00:00Z", "02:30", "2026-03-08T07:29:59Z", "2026-03-08T07:30:00Z", "2026-03-08T08:00:00Z", "2026-03-09T06:30:00.000Z"),
        ("2026-11-01T04:00:00Z", "01:30", "2026-11-01T05:29:59Z", "2026-11-01T05:30:00Z", "2026-11-01T06:30:00Z", "2026-11-02T06:30:00.000Z"),
    ];
    for (start, at, before, due, later, next) in cases {
        let f = Fixture::new().await;
        f.set_time(start);
        let routine = f.store.mutate(|s| s.add_routine(f.input(daily(at, "America/New_York")))).await.unwrap();
        assert_eq!(f.routine(&routine.id).next_run_at, Some(iso(time(due))));
        f.set_time(before);
        assert_eq!(f.tick().await.unwrap(), None);
        f.set_time(due);
        let started = f.tick().await.unwrap().unwrap();
        assert_eq!(f.conversation(&started).run.status, RunStatus::Running);
        f.finish(&started).await;
        f.set_time(later);
        assert_eq!(f.tick().await.unwrap(), None);
        assert_eq!(f.routine(&routine.id).next_run_at.as_deref(), Some(next));
    }
}

#[tokio::test]
async fn manual_runs_queue_behind_contention_preserve_recurring_deadlines_and_older_claims_dispatch_first() {
    let f = Fixture::new().await;
    let blocker = f.store.mutate(|s| s.add_conversation(&f.bot_id())).await.unwrap();
    f.start(&blocker.id).await;
    let first = f.store.mutate(|s| s.add_routine(RoutineInput { name: "First".into(), ..f.input(interval(5)) })).await.unwrap();
    let second = f.store.mutate(|s| s.add_routine(RoutineInput { name: "Second".into(), ..f.input(interval(5)) })).await.unwrap();
    let deadline = f.routine(&first.id).next_run_at;
    let first_run = f.store.mutate(|s| s.run_routine(&first.id, f.now())).await.unwrap();
    assert_eq!(f.tick().await.unwrap(), None);
    f.set_time("2026-01-01T00:00:01Z");
    let second_run = f.store.mutate(|s| s.run_routine(&second.id, f.now())).await.unwrap();
    assert_eq!(f.tick().await.unwrap(), None);
    assert_eq!(f.conversation(&first_run.conversation_id).run.status, RunStatus::Idle);
    assert_eq!(f.routine(&first.id).next_run_at, deadline);
    assert_eq!(f.store.mutate(|s| s.run_routine(&first.id, f.now())).await.unwrap_err().status, 409);
    f.store.mutate(|s| s.set_routine_enabled(&first.id, false)).await.unwrap();
    f.set_time("2026-01-01T00:20:00Z");
    assert_eq!(f.tick().await.unwrap(), None);
    assert_eq!(f.runs().len(), 2);
    assert_eq!(f.routine(&second.id).next_run_at.as_deref(), Some("2026-01-01T00:25:00.000Z"));
    f.finish(&blocker.id).await;
    assert_eq!(f.tick().await.unwrap(), Some(first_run.conversation_id.clone()));
    f.finish(&first_run.conversation_id).await;
    assert_eq!(f.tick().await.unwrap(), Some(second_run.conversation_id.clone()));
}

#[tokio::test]
async fn manual_runs_at_a_recurring_deadline_coalesce_the_due_slot_without_shifting_the_phase() {
    let f = Fixture::new().await;
    let routine = f.store.mutate(|s| s.add_routine(f.input(interval(5)))).await.unwrap();
    f.set_time("2026-01-01T00:05:30Z");
    let run = f.store.mutate(|s| s.run_routine(&routine.id, f.now())).await.unwrap();
    assert_eq!(run.trigger, RoutineTrigger::Manual);
    let started = f.tick().await.unwrap();
    assert_eq!(started.as_deref(), Some(run.conversation_id.as_str()));
    assert_eq!(f.runs().len(), 1);
    assert_eq!(f.routine(&routine.id).next_run_at.as_deref(), Some("2026-01-01T00:10:00.000Z"));
    f.finish(&run.conversation_id).await;
    assert_eq!(f.tick().await.unwrap(), None);
}

#[tokio::test]
async fn pause_blocks_future_claims_resume_uses_a_fresh_deadline_and_expired_one_time_schedules_cannot_resume() {
    let f = Fixture::new().await;
    let routine = f.store.mutate(|s| s.add_routine(f.input(interval(5)))).await.unwrap();
    f.store.mutate(|s| s.set_routine_enabled(&routine.id, false)).await.unwrap();
    // Setting the same value is a no-op that does not commit.
    let revision = f.store.snapshot().revision;
    f.store.mutate(|s| s.set_routine_enabled(&routine.id, false)).await.unwrap();
    assert_eq!(f.store.snapshot().revision, revision);
    f.set_time("2026-01-01T01:00:00Z");
    assert_eq!(f.tick().await.unwrap(), None);
    // Nothing due: the claim does not commit either.
    assert_eq!(f.store.snapshot().revision, revision);
    f.store.mutate(|s| s.set_routine_enabled(&routine.id, true)).await.unwrap();
    assert_eq!(f.routine(&routine.id).next_run_at.as_deref(), Some("2026-01-01T01:05:00.000Z"));
    assert_eq!(f.tick().await.unwrap(), None);
    let once = f.store.mutate(|s| s.add_routine(RoutineInput { enabled: false, ..f.input(once("2026-01-01T01:01:00Z")) })).await.unwrap();
    f.set_time("2026-01-01T01:05:00Z");
    let error = f.store.mutate(|s| s.set_routine_enabled(&once.id, true)).await.unwrap_err();
    assert_eq!(error.status, 400);
    assert_eq!(error.message, "A one-time schedule must be in the future. Edit its time before enabling it.");
    let started = f.tick().await.unwrap().unwrap();
    f.store.mutate(|s| s.set_routine_enabled(&routine.id, false)).await.unwrap();
    assert_eq!(f.conversation(&started).run.status, RunStatus::Running);
}

#[tokio::test]
async fn unrelated_routine_edits_stay_usable_while_own_edits_deletion_and_bot_mutations_are_guarded() {
    let f = Fixture::new().await;
    let first = f.store.mutate(|s| s.add_routine(f.input(interval(5)))).await.unwrap();
    let run = f.store.mutate(|s| s.run_routine(&first.id, f.now())).await.unwrap();
    f.tick().await.unwrap();
    let unrelated = f.store.mutate(|s| s.add_routine(f.input(interval(2)))).await.unwrap();
    let edit = RoutineInput { name: "Edited unrelated".into(), ..f.input(daily("09:00", "UTC")) };
    f.store.mutate(|s| s.update_routine(&unrelated.id, edit)).await.unwrap();
    assert_eq!(f.routine(&unrelated.id).name, "Edited unrelated");
    let error = f.store.mutate(|s| s.update_routine(&first.id, f.input(interval(7)))).await.unwrap_err();
    assert_eq!((error.status, error.message.as_str()), (409, "Wait for or cancel this routine’s queued or active run first."));
    assert_eq!(f.store.mutate(|s| s.delete_routine(&first.id)).await.unwrap_err().status, 409);
    let patch = BotPatch { name: Some("Blocked".into()), ..BotPatch::default() };
    let error = f.store.mutate(|s| s.update_bot(&first.bot_id, patch)).await.unwrap_err();
    assert_eq!((error.status, error.message.as_str()), (409, "Stop the active run before changing settings, bots, or files."));
    f.finish(&run.conversation_id).await;
    f.store.mutate(|s| s.delete_routine(&first.id)).await.unwrap();
    assert_eq!(f.runs().len(), 1);
    assert_eq!(f.store.read(|s| s.disk().conversations.len()), 1);
    let reopened = f.reopen().await.unwrap();
    reopened.read(|s| {
        assert!(s.routine(&first.id).is_err());
        assert_eq!(s.disk().routine_runs[0].routine_name, first.name);
        assert_eq!(s.conversation(&run.conversation_id).unwrap().run.status, RunStatus::Completed);
    });
}

#[tokio::test]
async fn startup_failures_create_visible_terminal_errors_without_leaked_secrets_or_retries() {
    let f = Fixture::new().await;
    let secret = "private-fixture-provider-key";
    let settings = SettingsInput {
        base_url: "http://127.0.0.1:12345/v1".into(),
        model: "fixture".into(),
        workspace: f.workspace.to_string_lossy().into_owned(),
        api_key: Some(secret.into()),
        clear_api_key: None,
        max_concurrent_runs: None,
    };
    f.store.mutate(|s| s.update_settings(settings)).await.unwrap();
    let input = RoutineInput { name: format!("Name {secret}"), prompt: format!("Prompt {secret}"), ..f.input(once("2026-01-01T00:01:00Z")) };
    let routine = f.store.mutate(|s| s.add_routine(input)).await.unwrap();
    f.set_time("2026-01-01T00:01:00Z");
    let now = f.now();
    f.store.mutate(|s| s.claim_due_routines(now)).await.unwrap();
    let run = f.runs()[0].clone();
    let message = format!("Invalid provider {secret}");
    f.store.mutate(|s| s.fail_routine_run(&run.id, &message, now)).await.unwrap();
    assert_eq!(f.tick().await.unwrap(), None);
    let conversation = f.conversation(&run.conversation_id);
    assert_eq!(conversation.run.status, RunStatus::Error);
    assert_eq!(conversation.run.id, run.id);
    assert_eq!(conversation.run.started_at, Some(f.iso_now()));
    assert_eq!(conversation.run.finished_at, Some(f.iso_now()));
    assert!(conversation.messages.is_empty());
    assert_eq!(conversation.run.error.as_deref(), Some("Invalid provider [redacted]"));
    assert!(!serde_json::to_string(&f.store.snapshot()).unwrap().contains(secret));
    assert!(!f.routine(&routine.id).enabled);
    let persisted = f.disk_json();
    assert_eq!(persisted["settings"]["apiKey"], secret);
    assert!(!persisted["conversations"][0]["run"]["error"].as_str().unwrap().contains(secret));
    assert_eq!(f.runs()[0].result.as_ref().unwrap().status, RoutineResultStatus::Error);
    f.store.mutate(|s| s.delete_routine(&routine.id)).await.unwrap();
}

#[tokio::test]
async fn public_routine_and_conversation_link_uuids_survive_secret_substring_redaction() {
    let f = Fixture::new().await;
    let routine = f.store.mutate(|s| s.add_routine(f.input(interval(5)))).await.unwrap();
    let key = routine.id[..8].to_owned();
    let settings = SettingsInput {
        base_url: "http://127.0.0.1:12345/v1".into(),
        model: "fixture".into(),
        workspace: f.workspace.to_string_lossy().into_owned(),
        api_key: Some(key.clone()),
        clear_api_key: None,
        max_concurrent_runs: None,
    };
    f.store.mutate(|s| s.update_settings(settings)).await.unwrap();
    let run = f.store.mutate(|s| s.run_routine(&routine.id, f.now())).await.unwrap();
    let execution_run_id = format!("{key}-1234-4123-8123-123456789abc");
    f.set_run(&run.conversation_id, RunState { id: execution_run_id.clone(), status: RunStatus::Running, started_at: Some(f.iso_now()), ..RunState::idle() })
        .await;
    let state = f.store.snapshot();
    assert_eq!(state.routines[0].id, routine.id);
    assert_eq!(state.routine_runs[0].routine_id, routine.id);
    assert_eq!(state.routine_runs[0].conversation_id, run.conversation_id);
    assert_eq!(state.routine_runs[0].execution_run_id.as_deref(), Some(execution_run_id.as_str()));
    assert_eq!(state.conversations[0].run.id, execution_run_id);
}

#[tokio::test]
async fn queued_claims_survive_restart_while_started_claims_become_interrupted_and_never_replay() {
    let f = Fixture::new().await;
    let first = f.store.mutate(|s| s.add_routine(f.input(once("2026-01-01T00:01:00Z")))).await.unwrap();
    let second = f.store.mutate(|s| s.add_routine(f.input(once("2026-01-01T00:02:00Z")))).await.unwrap();
    let active = f.store.mutate(|s| s.run_routine(&first.id, f.now())).await.unwrap();
    let queued = f.store.mutate(|s| s.run_routine(&second.id, f.now())).await.unwrap();
    f.set_run(&active.conversation_id, RunState { id: new_id(), status: RunStatus::AwaitingPermission, started_at: Some(f.iso_now()), ..RunState::idle() })
        .await;
    f.store.flush().await.unwrap();
    let reopened = f.reopen().await.unwrap();
    reopened.read(|s| {
        let interrupted = s.conversation(&active.conversation_id).unwrap();
        assert_eq!(interrupted.run.status, RunStatus::Interrupted);
        assert_eq!(interrupted.run.error.as_deref(), Some("The server stopped before this run finished."));
        assert_eq!(s.conversation(&queued.conversation_id).unwrap().run.status, RunStatus::Idle);
        let result = s.disk().routine_runs.iter().find(|run| run.id == active.id).unwrap().result.clone().unwrap();
        assert_eq!(result.status, RoutineResultStatus::Interrupted);
    });
    let g = Fixture { store: reopened, ..f };
    g.set_time("2026-01-02T00:00:00Z");
    assert_eq!(g.tick().await.unwrap(), Some(queued.conversation_id.clone()));
    assert_eq!(g.tick().await.unwrap(), None);
    assert_eq!(g.runs().len(), 2);
    assert_eq!(g.conversation(&active.conversation_id).run.status, RunStatus::Interrupted);
}

#[tokio::test]
async fn frozen_routine_results_survive_follow_up_chat_deletion_restart_and_future_schedules() {
    let f = Fixture::new().await;
    let routine = f.store.mutate(|s| s.add_routine(f.input(interval(1)))).await.unwrap();
    let run = f.store.mutate(|s| s.run_routine(&routine.id, f.now())).await.unwrap();
    f.tick().await.unwrap();
    f.set_time("2026-01-01T00:00:30Z");
    f.finish(&run.conversation_id).await;
    let frozen = f.runs()[0].clone();
    let result = frozen.result.clone().unwrap();
    assert_eq!(result.status, RoutineResultStatus::Completed);
    f.set_time("2026-01-01T00:01:00Z");
    f.start(&run.conversation_id).await;
    let edit = RoutineInput { name: "Edited after routine completion".into(), ..f.input(interval(1)) };
    f.store.mutate(|s| s.update_routine(&routine.id, edit)).await.unwrap();
    f.store.mutate(|s| s.delete_routine(&routine.id)).await.unwrap();
    let reopened = f.reopen().await.unwrap();
    reopened.read(|s| {
        assert_eq!(s.disk().routine_runs[0].result.as_ref(), Some(&result));
        assert_eq!(s.disk().routine_runs[0].execution_run_id, frozen.execution_run_id);
        assert_eq!(s.conversation(&run.conversation_id).unwrap().run.status, RunStatus::Interrupted);
        assert!(s.disk().routines.is_empty());
    });
    let recurring = f.store.mutate(|s| s.add_routine(f.input(interval(1)))).await.unwrap();
    f.finish(&run.conversation_id).await;
    let recurring_run = f.store.mutate(|s| s.run_routine(&recurring.id, f.now())).await.unwrap();
    f.tick().await.unwrap();
    f.set_time("2026-01-01T00:01:30Z");
    f.finish(&recurring_run.conversation_id).await;
    f.set_time("2026-01-01T00:05:00Z");
    f.set_run(&recurring_run.conversation_id, RunState { id: new_id(), status: RunStatus::Completed, finished_at: Some(f.iso_now()), ..RunState::idle() })
        .await;
    f.tick().await.unwrap();
    assert_eq!(f.runs().iter().filter(|item| item.routine_id == recurring.id).count(), 2);
    let recurring_result = f.runs().into_iter().find(|item| item.id == recurring_run.id).unwrap().result.unwrap();
    assert_eq!(recurring_result.finished_at, "2026-01-01T00:01:30.000Z");
}

#[tokio::test]
async fn failed_claim_persistence_marks_the_store_unwritable_and_the_runtime_unavailable() {
    let f = Fixture::new().await;
    f.store.mutate(|s| s.add_routine(f.input(once("2026-01-01T00:01:00Z")))).await.unwrap();
    let mut events = f.store.subscribe();
    std::fs::rename(f.state_file(), f.data_dir.join("saved-state.json")).unwrap();
    std::fs::create_dir(f.state_file()).unwrap();
    f.set_time("2026-01-01T00:01:00Z");
    let error = f.tick().await.unwrap_err();
    assert_eq!(error.status, 500);
    std::fs::remove_dir(f.state_file()).unwrap();
    std::fs::rename(f.data_dir.join("saved-state.json"), f.state_file()).unwrap();
    let error = f.tick().await.unwrap_err();
    assert_eq!((error.status, error.message.as_str()), (503, "State could not be persisted. Restart after checking the data directory."));
    assert_eq!(f.store.flush().await.unwrap_err().status, 503);
    let runtime = f.store.snapshot().runtime;
    assert!(!runtime.available);
    assert_eq!(runtime.error.as_deref(), Some("Application state could not be saved. Check the data directory before continuing."));
    let failure = events.recv().await.unwrap();
    assert!(!failure.runtime.available);
    // The claimed run stays in memory only; the saved file still has none.
    assert_eq!(f.disk_json()["routineRuns"], serde_json::json!([]));
    // Later commits keep failing without touching the file.
    let before = f.disk_text();
    assert_eq!(
        f.store
            .mutate(|s| {
                s.disk_mut();
                Ok(())
            })
            .await
            .unwrap_err()
            .status,
        500
    );
    assert_eq!(f.disk_text(), before);
}

#[tokio::test]
async fn routine_count_is_bounded_and_oversize_mutations_leave_existing_state_untouched() {
    let f = Fixture::new().await;
    let routine = f.store.mutate(|s| s.add_routine(f.input(interval(1)))).await.unwrap();
    f.store
        .mutate(|s| {
            for _ in 1..1000 {
                s.disk_mut().routines.push(Routine { id: new_id(), ..routine.clone() });
            }
            Ok(())
        })
        .await
        .unwrap();
    let before = f.disk_text();
    let error = f.store.mutate(|s| s.add_routine(f.input(interval(1)))).await.unwrap_err();
    assert_eq!((error.status, error.message.as_str()), (409, "The application supports at most 1000 routines."));
    assert_eq!(f.store.read(|s| s.disk().routines.len()), 1000);
    assert_eq!(f.disk_text(), before);
    let large = Conversation {
        id: new_id(),
        bot_id: routine.bot_id.clone(),
        title: "Large retained transcript".into(),
        messages: vec![Message {
            id: new_id(),
            role: Role::Assistant,
            text: "x".repeat(64 * 1024 * 1024),
            created_at: f.iso_now(),
            tools: vec![],
            bot_exchange_ids: None,
            source_bot_id: None,
            attachments: None,
            speaker_bot_id: None,
            report_delivery_ids: None,
        }],
        session_id: None,
        session_fingerprint: None,
        run: RunState { status: RunStatus::Completed, ..RunState::idle() },
        updated_at: f.iso_now(),
        group_id: None,
        thread: None,
        group_round: None,
        member_sessions: None,
        canvas: None,
    };
    let edit = RoutineInput { name: "Would exceed capacity".into(), ..f.input(interval(1)) };
    let error = f
        .store
        .mutate(|s| {
            // Like the Node test, the oversized transcript is in memory before the routine edit.
            s.disk_mut().conversations.push(large);
            s.update_routine(&routine.id, edit)
        })
        .await
        .unwrap_err();
    assert_eq!(error.status, 413);
    assert!(error.message.contains("64 MiB"));
    assert_eq!(f.routine(&routine.id).name, "Routine");
    assert_eq!(f.disk_text(), before);
    // Committing the oversized state fails at the write and leaves the file untouched.
    assert_eq!(
        f.store
            .mutate(|s| {
                s.disk_mut();
                Ok(())
            })
            .await
            .unwrap_err()
            .status,
        500
    );
    assert_eq!(f.disk_text(), before);
    assert_eq!(f.store.read(|s| s.assert_writable()).unwrap_err().status, 503);
}

#[tokio::test]
async fn history_capacity_rejects_new_claims_without_pruning_or_partial_state() {
    let f = Fixture::new().await;
    let routine = f.store.mutate(|s| s.add_routine(f.input(interval(1)))).await.unwrap();
    let other =
        f.store.mutate(|s| s.add_routine(RoutineInput { name: "Unadmitted due routine".into(), ..f.input(once("2026-01-01T00:01:00Z")) })).await.unwrap();
    let original = f.store.mutate(|s| s.run_routine(&routine.id, f.now())).await.unwrap();
    f.tick().await.unwrap();
    f.finish(&original.conversation_id).await;
    f.store
        .mutate(|s| {
            let template = s.conversation(&original.conversation_id)?.clone();
            let run = s.disk().routine_runs[0].clone();
            let disk = s.disk_mut();
            for _ in 1..9999 {
                let conversation_id = new_id();
                disk.conversations.push(Conversation { id: conversation_id.clone(), ..template.clone() });
                disk.routine_runs.push(RoutineRun { id: new_id(), conversation_id, ..run.clone() });
            }
            Ok(())
        })
        .await
        .unwrap();
    // The 10000th run is admitted manually.
    f.set_time("2026-01-01T00:01:00Z");
    let accepted = f.store.mutate(|s| s.run_routine(&routine.id, f.now())).await.unwrap();
    assert_eq!(f.runs().len(), 10000);
    assert_eq!(f.tick().await.unwrap(), Some(accepted.conversation_id.clone()));
    assert!(f.store.snapshot().routine_scheduling_error.unwrap_or_default().contains("history is full"));
    assert!(f.routine(&other.id).enabled);
    assert_eq!(f.routine(&other.id).next_run_at.as_deref(), Some("2026-01-01T00:01:00.000Z"));
    f.finish(&accepted.conversation_id).await;
    let count = f.store.read(|s| s.disk().conversations.len());
    let error = f.store.mutate(|s| s.run_routine(&routine.id, f.now())).await.unwrap_err();
    assert_eq!((error.status, error.message.as_str()), (409, "Routine history is full. Existing history and conversations are retained."));
    assert_eq!(f.store.read(|s| s.disk().conversations.len()), count);
    assert_eq!(f.runs().len(), 10000);
    assert_eq!(f.tick().await.unwrap(), None);
    assert_eq!(f.store.read(|s| s.disk().conversations.len()), count);
    f.store.mutate(|s| s.set_routine_enabled(&other.id, false)).await.unwrap();
    f.set_time("2026-01-01T00:01:30Z");
    f.store.mutate(|s| s.set_routine_enabled(&routine.id, false)).await.unwrap();
    f.tick().await.unwrap();
    assert_eq!(f.store.snapshot().routine_scheduling_error, None);
}
