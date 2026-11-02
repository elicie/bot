//! Private OAuth-only response filter at the pre-engine provider boundary. The legacy image-only
//! path stays in provider_relay.rs. Only one bounded SSE event, possible credential prefixes, and
//! unfinished tool arguments are retained; ordinary assistant text continues to stream.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, header};
use futures::StreamExt;
use serde_json::{Map, Value, json};

use super::{MAX_BUFFERED_BYTES, rewrite_completion, skipped_response_header};
use crate::runtime::text::redact;

pub(super) const FAILED: &str = "Provider response could not be safely redacted.";
// Match the existing OAuth relay's complete active + retired credential budget. Never silently
// truncate a key set, which would turn a resource limit into a credential disclosure.
const MAX_KEYS: usize = 1024;
const MAX_KEY_BYTES: usize = 16 * 1024 * 1024;
const MAX_STREAMS: usize = 1024;
const MAX_EVENT_LINES: usize = 16384;
const MAX_JSON_DEPTH: usize = 128;
const TEXT_FIELDS: [&str; 4] = ["content", "reasoning_content", "reasoning", "refusal"];

// Deliberately no Debug/serialization: keys are only the request-start, in-memory Store snapshot.
pub(super) struct Redactor {
    keys: Vec<String>,
}

impl Redactor {
    pub(super) fn new(mut keys: Vec<String>) -> Result<Option<Self>, ()> {
        keys.retain(|key| !key.is_empty());
        keys.sort_unstable_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
        keys.dedup();
        let bytes = keys.iter().try_fold(0usize, |sum, key| sum.checked_add(key.len())).ok_or(())?;
        if keys.len() > MAX_KEYS || bytes > MAX_KEY_BYTES {
            return Err(());
        }
        Ok((!keys.is_empty()).then_some(Self { keys }))
    }

    fn check_bytes(&self, bytes: &[u8]) -> Result<(), ()> {
        // Strings/keys are sanitized semantically first. A remaining literal match (for example a
        // numeric-looking credential in a JSON number) cannot be changed without corrupting the
        // protocol, so reject that frame instead of forwarding it.
        if self.keys.iter().any(|key| super::contains(bytes, key.as_bytes())) { Err(()) } else { Ok(()) }
    }

    fn text(&self, text: &str) -> Result<String, ()> {
        if text.len() > MAX_BUFFERED_BYTES {
            return Err(());
        }
        let mut out = text.to_owned();
        for key in &self.keys {
            let count = out.matches(key.as_str()).count();
            if count == 0 {
                continue;
            }
            let length = out.len().checked_sub(count.checked_mul(key.len()).ok_or(())?).and_then(|n| n.checked_add(count.checked_mul("[redacted]".len())?));
            if length.is_none_or(|length| length > MAX_BUFFERED_BYTES) {
                return Err(());
            }
            out = redact(key.as_str(), &out, None);
        }
        if self.keys.iter().any(|key| out.contains(key.as_str())) {
            return Err(());
        }
        Ok(out)
    }

    // Same proper-prefix rule as runtime::text::append_streamed_text, but without that UI helper's
    // 200000-character display cap: the engine must receive all harmless provider text.
    fn streamed(&self, held: &mut String, chunk: &str) -> Result<String, ()> {
        if held.len().checked_add(chunk.len()).is_none_or(|length| length > MAX_BUFFERED_BYTES) {
            return Err(());
        }
        let pending = format!("{held}{chunk}");
        let mut out = self.text(&pending)?;
        let mut length = 0;
        for key in &self.keys {
            for (prefix, _) in key.char_indices().rev() {
                if prefix <= length {
                    break;
                }
                if prefix <= out.len() && out.ends_with(&key[..prefix]) {
                    length = prefix;
                    break;
                }
            }
        }
        *held = out.split_off(out.len() - length);
        Ok(out)
    }

    fn finished_suffix(&self, suffix: &str) -> Result<String, ()> {
        // A long incomplete credential is still sensitive even when a terminal frame proves it
        // cannot complete. Keep short incidental prefixes (for example a JWT's final e) intact.
        let substantial = self.keys.iter().any(|key| key.starts_with(suffix) && suffix.len() < key.len() && suffix.len() >= 8.min(key.len() / 2));
        self.text(if substantial { "[redacted]" } else { suffix })
    }

    fn json(&self, value: &mut Value, parse_arguments: bool) -> Result<bool, ()> {
        self.json_inner(value, parse_arguments, false, 0)
    }

    fn json_inner(&self, value: &mut Value, parse_arguments: bool, function: bool, depth: usize) -> Result<bool, ()> {
        if depth > MAX_JSON_DEPTH {
            return Err(());
        }
        let mut changed = false;
        match value {
            Value::String(text) => {
                let safe = self.text(text)?;
                changed = safe != *text;
                *text = safe;
            }
            Value::Array(values) => {
                for value in values {
                    changed |= self.json_inner(value, parse_arguments, false, depth + 1)?;
                }
            }
            Value::Object(object) => {
                let mut safe = Map::new();
                for (key, mut value) in std::mem::take(object) {
                    if parse_arguments && function && key == "arguments" {
                        if let Some(arguments) = value.as_str() {
                            let sanitized = self.arguments(arguments, depth + 1)?;
                            changed |= sanitized != arguments;
                            value = Value::String(sanitized);
                        } else if !value.is_null() {
                            return Err(());
                        }
                    } else {
                        changed |= self.json_inner(&mut value, parse_arguments, key == "function" || key == "function_call", depth + 1)?;
                    }
                    let sanitized = self.text(&key)?;
                    changed |= sanitized != key;
                    // Redacted property names must not silently overwrite another property.
                    if safe.insert(sanitized, value).is_some() {
                        return Err(());
                    }
                }
                *object = safe;
            }
            _ => {}
        }
        Ok(changed)
    }

    fn arguments(&self, arguments: &str, depth: usize) -> Result<String, ()> {
        // arguments is JSON *inside* a JSON string: outer SSE decoding alone cannot reveal
        // Unicode-escaped credentials, including escapes divided between argument deltas.
        let mut value: Value = serde_json::from_str(arguments).map_err(|_| ())?;
        if self.json_inner(&mut value, true, false, depth)? { serialize(&value) } else { Ok(arguments.to_owned()) }
    }

    pub(super) fn response_headers(&self, upstream: &HeaderMap) -> Result<HeaderMap, ()> {
        if upstream.get_all(header::CONTENT_ENCODING).iter().any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity")) {
            return Err(());
        }
        let mut headers = HeaderMap::new();
        for (name, value) in upstream {
            if skipped_response_header(name) || matches!(name.as_str(), "etag" | "content-md5" | "digest") {
                continue;
            }
            if self.text(name.as_str())? != name.as_str() {
                continue;
            }
            // Uninterpretable optional upstream metadata is not forwarded to engine diagnostics.
            let Ok(text) = value.to_str() else { continue };
            let safe = self.text(text)?;
            headers.append(name, HeaderValue::from_str(&safe).map_err(|_| ())?);
        }
        Ok(headers)
    }
}

fn serialize(value: &Value) -> Result<String, ()> {
    struct Capped(Vec<u8>);
    impl std::io::Write for Capped {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            extend(&mut self.0, bytes).map_err(|()| std::io::Error::other(FAILED))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut out = Capped(Vec::new());
    serde_json::to_writer(&mut out, value).map_err(|_| ())?;
    String::from_utf8(out.0).map_err(|_| ())
}

pub(super) async fn rewrite_body(bytes: Bytes, directory: PathBuf, redactor: Redactor, json_required: bool, images: bool) -> Result<Bytes, ()> {
    tokio::task::spawn_blocking(move || {
        if bytes.len() > MAX_BUFFERED_BYTES {
            return Err(());
        }
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(mut value) => {
                let mut changed = redactor.json(&mut value, true)?;
                changed |= images && rewrite_completion(&mut value, &directory);
                changed |= redactor.json(&mut value, false)?;
                let out = if changed { Bytes::from(serialize(&value)?) } else { bytes };
                redactor.check_bytes(&out)?;
                Ok(out)
            }
            Err(_) if json_required => Err(()),
            Err(_) => {
                let text = std::str::from_utf8(&bytes).map_err(|_| ())?;
                let safe = redactor.text(text)?;
                if safe == text { Ok(bytes) } else { Ok(Bytes::from(safe)) }
            }
        }
    })
    .await
    .map_err(|_| ())?
}

#[derive(Default)]
struct Tool {
    arguments: Option<String>,
}

#[derive(Default)]
struct Choice {
    suffixes: BTreeMap<&'static str, String>,
    // Legacy function_call and modern tool_calls have separate namespaces.
    tools: BTreeMap<(bool, u64), Tool>,
}

impl Choice {
    fn delta(&mut self, delta: &mut Map<String, Value>, redactor: &Redactor) -> Result<bool, ()> {
        // The pinned ChatChunkDelta/sampler ignores extensions such as reasoning_details. Omit
        // unconsumed fields instead of either rejecting ordinary provider replies or forwarding
        // unrecognized fragmented credential channels to the engine.
        let original_fields = delta.len();
        delta.retain(|key, _| TEXT_FIELDS.contains(&key.as_str()) || matches!(key.as_str(), "role" | "tool_calls" | "function_call"));
        let mut changed = delta.len() != original_fields;
        for field in TEXT_FIELDS {
            match delta.get_mut(field) {
                Some(Value::String(text)) => {
                    let safe = redactor.streamed(self.suffixes.entry(field).or_default(), text)?;
                    changed |= safe != *text;
                    *text = safe;
                }
                Some(Value::Null) | None => {}
                _ => return Err(()),
            }
        }
        if let Some(value) = delta.get_mut("function_call").filter(|value| !value.is_null()) {
            let function = value.as_object_mut().ok_or(())?;
            changed |= Self::function(self.tools.entry((true, 0)).or_default(), function, redactor)?;
        }
        if let Some(value) = delta.get_mut("tool_calls").filter(|value| !value.is_null()) {
            let calls = value.as_array_mut().ok_or(())?;
            let single = calls.len() == 1;
            for call in calls {
                let call = call.as_object_mut().ok_or(())?;
                let index = index(call, single)?;
                let tool = self.tools.entry((false, index)).or_default();
                if let Some(function) = call.get_mut("function").filter(|value| !value.is_null()) {
                    changed |= Self::function(tool, function.as_object_mut().ok_or(())?, redactor)?;
                }
            }
        }
        Ok(changed)
    }

    fn function(tool: &mut Tool, function: &mut Map<String, Value>, redactor: &Redactor) -> Result<bool, ()> {
        let mut changed = false;
        if let Some(value) = function.get_mut("name").filter(|value| !value.is_null()) {
            let name = value.as_str().ok_or(())?;
            // The pinned sampler assigns the latest name; it appends only arguments. A held name
            // suffix emitted at finish would overwrite search_tool/todo_write with that suffix.
            let safe = redactor.text(name)?;
            changed |= safe != name;
            *value = Value::String(safe);
        }
        if let Some(value) = function.get_mut("arguments").filter(|value| !value.is_null()) {
            let arguments = value.as_str().ok_or(())?;
            let pending = tool.arguments.get_or_insert_default();
            if pending.len().checked_add(arguments.len()).is_none_or(|length| length > MAX_BUFFERED_BYTES) {
                return Err(());
            }
            pending.push_str(arguments);
            changed |= !arguments.is_empty();
            *value = Value::String(String::new());
        }
        Ok(changed)
    }

    fn finish(self, redactor: &Redactor) -> Result<Map<String, Value>, ()> {
        let mut delta = Map::new();
        for (field, suffix) in self.suffixes {
            if !suffix.is_empty() {
                delta.insert(field.to_owned(), Value::String(redactor.finished_suffix(&suffix)?));
            }
        }
        for ((legacy, index), tool) in self.tools {
            let mut function = Map::new();
            if let Some(arguments) = tool.arguments {
                function.insert("arguments".into(), Value::String(redactor.arguments(&arguments, 0)?));
            }
            if function.is_empty() {
                continue;
            }
            if legacy {
                delta.insert("function_call".into(), Value::Object(function));
            } else {
                delta.entry("tool_calls").or_insert_with(|| json!([])).as_array_mut().ok_or(())?.push(json!({"index": index, "function": function}));
            }
        }
        Ok(delta)
    }

    fn buffered(&self) -> usize {
        self.suffixes.values().map(String::len).sum::<usize>() + self.tools.values().map(|tool| tool.arguments.as_ref().map_or(0, String::len)).sum::<usize>()
    }
}

fn index(object: &Map<String, Value>, single: bool) -> Result<u64, ()> {
    match object.get("index") {
        Some(value) => value.as_u64().ok_or(()),
        None if single => Ok(0),
        None => Err(()),
    }
}

fn merge_strings(target: &mut Map<String, Value>, suffix: Map<String, Value>) -> Result<(), ()> {
    for (field, text) in suffix {
        let text = text.as_str().ok_or(())?;
        match target.get_mut(&field) {
            Some(Value::String(value)) => value.push_str(text),
            None | Some(Value::Null) => {
                target.insert(field, Value::String(text.to_owned()));
            }
            _ => return Err(()),
        }
    }
    Ok(())
}

fn merge_delta(target: &mut Map<String, Value>, mut suffix: Map<String, Value>) -> Result<(), ()> {
    if let Some(function) = suffix.remove("function_call") {
        let target = target.entry("function_call").or_insert_with(|| json!({})).as_object_mut().ok_or(())?;
        merge_strings(target, function.as_object().ok_or(())?.clone())?;
    }
    if let Some(calls) = suffix.remove("tool_calls") {
        let target = target.entry("tool_calls").or_insert_with(|| json!([])).as_array_mut().ok_or(())?;
        for call in calls.as_array().ok_or(())? {
            let index = call["index"].as_u64().ok_or(())?;
            let position = target.iter().position(|call| call.get("index").and_then(Value::as_u64).unwrap_or(0) == index);
            if let Some(position) = position {
                let function = target[position].as_object_mut().ok_or(())?.entry("function").or_insert_with(|| json!({})).as_object_mut().ok_or(())?;
                merge_strings(function, call["function"].as_object().ok_or(())?.clone())?;
            } else {
                target.push(call.clone());
            }
        }
    }
    merge_strings(target, suffix)
}

pub(super) struct Sse {
    redactor: Redactor,
    directory: PathBuf,
    line: Vec<u8>,
    event: Vec<u8>,
    event_lines: usize,
    choice_index: Option<u64>,
    choices: BTreeMap<u64, Choice>,
    completed_choices: BTreeSet<u64>,
    metadata: Map<String, Value>,
    done: bool,
}

impl Sse {
    pub(super) fn new(redactor: Redactor, directory: PathBuf) -> Self {
        Self {
            redactor,
            directory,
            line: Vec::new(),
            event: Vec::new(),
            event_lines: 0,
            choice_index: None,
            choices: BTreeMap::new(),
            completed_choices: BTreeSet::new(),
            metadata: Map::new(),
            done: false,
        }
    }

    fn bounded(&self) -> Result<(), ()> {
        let streams = self.choices.len() + self.completed_choices.len() + self.choices.values().map(|choice| choice.tools.len()).sum::<usize>();
        let metadata_bytes = self.metadata.values().map(|value| value.as_str().map_or(32, str::len)).sum::<usize>();
        let buffers = self.line.len() + self.event.len() + metadata_bytes + self.choices.values().map(Choice::buffered).sum::<usize>();
        if streams > MAX_STREAMS || buffers > MAX_BUFFERED_BYTES || self.event_lines > MAX_EVENT_LINES { Err(()) } else { Ok(()) }
    }

    pub(super) fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, ()> {
        let mut out = Vec::new();
        for part in chunk.split_inclusive(|byte| *byte == b'\n') {
            if self.line.len().checked_add(part.len()).is_none_or(|length| length > MAX_BUFFERED_BYTES) {
                return Err(());
            }
            self.line.extend_from_slice(part);
            if part.ends_with(b"\n") {
                let content = self.line.strip_suffix(b"\n").ok_or(())?;
                let content = content.strip_suffix(b"\r").unwrap_or(content);
                // Bare CR is another SSE line delimiter. Until supported explicitly, do not let
                // it smuggle data past this LF/CRLF parser into a more permissive engine parser.
                if content.contains(&b'\r') || content.starts_with(b"\xef\xbb\xbf") {
                    return Err(());
                }
                let blank = content.is_empty();
                if self.event.len().checked_add(self.line.len()).is_none_or(|length| length > MAX_BUFFERED_BYTES) {
                    return Err(());
                }
                self.event.extend_from_slice(&std::mem::take(&mut self.line));
                self.event_lines += 1;
                self.bounded()?;
                if blank {
                    let event = std::mem::take(&mut self.event);
                    self.event_lines = 0;
                    extend(&mut out, &self.event(event)?)?;
                }
            }
            self.bounded()?;
        }
        Ok(out)
    }

    pub(super) fn finish(&mut self) -> Result<Vec<u8>, ()> {
        let mut out = Vec::new();
        if !self.line.is_empty() || !self.event.is_empty() {
            if self.line.contains(&b'\r') || self.line.starts_with(b"\xef\xbb\xbf") {
                return Err(());
            }
            self.event.extend_from_slice(&std::mem::take(&mut self.line));
            self.bounded()?;
            // Make a final unterminated SSE event explicit before adding any suffix-flush event.
            if !self.event.ends_with(b"\n") {
                self.event.push(b'\n');
            }
            self.event.push(b'\n');
            let event = std::mem::take(&mut self.event);
            extend(&mut out, &self.event(event)?)?;
        }
        extend(&mut out, &self.flush()?)?;
        self.redactor.check_bytes(&out)?;
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<u8>, ()> {
        let mut choices = Vec::new();
        for (index, choice) in std::mem::take(&mut self.choices) {
            let delta = choice.finish(&self.redactor)?;
            if !delta.is_empty() {
                choices.push(json!({"index": index, "delta": delta, "finish_reason": null}));
            }
        }
        if choices.is_empty() {
            return Ok(Vec::new());
        }
        let mut value = self.metadata.clone();
        value.insert("choices".into(), Value::Array(choices));
        Ok(format!("data: {}\n\n", serialize(&Value::Object(value))?).into_bytes())
    }

    fn completion(&mut self, value: &mut Value) -> Result<bool, ()> {
        let mut changed = rewrite_completion(value, &self.directory);
        if let Some(choices) = value.get_mut("choices") {
            if self.done {
                return Err(());
            }
            let choices = choices.as_array_mut().ok_or(())?;
            let single = choices.len() == 1;
            for choice in choices {
                let choice = choice.as_object_mut().ok_or(())?;
                let index = index(choice, single)?;
                // The application requests n=1, but the pinned sampler concatenates ALL choice
                // indices into one content/reasoning accumulator. Never admit a second distinct
                // index, even after the first choice finished and flushed a harmless prefix.
                if self.choice_index.is_some_and(|first| first != index) {
                    return Err(());
                }
                self.choice_index = Some(index);
                // finish_reason made any held proper prefix safe to flush. Accepting later text
                // for that same choice could reassemble the credential after that flush.
                if self.completed_choices.contains(&index) {
                    return Err(());
                }
                let state = self.choices.entry(index).or_default();
                if let Some(delta) = choice.get_mut("delta").filter(|value| !value.is_null()) {
                    changed |= state.delta(delta.as_object_mut().ok_or(())?, &self.redactor)?;
                }
                if let Some(message) = choice.get_mut("message") {
                    changed |= self.redactor.json(message, true)?;
                }
                if choice.get("finish_reason").is_some_and(|reason| !reason.is_null()) {
                    let suffix = self.choices.remove(&index).ok_or(())?.finish(&self.redactor)?;
                    self.completed_choices.insert(index);
                    if !suffix.is_empty() {
                        let delta = choice.entry("delta").or_insert_with(|| json!({})).as_object_mut().ok_or(())?;
                        merge_delta(delta, suffix)?;
                        changed = true;
                    }
                }
            }
        } else if value
            .as_object()
            .is_some_and(|object| ["delta", "content", "reasoning_content", "reasoning", "arguments"].iter().any(|key| object.contains_key(*key)))
        {
            return Err(());
        }
        self.bounded()?;
        // Streamed arguments were already collected/decoded as an inner JSON document above;
        // intermediate empty argument placeholders must not be parsed as completed tool calls.
        changed |= self.redactor.json(value, false)?;
        for key in ["id", "object", "created", "model", "system_fingerprint", "service_tier"] {
            if let Some(value) = value.get(key) {
                if value.is_array() || value.is_object() {
                    return Err(());
                }
                self.metadata.insert(key.into(), value.clone());
            }
        }
        self.bounded()?;
        Ok(changed)
    }

    fn event(&mut self, event: Vec<u8>) -> Result<Vec<u8>, ()> {
        let text = std::str::from_utf8(&event).map_err(|_| ())?;
        let mut data = String::new();
        let mut data_lines = 0;
        for line in text.split_inclusive('\n') {
            let content = line.trim_end_matches(['\r', '\n']);
            if let Some(payload) = content.strip_prefix("data:").or_else(|| (content == "data").then_some("")) {
                if data_lines > 0 {
                    data.push('\n');
                }
                data.push_str(payload.strip_prefix(' ').unwrap_or(payload));
                data_lines += 1;
            }
        }
        let mut prefix = Vec::new();
        let replacement = if data_lines == 0 {
            None
        } else if data.trim() == "[DONE]" {
            prefix = self.flush()?;
            self.done = true;
            None
        } else {
            let mut value: Value = serde_json::from_str(&data).map_err(|_| ())?;
            if self.completion(&mut value)? { Some(serialize(&value)?) } else { None }
        };
        let mut out = Vec::new();
        let mut emitted = false;
        for line in text.split_inclusive('\n') {
            let content = line.trim_end_matches(['\r', '\n']);
            if content.starts_with("data:") || content == "data" {
                if let Some(replacement) = &replacement {
                    if !emitted {
                        extend(&mut out, b"data: ")?;
                        extend(&mut out, replacement.as_bytes())?;
                        extend(&mut out, if line.ends_with("\r\n") { b"\r\n" } else { b"\n" })?;
                        emitted = true;
                    }
                } else {
                    extend(&mut out, line.as_bytes())?;
                }
            } else {
                extend(&mut out, self.redactor.text(line)?.as_bytes())?;
            }
        }
        extend(&mut prefix, &out)?;
        self.redactor.check_bytes(&prefix)?;
        Ok(prefix)
    }
}

fn extend(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), ()> {
    if out.len().checked_add(bytes.len()).is_none_or(|length| length > MAX_BUFFERED_BYTES) {
        return Err(());
    }
    out.extend_from_slice(bytes);
    Ok(())
}

pub(super) fn rewrite_stream(
    upstream: reqwest::Response,
    directory: PathBuf,
    redactor: Redactor,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    let state = (upstream.bytes_stream(), Sse::new(redactor, directory), false);
    futures::stream::try_unfold(state, |(mut stream, mut sse, done)| async move {
        if done {
            return Ok(None);
        }
        let (chunk, finished) = match stream.next().await {
            Some(Ok(chunk)) => (Some(chunk), false),
            Some(Err(_)) => return Err(std::io::Error::other(FAILED)),
            None => (None, true),
        };
        let (sse, out) = tokio::task::spawn_blocking(move || {
            let out = match chunk {
                Some(chunk) => sse.push(&chunk),
                None => sse.finish(),
            };
            (sse, out)
        })
        .await
        .map_err(|_| std::io::Error::other(FAILED))?;
        let out = out.map_err(|()| std::io::Error::other(FAILED))?;
        Ok(Some((Bytes::from(out), (stream, sse, finished))))
    })
}
