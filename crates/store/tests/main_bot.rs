//! Main bot ("메인 Bot") rules: the default for existing data, choosing, persistence, run gating,
//! load validation, and reassignment when the main bot is deleted.

mod common;

use common::{Fixture, assert_load_rejected, write_json};
use grok_bot_model::*;

fn bot_input(name: &str) -> BotInput {
    BotInput { name: name.into(), color: None, instructions: None, shape: None, pinned: None }
}

async fn add_bot(f: &Fixture, name: &str) -> String {
    let input = bot_input(name);
    f.store.mutate(|s| s.add_bot(input)).await.unwrap().id
}

fn bot(f: &Fixture, id: &str) -> Bot {
    f.store.read(|s| s.bot(id).unwrap().clone())
}

fn main_bot(f: &Fixture) -> Option<String> {
    f.store.read(|s| s.main_bot_id().map(str::to_owned))
}

#[tokio::test]
async fn the_first_bot_is_main_by_default_without_rewriting_existing_data() {
    let f = Fixture::new().await;
    let first = f.bot_id();
    let second = add_bot(&f, "Second").await;
    assert_eq!(main_bot(&f).as_deref(), Some(first.as_str()));
    assert!(!f.disk_text().contains("mainBotId"), "the default is not written to state.json");
    let public = serde_json::to_value(f.store.snapshot()).unwrap();
    assert!(public.get("mainBotId").is_none(), "the snapshot mirrors state.json: absent means the first bot");

    // Choosing the bot that already is main changes nothing.
    let revision = f.store.snapshot().revision;
    let target = first.clone();
    f.store.mutate(move |s| s.set_main_bot(&target)).await.unwrap();
    assert_eq!(f.store.snapshot().revision, revision);
    assert!(!f.disk_text().contains("mainBotId"));

    // Existing data reopened: still the first bot, still unwritten.
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.read(|s| s.main_bot_id().map(str::to_owned)).as_deref(), Some(first.as_str()));
    assert_eq!(reopened.snapshot().main_bot_id, None);
    assert_ne!(first, second);
}

#[tokio::test]
async fn choosing_a_main_bot_persists_exactly_one_and_keeps_the_sidebar_stable() {
    let f = Fixture::new().await;
    let first = f.bot_id();
    let second = add_bot(&f, "Second").await;
    let third = add_bot(&f, "Third").await;
    // The new main bot was hidden and unpinned; choosing it shows and pins it.
    let patch = BotPatch { pinned: Some(false), hidden: Some(true), ..BotPatch::default() };
    let target = second.clone();
    f.store.mutate(move |s| s.update_bot(&target, patch)).await.unwrap();

    let target = second.clone();
    f.store.mutate(move |s| s.set_main_bot(&target)).await.unwrap();
    assert_eq!(main_bot(&f).as_deref(), Some(second.as_str()));
    assert_eq!(f.disk_json()["mainBotId"], second.as_str());
    let chosen = bot(&f, &second);
    assert_eq!((chosen.pinned, chosen.hidden), (None, None), "pinned by default as the main bot, and shown");
    assert_eq!(bot(&f, &first).pinned, Some(true), "the previous main bot stays pinned");
    assert_eq!(bot(&f, &third).pinned, None);

    // Changing again: the explicitly pinned/unpinned state of the previous main bot is kept as is
    // when it was set explicitly.
    let patch = BotPatch { pinned: Some(false), ..BotPatch::default() };
    let target = first.clone();
    f.store.mutate(move |s| s.update_bot(&target, patch)).await.unwrap();
    let target = third.clone();
    f.store.mutate(move |s| s.set_main_bot(&target)).await.unwrap();
    assert_eq!(bot(&f, &second).pinned, Some(true));
    assert_eq!(bot(&f, &first).pinned, Some(false));
    let snapshot = f.store.snapshot();
    assert_eq!(snapshot.main_bot_id.as_deref(), Some(third.as_str()));

    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.snapshot().main_bot_id.as_deref(), Some(third.as_str()));

    let missing = new_id();
    assert_eq!(f.store.mutate(move |s| s.set_main_bot(&missing)).await.unwrap_err().status, 404);
}

#[tokio::test]
async fn choosing_a_main_bot_is_refused_during_runs() {
    let f = Fixture::new().await;
    let second = add_bot(&f, "Second").await;
    let bot_id = f.bot_id();
    let conversation = f.store.mutate(move |s| s.add_conversation(&bot_id)).await.unwrap().id;
    f.start(&conversation).await;
    let target = second.clone();
    let error = f.store.mutate(move |s| s.set_main_bot(&target)).await.unwrap_err();
    assert_eq!((error.status, error.message.as_str()), (409, "Stop the active run before changing settings, bots, or files."));
    assert_eq!(main_bot(&f).as_deref(), Some(f.bot_id().as_str()));
    f.finish(&conversation).await;
    let target = second.clone();
    f.store.mutate(move |s| s.set_main_bot(&target)).await.unwrap();
    assert_eq!(main_bot(&f).as_deref(), Some(second.as_str()));
}

#[tokio::test]
async fn deleting_the_main_bot_makes_the_first_remaining_bot_main() {
    let f = Fixture::new().await;
    let first = f.bot_id();
    let second = add_bot(&f, "Second").await;
    let third = add_bot(&f, "Third").await;

    // Explicit main bot deleted: the choice is cleared and the first remaining bot is main.
    let target = third.clone();
    f.store.mutate(move |s| s.set_main_bot(&target)).await.unwrap();
    let target = third.clone();
    f.store.mutate(move |s| s.delete_bot(&target)).await.unwrap();
    assert_eq!(main_bot(&f).as_deref(), Some(first.as_str()));
    assert!(!f.disk_text().contains("mainBotId"));

    // Default main bot (the first bot) deleted: the next bot becomes the first and main.
    let target = first.clone();
    f.store.mutate(move |s| s.delete_bot(&target)).await.unwrap();
    assert_eq!(main_bot(&f).as_deref(), Some(second.as_str()));
    // Deleting a bot that is not main keeps the choice.
    let fourth = add_bot(&f, "Fourth").await;
    let target = fourth.clone();
    f.store.mutate(move |s| s.set_main_bot(&target)).await.unwrap();
    let target = second.clone();
    f.store.mutate(move |s| s.delete_bot(&target)).await.unwrap();
    assert_eq!(main_bot(&f).as_deref(), Some(fourth.as_str()));
    // The last bot (also the main bot) is still kept.
    let target = fourth.clone();
    assert_eq!(f.store.mutate(move |s| s.delete_bot(&target)).await.unwrap_err().status, 409);
    f.reopen().await.expect("valid after the reassignments");
}

#[tokio::test]
async fn a_main_bot_that_does_not_exist_is_rejected_on_load() {
    let f = Fixture::new().await;
    let mut value = f.disk_json();
    value["mainBotId"] = new_id().into();
    let bytes = write_json(&f.state_file(), &value);
    assert_load_rejected(&f, &bytes).await;

    value["mainBotId"] = f.bot_id().into();
    write_json(&f.state_file(), &value);
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.snapshot().main_bot_id, Some(f.bot_id()));
}
