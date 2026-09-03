use super::*;
use axum::body::to_bytes;
use kproxy_translate::responses_to_openai;

fn options() -> ResponsesOptions {
    let request: ResponsesRequest = serde_json::from_value(json!({
        "model":"claude-sonnet-4.5","input":"hello","store":false,
        "tools":[{"type":"namespace","name":"functions","tools":[
            {"type":"function","name":"read_file","parameters":{"type":"object"}},
            {"type":"custom","name":"apply_patch"}
        ]}]
    }))
    .unwrap();
    let translated = responses_to_openai(&request).unwrap();
    ResponsesOptions::new(&request, translated.tool_names)
}

fn chunk(delta: Value) -> Value {
    json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]})
}

fn usage_chunk() -> Value {
    json!({"choices":[],"usage":{
        "prompt_tokens":31,"completion_tokens":12,"total_tokens":43,
        "prompt_tokens_details":{"cached_tokens":8},"completion_tokens_details":{"reasoning_tokens":5}
    }})
}

fn events(frames: &[String]) -> Vec<Value> {
    frames
        .iter()
        .filter_map(|frame| {
            frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .map(|data| {
                    let value: Value = serde_json::from_str(data).expect("Responses event JSON");
                    assert!(
                        frame.starts_with(&format!("event: {}\n", value["type"].as_str().unwrap()))
                    );
                    value
                })
        })
        .collect()
}

#[test]
fn mixed_stream_has_stable_ids_order_complete_items_and_usage() {
    let mut stream = ResponsesStream::new(options());
    let id = stream.options.id().to_owned();
    let mut frames = stream.start();
    frames.extend(stream.chunk(chunk(json!({"reasoning_content":"Read "}))));
    frames.extend(stream.chunk(chunk(json!({"reasoning_content":"first."}))));
    frames.extend(stream.chunk(chunk(json!({"content":"Checking 文件."}))));
    frames.extend(stream.chunk(chunk(json!({"tool_calls":[
        {"index":0,"id":"call_read","type":"function","function":{"name":"functions.read_file","arguments":"{\"path\":"}}
    ]}))));
    frames.extend(stream.chunk(chunk(json!({"tool_calls":[
        {"index":1,"id":"call_patch","type":"custom","custom":{"name":"functions.apply_patch","input":"*** Begin Patch\n"}}
    ]}))));
    frames.extend(stream.chunk(chunk(json!({"tool_calls":[
        {"index":0,"function":{"arguments":"\"README.md\"}"}},
        {"index":1,"custom":{"input":"*** End Patch"}}
    ]}))));
    frames.extend(stream.chunk(json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})));
    frames.extend(stream.chunk(usage_chunk()));
    frames.extend(stream.finish());
    let events = events(&frames);
    for (sequence, event) in events.iter().enumerate() {
        assert_eq!(event["sequence_number"], sequence as u64);
    }
    assert_eq!(events[0]["type"], "response.created");
    assert_eq!(events[0]["response"]["id"], id);
    assert_eq!(events[0]["response"]["status"], "in_progress");
    let complete = events.last().unwrap();
    assert_eq!(complete["type"], "response.completed");
    let response = &complete["response"];
    assert_eq!(response["id"], id);
    assert_eq!(response["status"], "completed");
    assert_eq!(response["usage"]["input_tokens"], 31);
    assert_eq!(
        response["usage"]["input_tokens_details"]["cached_tokens"],
        8
    );
    assert_eq!(
        response["usage"]["output_tokens_details"]["reasoning_tokens"],
        5
    );
    let output = response["output"].as_array().unwrap();
    assert_eq!(output.len(), 4);
    assert_eq!(output[0]["summary"][0]["text"], "Read first.");
    assert_eq!(output[1]["content"][0]["text"], "Checking 文件.");
    assert_eq!(output[2]["call_id"], "call_read");
    assert_eq!(output[2]["name"], "read_file");
    assert_eq!(output[2]["namespace"], "functions");
    assert_eq!(output[2]["arguments"], "{\"path\":\"README.md\"}");
    assert_eq!(output[3]["type"], "custom_tool_call");
    assert_eq!(output[3]["input"], "*** Begin Patch\n*** End Patch");
    for (index, item) in output.iter().enumerate() {
        let added = events
            .iter()
            .find(|e| e["type"] == "response.output_item.added" && e["output_index"] == index)
            .unwrap();
        let done = events
            .iter()
            .find(|e| e["type"] == "response.output_item.done" && e["output_index"] == index)
            .unwrap();
        assert_eq!(added["item"]["id"], item["id"]);
        assert_eq!(done["item"], *item);
    }
    let summary_done = events
        .iter()
        .position(|e| e["type"] == "response.reasoning_summary_text.done")
        .unwrap();
    let text_delta = events
        .iter()
        .position(|e| e["type"] == "response.output_text.delta")
        .unwrap();
    assert!(summary_done < text_delta);
    assert!(stream.finish().is_empty());
    assert!(stream.fail("late", "late error").is_empty());
}

#[test]
fn nonstream_response_preserves_text_alongside_tools_and_reasoning() {
    let chat = json!({"choices":[{"finish_reason":"tool_calls","message":{
        "content":"Checking.","reasoning_content":"Read first.","tool_calls":[
            {"id":"call_1","type":"function","function":{"name":"functions.read_file","arguments":"{}"}},
            {"id":"call_2","type":"custom","custom":{"name":"functions.apply_patch","input":"a patch"}}
        ]}}],"usage":usage_chunk()["usage"]});
    let response = json_response(chat, options());
    let output = response["output"].as_array().unwrap();
    assert_eq!(output.len(), 4);
    assert_eq!(output[1]["content"][0]["text"], "Checking.");
    assert_eq!(output[2]["call_id"], "call_1");
    assert_eq!(output[3]["input"], "a patch");
    assert_eq!(response["object"], "response");
    assert_eq!(response["usage"]["total_tokens"], 43);
    assert_eq!(response["store"], false);
    assert!(response.get("choices").is_none());
}

#[test]
fn output_limit_is_incomplete_in_both_modes() {
    let mut stream = ResponsesStream::new(options());
    let mut frames = stream.start();
    frames.extend(stream.chunk(chunk(json!({"content":"partial"}))));
    frames.extend(stream.chunk(json!({"choices":[{"delta":{},"finish_reason":"length"}]})));
    frames.extend(stream.chunk(usage_chunk()));
    frames.extend(stream.finish());
    let events = events(&frames);
    let last = events.last().unwrap();
    assert_eq!(last["type"], "response.incomplete");
    assert_eq!(
        last["response"]["incomplete_details"]["reason"],
        "max_output_tokens"
    );
    assert_eq!(last["response"]["output"][0]["status"], "incomplete");
    let response = json_response(
        json!({"choices":[{"finish_reason":"length","message":{"content":"partial"}}],"usage":usage_chunk()["usage"]}),
        options(),
    );
    assert_eq!(response["status"], "incomplete");
    assert_eq!(response["output"][0]["status"], "incomplete");
}

#[test]
fn failed_stream_never_completes_a_partial_tool_call() {
    let mut stream = ResponsesStream::new(options());
    let mut frames = stream.start();
    frames.extend(stream.chunk(chunk(json!({"tool_calls":[{"index":0,"id":"call","type":"function","function":{"name":"functions.read_file","arguments":"{"}}]}))));
    frames.extend(stream.chunk(json!({"error":{"type":"server_error","code":"upstream_timeout","message":"upstream timed out"}})));
    frames.extend(stream.finish());
    let events = events(&frames);
    let last = events.last().unwrap();
    assert_eq!(last["type"], "response.failed");
    assert_eq!(last["response"]["error"]["code"], "upstream_timeout");
    assert_eq!(last["response"]["output"][0]["status"], "incomplete");
    assert!(!events.iter().any(|e| matches!(
        e["type"].as_str(),
        Some("response.completed" | "response.output_item.done")
    )));
}

#[test]
fn incomplete_response_keeps_the_status_of_already_finished_items() {
    let mut stream = ResponsesStream::new(options());
    let mut frames = stream.start();
    frames.extend(stream.chunk(chunk(json!({"content":"An initial observation."}))));
    frames.extend(stream.chunk(chunk(json!({"reasoning_content":"Checking the details."}))));
    frames.extend(stream.chunk(chunk(json!({"content":"A partial answer"}))));
    frames.extend(stream.chunk(json!({"choices":[{"delta":{},"finish_reason":"length"}]})));
    frames.extend(stream.chunk(usage_chunk()));
    frames.extend(stream.finish());
    let events = events(&frames);
    let response = &events.last().unwrap()["response"];
    assert_eq!(response["status"], "incomplete");
    assert_eq!(response["output"][0]["status"], "completed");
    assert_eq!(response["output"][2]["status"], "incomplete");
    for (index, item) in response["output"].as_array().unwrap().iter().enumerate() {
        let done = events
            .iter()
            .find(|event| {
                event["type"] == "response.output_item.done" && event["output_index"] == index
            })
            .unwrap();
        assert_eq!(done["item"], *item);
    }
}

#[tokio::test]
async fn adapter_handles_fragmented_utf8_crlf_multiple_frames_and_keepalives() {
    let wire = format!(
        ": ping\r\n\r\ndata: {}\r\n\r\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(json!({"content":"你好 🦀"})),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        usage_chunk()
    );
    // Split inside every multibyte code point and every SSE separator.
    let pieces = wire
        .as_bytes()
        .chunks(1)
        .map(|piece| Ok::<_, Infallible>(Bytes::copy_from_slice(piece)))
        .collect::<Vec<_>>();
    let response = stream_response(
        Response::new(Body::from_stream(futures::stream::iter(pieces))),
        options(),
    );
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let output = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(output.contains(": keepalive\n\n"));
    assert!(!output.contains("[DONE]"));
    let frames = output
        .split("\n\n")
        .filter(|frame| frame.starts_with("event:"))
        .map(|frame| format!("{frame}\n\n"))
        .collect::<Vec<_>>();
    let events = events(&frames);
    assert_eq!(
        events.last().unwrap()["response"]["output"][0]["content"][0]["text"],
        "你好 🦀"
    );
}

#[tokio::test]
async fn adapter_reports_truncated_and_invalid_streams_as_failed() {
    for wire in [
        format!("data: {}\n\n", chunk(json!({"content":"partial"}))),
        "data: {bad json}\n\n".into(),
        "data: [DONE]\n\n".into(),
        "data: {\"choices\":".into(),
    ] {
        let response = stream_response(Response::new(Body::from(wire)), options());
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("event: response.failed\n"));
        assert!(!body.contains("event: response.completed\n"));
    }
}
