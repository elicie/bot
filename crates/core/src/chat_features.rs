//! HTTP routes for bot read state, update notifications, bot deletion, and message attachments.
//! Merged into `http::router` with one `.merge(...)` line.
//!
//! - `POST /api/bots/:id/unread` `{ "unread": bool }` (`setAgentUnread`)
//! - `POST /api/bots/:id/notify` `{ "enabled": bool }` (`setAgentNotifyOnUpdates`)
//! - `DELETE /api/bots/:id` `{}` (`deleteAgent`; rules in `grok_bot_store::bot_activity`)
//! - `POST /api/conversations/:id/attachment-messages` `{ "text", "attachments": [{ "name",
//!   "mimeType", "data" (base64) }] }`: like `/messages`, with files (limits in
//!   `grok_bot_store::attachments`). Its body limit is raised from 2 MiB to fit the attachments.
//! - `GET /api/conversations/:id/attachments/:attachmentId`: the stored bytes, served as an image
//!   or as `text/plain` under a sandboxing content security policy.

use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use grok_bot_model::{AttachmentKind, AttachmentUpload, HttpError};
use grok_bot_store::attachments::{
    MAX_ATTACHMENT_REQUEST_BYTES, MAX_ATTACHMENTS_TOTAL_BYTES, prepare_attachments, read_attachment, remove_conversation_files, size_label,
};
use serde::Deserialize;

use crate::http::{ApiError, ApiResult, Ctx, Empty, JsonBody, Remote, bad_request, state_reply};

pub(crate) fn routes() -> Router<Ctx> {
    Router::new()
        .route("/bots/{id}/unread", post(set_unread))
        .route("/bots/{id}/notify", post(set_notify))
        .route("/bots/{id}", axum::routing::delete(delete_bot))
        .route("/conversations/{id}/attachment-messages", post(send_with_attachments).layer(DefaultBodyLimit::max(MAX_ATTACHMENT_REQUEST_BYTES)))
        .route("/conversations/{id}/attachments/{attachment_id}", get(attachment))
        .route("/generated-images/{id}", get(generated_image))
}

/// Unreferenced generated images are kept this long: a streaming reply that links one has not been
/// committed to a message yet.
const GENERATED_IMAGE_GRACE: std::time::Duration = std::time::Duration::from_secs(3600);

/// Sweep generated images that no stored message references (`grok_bot_store::generated_images`).
pub(crate) async fn prune_generated_images(store: &std::sync::Arc<grok_bot_store::Store>) {
    let referenced: std::collections::HashSet<String> = store.read(|s| {
        let disk = s.disk();
        disk.conversations
            .iter()
            .flat_map(|conversation| &conversation.messages)
            .map(|message| message.text.as_str())
            .chain(disk.work_queue.iter().filter_map(|work| match &work.kind {
                grok_bot_model::WorkKind::User { text, .. } => Some(text.as_str()),
                _ => None,
            }))
            .flat_map(|text| grok_bot_store::generated_images::referenced_ids(text).map(str::to_owned).collect::<Vec<_>>())
            .collect()
    });
    let data_dir = store.data_dir().to_path_buf();
    let _ = tokio::task::spawn_blocking(move || grok_bot_store::generated_images::prune_generated_images(&data_dir, &referenced, GENERATED_IMAGE_GRACE)).await;
}

/// A model-generated image (`crate::provider_relay`), served like an image attachment.
async fn generated_image(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult {
    let data_dir = ctx.store.data_dir().to_path_buf();
    let (bytes, mime) =
        tokio::task::spawn_blocking(move || grok_bot_store::generated_images::read_generated_image(&data_dir, &id)).await.map_err(|_| failed())??;
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; sandbox"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=31536000, immutable"));
    Ok(response)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Unread {
    unread: bool,
}

async fn set_unread(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(input): JsonBody<Unread>) -> ApiResult {
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|s| s.set_bot_unread(&id, input.unread)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Notify {
    enabled: bool,
}

async fn set_notify(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(input): JsonBody<Notify>) -> ApiResult {
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|s| s.set_bot_notify_on_updates(&id, input.enabled)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

async fn delete_bot(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(_): JsonBody<Empty>) -> ApiResult {
    let store = ctx.store.clone();
    let bot_id = id.clone();
    let removed = ctx.queue.run(async move { store.mutate(|s| s.delete_bot(&id)).await }).await?;
    let data_dir = ctx.store.data_dir().to_path_buf();
    let _ = tokio::task::spawn_blocking(move || {
        for conversation_id in removed {
            remove_conversation_files(&data_dir, &conversation_id);
        }
        // Knowledge file contents of the deleted bot (metadata left with the bot).
        let knowledge = grok_bot_store::knowledge::knowledge_root(&data_dir).join(&bot_id);
        if std::fs::symlink_metadata(&knowledge).is_ok_and(|metadata| metadata.is_dir()) {
            let _ = std::fs::remove_dir_all(knowledge);
        }
    })
    .await;
    prune_generated_images(&ctx.store).await;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachmentMessage {
    text: String,
    #[serde(default)]
    attachments: Vec<AttachmentUpload>,
}

fn failed() -> ApiError {
    ApiError(HttpError::new(500, "The operation failed. Check that the workspace and data directory are accessible."))
}

async fn send_with_attachments(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, body: Result<Bytes, BytesRejection>) -> ApiResult {
    let bytes = match body {
        Ok(bytes) => bytes,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return Err(ApiError(HttpError::too_large(format!("Attachments of one message may total at most {}.", size_label(MAX_ATTACHMENTS_TOTAL_BYTES)))));
        }
        Err(_) => return Err(bad_request("Request body must be valid JSON.")),
    };
    let input: AttachmentMessage = serde_json::from_slice::<serde_json::Value>(&bytes)
        .map_err(|_| bad_request("Request body must be valid JSON."))
        .and_then(|value| serde_json::from_value(value).map_err(|error| bad_request(format!("Request: {error}").chars().take(1000).collect::<String>())))?;
    drop(bytes);
    let text = input.text.trim().to_owned();
    if text.is_empty() && input.attachments.is_empty() {
        return Err(bad_request("text: String must contain at least 1 character(s)"));
    }
    if text.chars().count() > 100000 {
        return Err(bad_request("text: String must contain at most 100000 character(s)"));
    }
    if uuid::Uuid::parse_str(&id).is_err() {
        return Err(ApiError(HttpError::not_found("Conversation does not exist.")));
    }
    let uploads = input.attachments;
    let prepared = tokio::task::spawn_blocking(move || prepare_attachments(uploads)).await.map_err(|_| failed())??;
    let (store, runtime) = (ctx.store.clone(), ctx.runtime.clone());
    ctx.queue
        .run(async move {
            store.read(|s| s.conversation(&id).map(|_| ()))?;
            if store.read(|s| s.is_reserved_conversation(&id)) {
                return Err(HttpError::conflict("This conversation is reserved for queued work or a read-only bot delivery."));
            }
            // Runtime persists the private blobs before durably accepting their queue record.
            // Once accepted, cancellation or dispatch owns them, not this transient HTTP request.
            runtime.start_with_attachments(&id, &text, prepared).await
        })
        .await?;
    Ok(state_reply(&ctx, StatusCode::ACCEPTED, &remote))
}

/// RFC 5987 `filename*` value.
fn encoded_filename(name: &str) -> String {
    name.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

async fn attachment(State(ctx): State<Ctx>, Path((id, attachment_id)): Path<(String, String)>) -> ApiResult {
    let meta = ctx
        .store
        .read(|s| {
            s.conversation(&id)
                .ok()
                .and_then(|conversation| conversation.messages.iter().flat_map(|m| m.attachments.iter().flatten()).find(|a| a.id == attachment_id).cloned())
        })
        .ok_or_else(|| ApiError(HttpError::not_found("Attachment does not exist.")))?;
    let data_dir = ctx.store.data_dir().to_path_buf();
    let bytes = tokio::task::spawn_blocking(move || read_attachment(&data_dir, &id, &attachment_id)).await.map_err(|_| failed())??;
    let content_type = match meta.kind {
        AttachmentKind::Image => meta.mime_type.clone(),
        AttachmentKind::Text => "text/plain; charset=utf-8".to_owned(),
    };
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(&content_type).unwrap_or(HeaderValue::from_static("application/octet-stream")));
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; sandbox"));
    if let Ok(value) = HeaderValue::from_str(&format!("inline; filename*=UTF-8''{}", encoded_filename(&meta.name))) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use grok_bot_store::Store;
    use grok_bot_store::generated_images::{ROUTE_PREFIX, read_generated_image, save_generated_image};

    #[tokio::test]
    async fn queued_generated_image_survives_startup_sweep_until_cancelled() {
        const PNG: &[u8] = b"\x89PNG\r\n\x1a\nqueued fixture";
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path(), grok_bot_model::system_clock()).await.unwrap();
        let conversation = store
            .mutate(|state| {
                state.update_settings(grok_bot_model::SettingsInput {
                    base_url: "http://127.0.0.1:8000/v1".into(),
                    model: "fixture-model".into(),
                    workspace: String::new(),
                    api_key: None,
                    clear_api_key: None,
                    max_concurrent_runs: None,
                })?;
                let bot = state.disk().bots[0].id.clone();
                state.add_conversation(&bot)
            })
            .await
            .unwrap();
        let queued_id = save_generated_image(store.data_dir(), PNG).unwrap();
        let unreferenced_id = save_generated_image(store.data_dir(), PNG).unwrap();
        let queued_path = store.data_dir().join("generated-images").join(&queued_id);
        let unreferenced_path = store.data_dir().join("generated-images").join(&unreferenced_id);
        let old = std::time::SystemTime::now() - GENERATED_IMAGE_GRACE - std::time::Duration::from_secs(60);
        for (id, path) in [(&queued_id, &queued_path), (&unreferenced_id, &unreferenced_path)] {
            assert_eq!(read_generated_image(store.data_dir(), id).unwrap(), (PNG.to_vec(), "image/png"));
            let file = std::fs::File::open(path).unwrap();
            file.set_times(std::fs::FileTimes::new().set_modified(old)).unwrap();
            assert!(file.metadata().unwrap().modified().unwrap().elapsed().unwrap() >= GENERATED_IMAGE_GRACE);
        }
        let text = format!("Please inspect ![queued image]({ROUTE_PREFIX}{queued_id})");
        let accepted = store.mutate(|state| state.enqueue_user_turn(&conversation.id, &text, Vec::new())).await.unwrap();
        assert!(store.read(|state| state.conversation(&conversation.id).unwrap().messages.is_empty()));
        // End the old writer after its durable acceptance before reopening the same data directory.
        store.flush().await.unwrap();
        drop(store);
        let store = Store::open(root.path(), grok_bot_model::system_clock()).await.unwrap();
        assert_eq!(store.read(|state| state.disk().work_queue.clone()), vec![accepted.clone()]);
        assert!(store.read(|state| state.disk().conversations.iter().all(|conversation| conversation.messages.is_empty())));
        assert_eq!(read_generated_image(store.data_dir(), &queued_id).unwrap(), (PNG.to_vec(), "image/png"));
        assert_eq!(read_generated_image(store.data_dir(), &unreferenced_id).unwrap(), (PNG.to_vec(), "image/png"));

        prune_generated_images(&store).await;
        assert_eq!(read_generated_image(store.data_dir(), &queued_id).unwrap(), (PNG.to_vec(), "image/png"));
        assert!(queued_path.exists(), "the durable queued reference is a retention root before dispatch");
        assert!(!unreferenced_path.exists(), "the startup sweep removes an equally old unreferenced image");
        assert_eq!(read_generated_image(store.data_dir(), &unreferenced_id).unwrap_err().status, 404);

        assert_eq!(store.mutate(|state| state.cancel_user_turn(&conversation.id, &accepted.id)).await.unwrap(), accepted);
        assert!(store.read(|state| state.disk().work_queue.is_empty()));
        prune_generated_images(&store).await;
        assert!(!queued_path.exists(), "cancellation releases the final reference without another grace delay");
        assert_eq!(read_generated_image(store.data_dir(), &queued_id).unwrap_err().status, 404);
    }
}
