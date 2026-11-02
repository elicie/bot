//! Lossless public-view pages below the engine's 20,000-byte inline MCP limit.
use grok_bot_model::{Canvas, HttpError, HttpResult};
use grok_bot_store::skills::skill_response_text;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(super) const MAX_RESPONSE_BYTES: usize = 18 * 1024;

pub(super) fn page(canvas: Option<&Canvas>, offset: usize, revision: Option<u64>, expected_view: Option<&str>) -> HttpResult<Value> {
    if offset > 0 && (revision.is_none() || expected_view.is_none()) {
        return Err(HttpError::bad_request("Continuation pages require the revision and viewHash returned by the first page."));
    }
    let Some(canvas) = canvas else {
        return if revision.is_some() || expected_view.is_some() {
            Err(HttpError::conflict("The canvas changed. Start reading again at offset 0."))
        } else {
            Ok(json!({"canvas": null}))
        };
    };
    let view = format!("{:x}", Sha256::digest(serde_json::to_vec(&json!([canvas.title, canvas.language, canvas.content, canvas.redacted])).unwrap()));
    if revision.is_some_and(|value| value != canvas.revision) || expected_view.is_some_and(|value| value != view) {
        return Err(HttpError::conflict("The canvas or its public view changed. Discard earlier pages and start again at offset 0."));
    }
    if offset > canvas.content.len() || !canvas.content.is_char_boundary(offset) {
        return Err(HttpError::bad_request("offset must be a UTF-8 byte boundary within the public canvas content."));
    }
    let history: Vec<Value> = canvas
        .history
        .iter()
        .map(|version| {
            let mut value = json!({"revision": version.revision, "source": version.source, "updatedAt": version.updated_at});
            if version.redacted {
                value["redacted"] = json!(true);
            }
            value
        })
        .collect();
    let base = json!({
        "revision": canvas.revision, "title": canvas.title, "language": canvas.language,
        "updatedAt": canvas.updated_at, "source": canvas.source, "history": history,
        "offset": offset, "totalBytes": canvas.content.len(), "viewHash": view,
    });
    let build = |end: usize| {
        let mut value = base.clone();
        value["content"] = json!(&canvas.content[offset..end]);
        if canvas.redacted {
            value["redacted"] = json!(true);
        }
        if end < canvas.content.len() {
            value["nextOffset"] = json!(end);
        }
        json!({"canvas": value})
    };
    let full = build(canvas.content.len());
    if skill_response_text(&full).len() <= MAX_RESPONSE_BYTES {
        return Ok(full);
    }
    // JSON and data-URI escaping can expand text: measure the actual complete wire result,
    // not raw source bytes. Search only valid character ends, and always make progress.
    let mut ends = vec![offset];
    ends.extend(canvas.content[offset..].char_indices().take(MAX_RESPONSE_BYTES).map(|(index, character)| offset + index + character.len_utf8()));
    let (mut low, mut high) = (0, ends.len() - 1);
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if skill_response_text(&build(ends[middle])).len() <= MAX_RESPONSE_BYTES {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    if low == 0 {
        return Err(HttpError::too_large("Canvas metadata exceeds the complete tool response limit."));
    }
    Ok(build(ends[low]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use grok_bot_model::{CanvasSource, CanvasVersion};
    fn canvas(content: String) -> Canvas {
        Canvas {
            revision: 21,
            title: "문서 \"data: draft\"".into(),
            language: "markdown".into(),
            content,
            updated_at: "2026-11-02T00:00:00.000Z".into(),
            source: CanvasSource::User,
            redacted: false,
            history: (1..=20)
                .map(|revision| CanvasVersion {
                    revision,
                    title: "x".repeat(100),
                    language: "text".into(),
                    content: "old private history".repeat(1000),
                    updated_at: "2026-11-02T00:00:00.000Z".into(),
                    source: CanvasSource::User,
                    redacted: false,
                })
                .collect(),
        }
    }
    #[test]
    fn escaped_pages_reconstruct_large_unicode_code_and_control_text() {
        let content = "한글 😀\r\n\t\"\\\u{1}data:image/png;base64,AAAA\n".repeat(1600) + "UNIQUE_TAIL";
        let doc = canvas(content.clone());
        let (mut offset, mut reconstructed, mut hash, mut count) = (0, String::new(), None, 0);
        loop {
            let result = page(Some(&doc), offset, Some(doc.revision), hash.as_deref()).unwrap();
            let wire = skill_response_text(&result);
            assert!(wire.len() <= MAX_RESPONSE_BYTES);
            assert!(!wire.to_ascii_lowercase().contains("data:"));
            let value: Value = serde_json::from_str(&wire).unwrap();
            let current = &value["canvas"];
            assert_eq!(current["offset"], offset);
            assert_eq!(current["totalBytes"], content.len());
            assert!(current["history"][0].get("content").is_none());
            assert!(current["history"][0].get("title").is_none());
            reconstructed.push_str(current["content"].as_str().unwrap());
            hash = Some(current["viewHash"].as_str().unwrap().to_owned());
            count += 1;
            let Some(next) = current["nextOffset"].as_u64() else { break };
            assert!(next as usize > offset);
            offset = next as usize;
        }
        assert!(count > 1);
        assert_eq!(reconstructed, content);
        assert_eq!(page(Some(&doc), 1, Some(doc.revision), hash.as_deref()).unwrap_err().status, 400); // inside a Korean character
        assert_eq!(page(Some(&doc), content.len() + 1, Some(doc.revision), hash.as_deref()).unwrap_err().status, 400);
        assert_eq!(page(Some(&doc), 3, None, None).unwrap_err().status, 400);
        assert_eq!(page(None, 0, Some(doc.revision), hash.as_deref()).unwrap_err().status, 409);
        assert_eq!(page(None, 0, None, None).unwrap(), json!({"canvas": null}));
        let eof = page(Some(&doc), content.len(), Some(doc.revision), hash.as_deref()).unwrap();
        assert_eq!(eof["canvas"]["content"], "");
        assert!(eof["canvas"].get("nextOffset").is_none());
        let mut changed = doc.clone();
        changed.redacted = true;
        changed.content = changed.content.replace("한글", "[redacted]");
        assert_eq!(page(Some(&changed), 0, Some(doc.revision), hash.as_deref()).unwrap_err().status, 409);
        changed.revision += 1;
        assert_eq!(page(Some(&changed), 0, Some(doc.revision), None).unwrap_err().status, 409);
    }
}
