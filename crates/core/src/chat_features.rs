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

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use grok_bot_model::{Attachment, AttachmentKind, AttachmentUpload, HttpError};
use grok_bot_store::attachments::{
    MAX_ATTACHMENT_REQUEST_BYTES, MAX_ATTACHMENTS_TOTAL_BYTES, discard_attachments, prepare_attachments, read_attachment, remove_conversation_files,
    size_label, write_attachments,
};
use serde::Deserialize;

use crate::http::{ApiError, ApiResult, Ctx, Empty, JsonBody, Remote, bad_request, state_reply};

pub(crate) fn routes() -> Router<Ctx> {
    Router::new()
        .route("/bots/{id}/unread", post(set_unread))
        .route("/bots/{id}/notify", post(set_notify))
        .route("/bots/{id}", axum::routing::delete(delete_bot))
        .route(
            "/conversations/{id}/attachment-messages",
            post(send_with_attachments).layer(DefaultBodyLimit::max(MAX_ATTACHMENT_REQUEST_BYTES)),
        )
        .route("/conversations/{id}/attachments/{attachment_id}", get(attachment))
        .route("/generated-images/{id}", get(generated_image))
}

/// Unreferenced generated images are kept this long: a streaming reply that links one has not been
/// committed to a message yet.
const GENERATED_IMAGE_GRACE: std::time::Duration = std::time::Duration::from_secs(3600);

/// Sweep generated images that no stored message references (`grok_bot_store::generated_images`).
pub(crate) async fn prune_generated_images(store: &std::sync::Arc<grok_bot_store::Store>) {
    let referenced: std::collections::HashSet<String> = store.read(|s| {
        s.disk()
            .conversations
            .iter()
            .flat_map(|conversation| conversation.messages.iter())
            .flat_map(|message| grok_bot_store::generated_images::referenced_ids(&message.text).map(str::to_owned).collect::<Vec<_>>())
            .collect()
    });
    let data_dir = store.data_dir().to_path_buf();
    let _ = tokio::task::spawn_blocking(move || grok_bot_store::generated_images::prune_generated_images(&data_dir, &referenced, GENERATED_IMAGE_GRACE)).await;
}

/// A model-generated image (`crate::provider_relay`), served like an image attachment.
async fn generated_image(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult {
    let data_dir = ctx.store.data_dir().to_path_buf();
    let (bytes, mime) = tokio::task::spawn_blocking(move || grok_bot_store::generated_images::read_generated_image(&data_dir, &id)).await.map_err(|_| failed())??;
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

async fn send_with_attachments(
    State(ctx): State<Ctx>,
    ConnectInfo(remote): Remote,
    Path(id): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult {
    let bytes = match body {
        Ok(bytes) => bytes,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return Err(ApiError(HttpError::too_large(format!(
                "Attachments of one message may total at most {}.",
                size_label(MAX_ATTACHMENTS_TOTAL_BYTES)
            ))));
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
            let metas: Vec<Attachment> = prepared.iter().map(|a| a.meta.clone()).collect();
            let data_dir = store.data_dir().to_path_buf();
            let (written, blobs) = {
                let (data_dir, id) = (data_dir.clone(), id.clone());
                tokio::task::spawn_blocking(move || {
                    let result = write_attachments(&data_dir, &id, &prepared);
                    (result, prepared)
                })
                .await
                .map_err(|_| HttpError::new(500, "Attachments could not be stored."))?
            };
            if let Err(error) = written {
                discard_attachments(&data_dir, &id, &metas);
                return Err(error);
            }
            let started = runtime.start_with_attachments(&id, &text, blobs).await;
            if started.is_err() {
                discard_attachments(&data_dir, &id, &metas);
            }
            started
        })
        .await?;
    Ok(state_reply(&ctx, StatusCode::ACCEPTED, &remote))
}

/// RFC 5987 `filename*` value.
fn encoded_filename(name: &str) -> String {
    name.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

async fn attachment(State(ctx): State<Ctx>, Path((id, attachment_id)): Path<(String, String)>) -> ApiResult {
    let meta = ctx
        .store
        .read(|s| {
            s.conversation(&id).ok().and_then(|conversation| {
                conversation.messages.iter().flat_map(|m| m.attachments.iter().flatten()).find(|a| a.id == attachment_id).cloned()
            })
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
