//! Synthetic-only credential canaries. Assertions intentionally never print credential-bearing
//! inputs/outputs, including when a regression makes the test fail.

use super::redaction::{Redactor, Sse};
use super::*;

const ACCESS: &str = "synthetic-oauth-provider-access-0123456789";
const RETIRED: &str = "synthetic-oauth-provider-retired-9876543210";

fn redactor() -> Redactor {
    Redactor::new(vec![ACCESS.into(), RETIRED.into()]).unwrap().unwrap()
}

fn sse(directory: &std::path::Path) -> Sse {
    Sse::new(redactor(), directory.to_owned())
}

fn event(value: Value) -> Vec<u8> {
    format!("data: {value}\n\n").into_bytes()
}

fn delta(index: u64, value: Value) -> Vec<u8> {
    event(
        json!({"id":"synthetic-completion", "object":"chat.completion.chunk", "created":1, "model":"fixture", "choices":[{"index":index,"delta":value,"finish_reason":null}]}),
    )
}

fn finish(index: u64) -> Vec<u8> {
    event(json!({"choices":[{"index":index,"delta":{},"finish_reason":"stop"}]}))
}

fn values(bytes: &[u8]) -> Vec<Value> {
    std::str::from_utf8(bytes)
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|payload| *payload != "[DONE]")
        .map(|payload| serde_json::from_str(payload).expect("rewritten SSE remains valid JSON"))
        .collect()
}

fn assert_safe(bytes: &[u8]) {
    for key in [ACCESS, RETIRED] {
        assert!(!contains(bytes, key.as_bytes()), "synthetic OAuth credential escaped the provider boundary");
    }
}

fn field(bytes: &[u8], choice: u64, field: &str) -> String {
    values(bytes)
        .iter()
        .flat_map(|value| value["choices"].as_array().into_iter().flatten())
        .filter(|value| value["index"] == choice)
        .filter_map(|choice| choice["delta"][field].as_str())
        .collect()
}

#[test]
fn escaped_split_content_and_reasoning_are_held_across_sse_and_network_chunks() {
    let root = tempfile::tempdir().unwrap();
    let mut filter = sse(root.path());
    let prefix = &ACCESS[..ACCESS.len() - 1];
    // A JSON escape in the first delta must be decoded before prefix matching.
    let first =
        String::from_utf8(delta(0, json!({"content":format!("visible {prefix}"), "reasoning_content":format!("thinking {prefix}"), "reasoning":prefix})))
            .unwrap();
    let first = first.replace("synthetic-oauth", "\\u0073ynthetic-oauth");
    let mut wire = Vec::new();
    for byte in first.as_bytes().chunks(1) {
        wire.extend(filter.push(byte).unwrap());
        assert!(!contains(&wire, prefix.as_bytes()), "a possible credential prefix was emitted prematurely");
    }
    assert!(field(&wire, 0, "content") == "visible ");
    assert!(field(&wire, 0, "reasoning_content") == "thinking ");
    wire.extend(
        filter
            .push(&delta(0, json!({"content":&ACCESS[ACCESS.len()-1..], "reasoning_content":&ACCESS[ACCESS.len()-1..], "reasoning":&ACCESS[ACCESS.len()-1..]})))
            .unwrap(),
    );
    wire.extend(filter.push(&finish(0)).unwrap());
    wire.extend(filter.push(b"data: [DONE]\n\n").unwrap());
    wire.extend(filter.finish().unwrap());
    assert_safe(&wire);
    assert!(field(&wire, 0, "content") == "visible [redacted]");
    assert!(field(&wire, 0, "reasoning_content") == "thinking [redacted]");
    assert!(field(&wire, 0, "reasoning") == "[redacted]");
}

#[test]
fn harmless_suffixes_precede_finish_or_done() {
    let root = tempfile::tempdir().unwrap();
    let mut filter = sse(root.path());
    let prefix = &ACCESS[..1];
    let mut wire = filter.push(&delta(0, json!({"content":prefix}))).unwrap();
    assert!(field(&wire, 0, "content").is_empty());
    let terminal = filter.push(&finish(0)).unwrap();
    assert!(values(&terminal)[0]["choices"][0]["delta"]["content"] == prefix);
    assert!(values(&terminal)[0]["choices"][0]["finish_reason"] == "stop");
    wire.extend(terminal);
    assert!(field(&wire, 0, "content") == prefix);
    let mut filter = sse(root.path());
    assert!(field(&filter.push(&delta(0, json!({"content":prefix}))).unwrap(), 0, "content").is_empty());
    let done = filter.push(b"data: [DONE]\n\n").unwrap();
    assert!(field(&done, 0, "content") == prefix);
    assert!(done.ends_with(b"data: [DONE]\n\n"));
    // Synthesized flush frames retain the provider metadata required by OpenAI clients.
    assert!(values(&done)[0]["id"] == "synthetic-completion");
    assert!(filter.finish().unwrap().is_empty());
}

#[test]
fn a_second_choice_index_cannot_complete_content_or_reasoning_in_the_native_accumulator() {
    let root = tempfile::tempdir().unwrap();
    let prefix = &ACCESS[..ACCESS.len() - 1];
    for channel in ["content", "reasoning_content"] {
        for finish_first in [false, true] {
            let mut filter = sse(root.path());
            let mut wire = filter.push(&delta(0, json!({channel:prefix}))).unwrap();
            assert!(field(&wire, 0, channel).is_empty());
            if finish_first {
                wire.extend(filter.push(&finish(0)).unwrap());
                assert!(field(&wire, 0, channel) == "[redacted]");
            }
            assert!(!contains(&wire, &ACCESS.as_bytes()[..20]), "a substantial credential prefix escaped before rejecting the extra choice");
            let second = event(json!({"choices":[{"index":1,"delta":{channel:&ACCESS[ACCESS.len()-1..]},"finish_reason":"stop"}]}));
            assert!(filter.push(&second).is_err(), "the n=1 native sampler must never receive a second distinct choice index");
            assert_safe(&wire);
        }
        let both = event(json!({"choices":[
            {"index":0,"delta":{channel:prefix},"finish_reason":"stop"},
            {"index":1,"delta":{channel:&ACCESS[ACCESS.len()-1..]},"finish_reason":"stop"}
        ]}));
        assert!(sse(root.path()).push(&both).is_err(), "a multi-choice frame must fail before any of it is forwarded");
    }
}

#[test]
fn a_finished_choice_cannot_resume_and_reassemble_a_flushed_credential_prefix() {
    let root = tempfile::tempdir().unwrap();
    let mut filter = sse(root.path());
    let prefix = &ACCESS[..ACCESS.len() - 1];
    let mut wire = filter.push(&delta(0, json!({"content":prefix}))).unwrap();
    assert!(field(&wire, 0, "content").is_empty());
    wire.extend(filter.push(&finish(0)).unwrap());
    assert!(field(&wire, 0, "content") == "[redacted]");
    assert!(!contains(&wire, &ACCESS.as_bytes()[..20]), "a substantial credential prefix escaped at finish");
    assert!(filter.push(&delta(0, json!({"content":&ACCESS[ACCESS.len()-1..]}))).is_err(), "a completed choice must reject later fragments");
    assert_safe(&wire);
}

#[test]
fn terminal_boundaries_redact_substantial_prefixes_but_preserve_short_incidental_suffixes() {
    let root = tempfile::tempdir().unwrap();
    let prefix = &ACCESS[..ACCESS.len() - 1];
    for channel in ["content", "reasoning_content"] {
        for terminal in 0..3 {
            let mut filter = sse(root.path());
            let mut wire = filter.push(&delta(0, json!({channel:prefix}))).unwrap();
            wire.extend(match terminal {
                0 => filter.push(&finish(0)).unwrap(),
                1 => filter.push(b"data: [DONE]\n\n").unwrap(),
                _ => filter.finish().unwrap(),
            });
            assert!(field(&wire, 0, channel) == "[redacted]");
            assert!(!contains(&wire, &ACCESS.as_bytes()[..20]), "a substantial credential prefix escaped at a terminal boundary");
            assert_safe(&wire);
        }
    }
    for terminal in 0..3 {
        let keys = vec!["eyJ.synthetic-oauth-fixture-token".into()];
        let mut filter = Sse::new(Redactor::new(keys).unwrap().unwrap(), root.path().to_owned());
        let mut wire = filter.push(&delta(0, json!({"content":"legitimate"}))).unwrap();
        assert!(field(&wire, 0, "content") == "legitimat");
        wire.extend(match terminal {
            0 => filter.push(&finish(0)).unwrap(),
            1 => filter.push(b"data: [DONE]\n\n").unwrap(),
            _ => filter.finish().unwrap(),
        });
        assert!(field(&wire, 0, "content") == "legitimate");
    }
}

#[test]
fn split_inner_unicode_escaped_tool_arguments_are_decoded_before_emission() {
    let root = tempfile::tempdir().unwrap();
    let mut filter = sse(root.path());
    let encoded: String = ACCESS.bytes().map(|byte| format!("\\u{byte:04x}")).collect();
    let arguments = format!("{{\"query\":\"{encoded}\",\"nested\":{{\"{RETIRED}\":\"ok\"}}}}");
    let split = 17; // Deliberately in the middle of an inner \\uXXXX escape.
    let mut wire = filter
        .push(&delta(
            0,
            json!({"tool_calls":[
                {"index":0,"id":"call-first","type":"function","function":{"name":"lookup","arguments":&arguments[..split]}},
                {"index":1,"id":"call-second","type":"function","function":{"name":"other","arguments":"{\"limit\":"}}
            ]}),
        ))
        .unwrap();
    assert!(values(&wire)[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"] == "");
    wire.extend(
        filter
            .push(&delta(
                0,
                json!({"tool_calls":[
                    {"index":1,"function":{"arguments":"5}"}},
                    {"index":0,"function":{"arguments":&arguments[split..]}}
                ]}),
            ))
            .unwrap(),
    );
    wire.extend(filter.push(&finish(0)).unwrap());
    wire.extend(filter.finish().unwrap());
    assert_safe(&wire);
    let frames = values(&wire);
    let calls = frames.last().unwrap()["choices"][0]["delta"]["tool_calls"].as_array().unwrap();
    let first = calls.iter().find(|call| call["index"] == 0).unwrap()["function"]["arguments"].as_str().unwrap();
    let first: Value = serde_json::from_str(first).unwrap();
    assert!(first == json!({"query":"[redacted]","nested":{"[redacted]":"ok"}}));
    let second = calls.iter().find(|call| call["index"] == 1).unwrap()["function"]["arguments"].as_str().unwrap();
    assert!(serde_json::from_str::<Value>(second).unwrap() == json!({"limit":5}));
}

#[test]
fn legacy_function_arguments_flush_before_done() {
    let root = tempfile::tempdir().unwrap();
    let mut filter = sse(root.path());
    let mut wire = filter.push(&delta(0, json!({"function_call":{"name":"lookup","arguments":"{\"value\":\""}}))).unwrap();
    wire.extend(filter.push(&delta(0, json!({"function_call":{"arguments":format!("{RETIRED}\"}}")}}))).unwrap());
    wire.extend(filter.push(b"data: [DONE]\n\n").unwrap());
    assert_safe(&wire);
    let frames = values(&wire);
    let function = &frames.last().unwrap()["choices"][0]["delta"]["function_call"];
    assert!(serde_json::from_str::<Value>(function["arguments"].as_str().unwrap()).unwrap() == json!({"value":"[redacted]"}));
    assert!(function.get("name").is_none(), "argument flush must not replace a previously emitted name");
}

#[test]
fn native_last_name_assignment_preserves_search_tool_and_todo_write() {
    let root = tempfile::tempdir().unwrap();
    for expected in ["search_tool", "todo_write"] {
        // The first key shares search_tool's suffix; a JWT-style key shares todo_write's final e.
        // Neither partial match is a credential, and the native sampler assigns complete names.
        let keys = vec!["tool-secret-credential".into(), "eyJ.synthetic-oauth-fixture-token".into()];
        let mut filter = Sse::new(Redactor::new(keys).unwrap().unwrap(), root.path().to_owned());
        let mut wire = filter
            .push(&delta(0, json!({"tool_calls":[{"index":0,"id":"call-name","type":"function","function":{"name":"placeholder","arguments":""}}]})))
            .unwrap();
        let latest = filter.push(&delta(0, json!({"tool_calls":[{"index":0,"function":{"name":expected,"arguments":"{}"}}]}))).unwrap();
        assert!(values(&latest)[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"] == expected);
        wire.extend(latest);
        // The pinned Option<String> fields also accept explicit null continuation fragments.
        wire.extend(filter.push(&delta(0, json!({"tool_calls":[{"index":0,"function":{"name":null,"arguments":null}}]}))).unwrap());
        wire.extend(filter.push(&finish(0)).unwrap());
        let mut actual_name = String::new();
        let mut arguments = String::new();
        // Mirror pinned stream/chat_completions.rs: name replaces, arguments append.
        for frame in values(&wire) {
            for call in frame["choices"][0]["delta"]["tool_calls"].as_array().into_iter().flatten() {
                if let Some(name) = call["function"]["name"].as_str() {
                    actual_name = name.to_owned();
                }
                if let Some(fragment) = call["function"]["arguments"].as_str() {
                    arguments.push_str(fragment);
                }
            }
        }
        assert!(actual_name == expected, "the final native tool name must remain the complete latest name");
        assert!(serde_json::from_str::<Value>(&arguments).unwrap() == json!({}));
    }
}

#[tokio::test]
async fn json_recursively_scrubs_escaped_values_keys_errors_and_inner_tool_json() {
    let root = tempfile::tempdir().unwrap();
    let arguments = format!(r#"{{"value":"{}"}}"#, ACCESS.replace('s', "\\u0073"));
    let input = json!({
        "error":{"message":RETIRED,"status":format!("failed {ACCESS}")},
        ACCESS:[{"nested":RETIRED}],
        "choices":[{"message":{"content":ACCESS,"reasoning_content":RETIRED,"tool_calls":[{"function":{"name":"lookup","arguments":arguments}}]}}]
    });
    let raw = serde_json::to_string(&input).unwrap().replace("synthetic-oauth", "\\u0073ynthetic-oauth");
    let out = redaction::rewrite_body(Bytes::from(raw), root.path().to_owned(), redactor(), true, true).await.unwrap();
    assert_safe(&out);
    let parsed: Value = serde_json::from_slice(&out).unwrap();
    assert!(parsed["error"]["message"] == "[redacted]");
    assert!(parsed["[redacted]"][0]["nested"] == "[redacted]");
    let arguments: Value = serde_json::from_str(parsed["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert!(arguments["value"] == "[redacted]");
}

#[test]
fn comments_status_and_error_events_are_scrubbed_and_normal_events_are_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let mut filter = sse(root.path());
    let normal =
        b": keep-alive\r\nevent: message\r\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ordinary text\"},\"finish_reason\":null}]}\r\n\r\n";
    assert!(filter.push(normal).unwrap() == normal);
    let input =
        format!(": {ACCESS}\nevent: {RETIRED}\nstatus: rejected {ACCESS}\ndata: {{\"error\":{{\"message\":\"{}\"}}}}\n\n", ACCESS.replace('s', "\\u0073"));
    let out = filter.push(input.as_bytes()).unwrap();
    assert_safe(&out);
    assert!(std::str::from_utf8(&out).unwrap().contains(": [redacted]\nevent: [redacted]\nstatus: rejected [redacted]"));
    assert!(values(&out)[0]["error"]["message"] == "[redacted]");
    let multiline = format!("data: {{\"error\":\ndata: {{\"message\":\"{RETIRED}\"}}}}\n\n");
    let out = filter.push(multiline.as_bytes()).unwrap();
    assert_safe(&out);
    assert!(values(&out)[0]["error"]["message"] == "[redacted]");
}

#[tokio::test]
async fn ordinary_json_plain_text_and_images_keep_their_existing_semantics() {
    let root = tempfile::tempdir().unwrap();
    let normal = Bytes::from_static(b"{ \"choices\": [ { \"message\": { \"content\": \"ordinary text\" } } ] }");
    let out = redaction::rewrite_body(normal.clone(), root.path().to_owned(), redactor(), true, true).await.unwrap();
    assert!(out == normal);
    let text = Bytes::from(format!("provider rejected {RETIRED}"));
    let out = redaction::rewrite_body(text, root.path().to_owned(), redactor(), false, false).await.unwrap();
    assert_safe(&out);
    assert!(out == "provider rejected [redacted]");
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==";
    let input = delta(0, json!({"images":[{"image_url":{"url":format!("data:image/png;base64,{png}")}}]}));
    let mut filter = sse(root.path());
    let out = filter.push(&input).unwrap();
    let parsed = values(&out);
    let delta = &parsed[0]["choices"][0]["delta"];
    assert!(delta.get("images").is_none());
    let content = delta["content"].as_str().unwrap();
    let id = content.split(ROUTE_PREFIX).nth(1).unwrap().split(')').next().unwrap();
    assert!(grok_bot_store::generated_images::read_generated_image(root.path(), id).is_ok());
}

#[test]
fn ignored_delta_extensions_are_omitted_without_rejecting_ordinary_content() {
    let root = tempfile::tempdir().unwrap();
    for details in [json!([]), json!([{"type":"reasoning.text","text":&ACCESS[..ACCESS.len()-1]}])] {
        let mut filter = sse(root.path());
        let mut wire = filter.push(&delta(0, json!({"content":"ordinary ","reasoning_details":details,"audio":{"data":ACCESS}}))).unwrap();
        wire.extend(filter.push(&delta(0, json!({"content":"response","reasoning_details":[{"text":&ACCESS[ACCESS.len()-1..]}]}))).unwrap());
        wire.extend(filter.push(&finish(0)).unwrap());
        assert!(field(&wire, 0, "content") == "ordinary response");
        assert_safe(&wire);
        assert!(!contains(&wire, &ACCESS.as_bytes()[..20]), "an ignored extension leaked a credential prefix");
        for value in values(&wire) {
            let delta = &value["choices"][0]["delta"];
            assert!(delta.get("reasoning_details").is_none());
            assert!(delta.get("audio").is_none());
        }
    }
}

#[test]
fn malformed_or_unsupported_frames_and_excess_state_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    for input in [b"data: not-json\n\n".as_slice(), b"data: {\"delta\":\"unknown fragment\"}\n\n", b"data: \xff\n\n"] {
        assert!(sse(root.path()).push(input).is_err(), "unsupported SSE must not forward unparsed bytes");
    }
    let mut filter = sse(root.path());
    let pending = delta(0, json!({"tool_calls":[{"index":0,"function":{"arguments":"{broken"}}]}));
    filter.push(&pending).unwrap();
    assert!(filter.push(&finish(0)).is_err(), "invalid inner tool JSON must fail closed");
    let calls: Vec<_> = (0..1025).map(|index| json!({"index":index,"function":{"arguments":""}})).collect();
    assert!(sse(root.path()).push(&delta(0, json!({"tool_calls":calls}))).is_err());
    assert!(sse(root.path()).push(": pending\n".repeat(16385).as_bytes()).is_err());
    assert!(Redactor::new((0..1025).map(|index| format!("synthetic-budget-key-{index}")).collect()).is_err());
    assert!(Redactor::new(Vec::new()).unwrap().is_none());
}

#[tokio::test]
async fn real_forward_filters_error_json_headers_and_freezes_external_key_snapshot() {
    use axum::routing::post;
    use grok_bot_model::SettingsInput;

    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path(), Arc::new(chrono::Utc::now)).await.unwrap();
    let response_store = store.clone();
    let provider = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let store = response_store.clone();
            async move {
                // Request-start keys still protect the response if credentials retire/change while it
                // is in flight. Keys registered only after this snapshot belong to a later request.
                store.set_external_redaction_keys(vec!["synthetic-newer-oauth-key".into()]);
                let body = serde_json::to_vec(&json!({"error":{"message":ACCESS,"nested":[RETIRED]}})).unwrap();
                let length = body.len();
                let mut response = Response::new(Body::from(body));
                *response.status_mut() = StatusCode::UNAUTHORIZED;
                response.headers_mut().insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
                response.headers_mut().insert("x-provider-status", format!("rejected {RETIRED}").parse().unwrap());
                response.headers_mut().insert(header::CONTENT_LENGTH, length.to_string().parse().unwrap());
                response.headers_mut().insert(header::CONTENT_ENCODING, "identity".parse().unwrap());
                response
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
    store
        .mutate(|state| {
            state.update_settings(SettingsInput {
                base_url: format!("http://{address}/v1"),
                model: "fixture".into(),
                workspace: String::new(),
                api_key: None,
                clear_api_key: Some(true),
            })
        })
        .await
        .unwrap();
    store.set_external_redaction_keys(vec![ACCESS.into(), RETIRED.into()]);
    let relay = start(store).await.unwrap();
    let response = reqwest::Client::new().post(format!("{}/chat/completions", relay.base_url)).bearer_auth(relay.token).body("{}").send().await.unwrap();
    assert!(response.status() == StatusCode::UNAUTHORIZED);
    assert!(response.headers()["x-provider-status"] == "rejected [redacted]");
    assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
    let length = response.headers().get(header::CONTENT_LENGTH).map(|value| value.to_str().unwrap().parse::<usize>().unwrap());
    let bytes = response.bytes().await.unwrap();
    assert_safe(&bytes);
    assert!(length.is_none_or(|length| length == bytes.len()));
    assert!(serde_json::from_slice::<Value>(&bytes).unwrap()["error"]["message"] == "[redacted]");
    task.abort();
    let _ = task.await;
}
