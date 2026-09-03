//! Responses output encoding over the existing OpenAI execution stream.
//! Keeping this adapter outside the upstream loop preserves its retries,
//! backpressure, accounting, filters, and cancellation behavior.

use std::collections::HashMap;
use std::convert::Infallible;

use axum::body::{Body, Bytes};
use axum::http::header;
use axum::response::Response;
use bytes::BytesMut;
use futures::StreamExt;
use kproxy_translate::{ResponsesRequest, ResponsesToolName};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::meter::now_secs;

pub(super) fn is_responses_path(path: &str) -> bool {
    matches!(path, "/v1/responses" | "/responses")
}

pub(super) struct ResponsesOptions {
    template: Value,
    tool_names: HashMap<String, ResponsesToolName>,
}

impl ResponsesOptions {
    pub fn new(request: &ResponsesRequest, tool_names: HashMap<String, ResponsesToolName>) -> Self {
        Self {
            template: json!({
                "id":format!("resp_{}", Uuid::new_v4().simple()),
                "object":"response", "created_at":now_secs(),
                "status":"in_progress", "error":null, "incomplete_details":null,
                "model":request.model, "instructions":request.instructions,
                "output":[], "usage":null, "store":false, "background":false,
                "previous_response_id":null,
                "max_output_tokens":request.max_output_tokens,
                "parallel_tool_calls":request.parallel_tool_calls.unwrap_or(true),
                "tool_choice":request.tool_choice.as_ref().unwrap_or(&json!("auto")),
                "tools":request.tools, "reasoning":request.reasoning,
                "text":request.text.as_ref().map(|text| json!({
                    "format":text.format.as_ref().unwrap_or(&json!({"type":"text"})),
                    "verbosity":text.verbosity.as_deref().unwrap_or("medium")
                })).unwrap_or_else(|| json!({"format":{"type":"text"}})),
                "temperature":request.temperature.unwrap_or(1.0),
                "top_p":request.top_p.unwrap_or(1.0),
                "truncation":"disabled", "service_tier":"auto",
                "metadata":request.metadata.as_ref().unwrap_or(&json!({}))
            }),
            tool_names,
        }
    }

    pub fn id(&self) -> &str {
        self.template["id"].as_str().expect("response id")
    }

    fn envelope(&self, output: Vec<Value>, status: &str, usage: Value, error: Value) -> Value {
        let mut response = self.template.clone();
        response["output"] = json!(output);
        response["status"] = json!(status);
        response["usage"] = usage;
        response["error"] = error;
        if status == "incomplete" {
            response["incomplete_details"] = json!({"reason":"max_output_tokens"});
        }
        response
    }

    fn tool(&self, call: &Value) -> ItemContent {
        let custom = call["type"] == "custom";
        let body = &call[if custom { "custom" } else { "function" }];
        let name = body["name"].as_str().unwrap_or_default();
        let identity = self.tool_names.get(name);
        ItemContent::Tool {
            custom,
            call_id: call["id"].as_str().unwrap_or_default().into(),
            name: identity
                .map_or(name, |identity| identity.name.as_str())
                .into(),
            namespace: identity.and_then(|identity| identity.namespace.clone()),
            input: body[if custom { "input" } else { "arguments" }]
                .as_str()
                .unwrap_or_default()
                .into(),
        }
    }
}

pub(super) fn json_response(chat: Value, options: ResponsesOptions) -> Value {
    let message = &chat["choices"][0]["message"];
    let mut output = Vec::new();
    if let Some(text) = message["reasoning_content"]
        .as_str()
        .filter(|text| !text.is_empty())
    {
        output.push(OutputItem::new(ItemContent::Reasoning(text.into())).json("completed"));
    }
    if let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
        output.push(OutputItem::new(ItemContent::Message(text.into())).json("completed"));
    }
    if let Some(calls) = message["tool_calls"].as_array() {
        output.extend(
            calls
                .iter()
                .map(|call| OutputItem::new(options.tool(call)).json("completed")),
        );
    }
    let incomplete = chat["choices"][0]["finish_reason"] == "length";
    let status = if incomplete {
        "incomplete"
    } else {
        "completed"
    };
    if output.is_empty() {
        output.push(OutputItem::new(ItemContent::Message(String::new())).json(status));
    } else if incomplete {
        for item in &mut output {
            if item["type"] == "message" {
                item["status"] = json!("incomplete");
            }
        }
    }
    options.envelope(output, status, usage(&chat["usage"]), Value::Null)
}

enum ItemContent {
    Message(String),
    Reasoning(String),
    Tool {
        custom: bool,
        call_id: String,
        name: String,
        namespace: Option<String>,
        input: String,
    },
}

struct OutputItem {
    id: String,
    content: ItemContent,
    status: &'static str,
}

impl OutputItem {
    fn new(content: ItemContent) -> Self {
        let prefix = match &content {
            ItemContent::Message(_) => "msg",
            ItemContent::Reasoning(_) => "rs",
            ItemContent::Tool { custom: true, .. } => "ctc",
            ItemContent::Tool { .. } => "fc",
        };
        Self {
            id: format!("{prefix}_{}", Uuid::new_v4().simple()),
            content,
            status: "in_progress",
        }
    }

    fn json(&self, status: &str) -> Value {
        match &self.content {
            ItemContent::Message(text) => json!({
                "id":self.id,"type":"message","role":"assistant","status":status,
                "content":if status == "in_progress" && text.is_empty() { Vec::new() } else { vec![text_part(text)] }
            }),
            ItemContent::Reasoning(text) => json!({
                "id":self.id,"type":"reasoning",
                "summary":if text.is_empty() { Vec::new() } else { vec![json!({"type":"summary_text","text":text})] }
            }),
            ItemContent::Tool {
                custom,
                call_id,
                name,
                namespace,
                input,
            } => {
                let mut item = json!({
                    "id":self.id,"type":if *custom { "custom_tool_call" } else { "function_call" },
                    "call_id":call_id,"name":name,"status":status
                });
                item[if *custom { "input" } else { "arguments" }] = json!(input);
                if let Some(namespace) = namespace {
                    item["namespace"] = json!(namespace);
                }
                item
            }
        }
    }
}

fn text_part(text: &str) -> Value {
    json!({"type":"output_text","text":text,"annotations":[],"logprobs":[]})
}

fn usage(chat: &Value) -> Value {
    let input = chat["prompt_tokens"].as_u64().unwrap_or_default();
    let output = chat["completion_tokens"].as_u64().unwrap_or_default();
    json!({
        "input_tokens":input,"output_tokens":output,"total_tokens":input.saturating_add(output),
        "input_tokens_details":{"cached_tokens":chat["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or_default()},
        "output_tokens_details":{"reasoning_tokens":chat["completion_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or_default()}
    })
}

struct ResponsesStream {
    options: ResponsesOptions,
    items: Vec<OutputItem>,
    active_text: Option<usize>,
    active_reasoning: Option<usize>,
    tool_indices: HashMap<u64, usize>,
    sequence: u64,
    usage: Value,
    finish_reason: Option<String>,
    terminal: bool,
}

impl ResponsesStream {
    fn new(options: ResponsesOptions) -> Self {
        Self {
            options,
            items: Vec::new(),
            active_text: None,
            active_reasoning: None,
            tool_indices: HashMap::new(),
            sequence: 0,
            usage: Value::Null,
            finish_reason: None,
            terminal: false,
        }
    }

    fn event(&mut self, mut value: Value) -> String {
        value["sequence_number"] = json!(self.sequence);
        self.sequence += 1;
        format!(
            "event: {}\ndata: {value}\n\n",
            value["type"].as_str().expect("event type")
        )
    }

    fn start(&mut self) -> Vec<String> {
        ["response.created", "response.in_progress"]
            .into_iter()
            .map(|kind| self.event(json!({"type":kind,"response":self.options.template})))
            .collect()
    }

    fn add(&mut self, content: ItemContent, events: &mut Vec<String>) -> usize {
        let index = self.items.len();
        let item = OutputItem::new(content);
        events.push(self.event(json!({"type":"response.output_item.added","output_index":index,"item":item.json("in_progress")})));
        self.items.push(item);
        index
    }

    fn close(&mut self, index: usize, status: &'static str, events: &mut Vec<String>) {
        if self.items[index].status != "in_progress" {
            return;
        }
        let item = &self.items[index];
        let mut values = match &item.content {
            ItemContent::Message(text) => vec![
                json!({"type":"response.output_text.done","item_id":item.id,"output_index":index,"content_index":0,"text":text,"logprobs":[]}),
                json!({"type":"response.content_part.done","item_id":item.id,"output_index":index,"content_index":0,"part":text_part(text)}),
            ],
            ItemContent::Reasoning(text) => vec![
                json!({"type":"response.reasoning_summary_text.done","item_id":item.id,"output_index":index,"summary_index":0,"text":text}),
                json!({"type":"response.reasoning_summary_part.done","item_id":item.id,"output_index":index,"summary_index":0,"part":{"type":"summary_text","text":text}}),
            ],
            ItemContent::Tool {
                custom,
                name,
                input,
                ..
            } => {
                if *custom {
                    vec![
                        json!({"type":"response.custom_tool_call_input.done","item_id":item.id,"output_index":index,"input":input}),
                    ]
                } else {
                    vec![
                        json!({"type":"response.function_call_arguments.done","item_id":item.id,"output_index":index,"name":name,"arguments":input}),
                    ]
                }
            }
        };
        values.push(json!({"type":"response.output_item.done","output_index":index,"item":item.json(status)}));
        self.items[index].status = status;
        events.extend(values.into_iter().map(|value| self.event(value)));
    }

    fn close_text(&mut self, events: &mut Vec<String>) {
        for index in [self.active_text.take(), self.active_reasoning.take()]
            .into_iter()
            .flatten()
        {
            self.close(index, "completed", events);
        }
    }

    fn text(&mut self, text: &str, reasoning: bool, events: &mut Vec<String>) {
        if text.is_empty() {
            return;
        }
        let active = if reasoning {
            self.active_reasoning
        } else {
            self.active_text
        };
        let index = if let Some(index) = active {
            index
        } else {
            self.close_text(events);
            let content = if reasoning {
                ItemContent::Reasoning(String::new())
            } else {
                ItemContent::Message(String::new())
            };
            let index = self.add(content, events);
            if reasoning {
                self.active_reasoning = Some(index);
                events.push(self.event(json!({"type":"response.reasoning_summary_part.added","item_id":self.items[index].id,"output_index":index,"summary_index":0,"part":{"type":"summary_text","text":""}})));
            } else {
                self.active_text = Some(index);
                events.push(self.event(json!({"type":"response.content_part.added","item_id":self.items[index].id,"output_index":index,"content_index":0,"part":text_part("")})));
            }
            index
        };
        match &mut self.items[index].content {
            ItemContent::Message(content) | ItemContent::Reasoning(content) => {
                content.push_str(text)
            }
            _ => unreachable!("text item"),
        }
        let value = if reasoning {
            json!({"type":"response.reasoning_summary_text.delta","item_id":self.items[index].id,"output_index":index,"summary_index":0,"delta":text})
        } else {
            json!({"type":"response.output_text.delta","item_id":self.items[index].id,"output_index":index,"content_index":0,"delta":text,"logprobs":[]})
        };
        events.push(self.event(value));
    }

    fn chunk(&mut self, chat: Value) -> Vec<String> {
        if self.terminal {
            return Vec::new();
        }
        if let Some(error) = chat.get("error") {
            return self.fail(
                error["code"].as_str().unwrap_or("upstream_error"),
                error["message"]
                    .as_str()
                    .unwrap_or("upstream stream failed"),
            );
        }
        if chat.get("usage").is_some_and(Value::is_object) {
            self.usage = usage(&chat["usage"]);
        }
        let mut events = Vec::new();
        let Some(choices) = chat["choices"].as_array() else {
            return self.fail("invalid_stream", "invalid internal OpenAI stream");
        };
        for choice in choices {
            if let Some(reason) = choice["finish_reason"].as_str() {
                self.finish_reason = Some(reason.into());
            }
            let delta = &choice["delta"];
            if let Some(text) = delta["reasoning_content"].as_str() {
                self.text(text, true, &mut events);
            }
            if let Some(text) = delta["content"].as_str() {
                self.text(text, false, &mut events);
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                self.close_text(&mut events);
                for call in calls {
                    let Some(tool_index) = call["index"].as_u64() else {
                        events
                            .extend(self.fail("invalid_stream", "tool delta is missing its index"));
                        return events;
                    };
                    let index = if let Some(index) = self.tool_indices.get(&tool_index) {
                        *index
                    } else {
                        let mut tool = self.options.tool(call);
                        let ItemContent::Tool {
                            call_id,
                            name,
                            input,
                            ..
                        } = &mut tool
                        else {
                            unreachable!()
                        };
                        if call_id.is_empty() || name.is_empty() {
                            events.extend(self.fail(
                                "invalid_stream",
                                "tool delta is missing its call id or name",
                            ));
                            return events;
                        }
                        input.clear();
                        let index = self.add(tool, &mut events);
                        self.tool_indices.insert(tool_index, index);
                        index
                    };
                    let ItemContent::Tool {
                        custom,
                        input,
                        call_id,
                        ..
                    } = &mut self.items[index].content
                    else {
                        unreachable!()
                    };
                    let body = &call[if *custom { "custom" } else { "function" }];
                    let added = body[if *custom { "input" } else { "arguments" }]
                        .as_str()
                        .unwrap_or_default();
                    input.push_str(added);
                    let custom = *custom;
                    let call_id = call_id.clone();
                    if !added.is_empty() {
                        events.push(self.event(json!({
                            "type":if custom { "response.custom_tool_call_input.delta" } else { "response.function_call_arguments.delta" },
                            "item_id":self.items[index].id,"output_index":index,"call_id":call_id,"delta":added
                        })));
                    }
                }
            }
        }
        events
    }

    fn finish(&mut self) -> Vec<String> {
        if self.terminal {
            return Vec::new();
        }
        if self.finish_reason.is_none() || self.usage.is_null() {
            return self.fail(
                "incomplete_stream",
                "upstream stream ended without a finish reason and usage",
            );
        }
        let status = if self.finish_reason.as_deref() == Some("length") {
            "incomplete"
        } else {
            "completed"
        };
        let mut events = Vec::new();
        if self.items.is_empty() {
            let index = self.add(ItemContent::Message(String::new()), &mut events);
            events.push(self.event(json!({"type":"response.content_part.added","item_id":self.items[index].id,"output_index":index,"content_index":0,"part":text_part("")})));
        }
        for index in 0..self.items.len() {
            self.close(index, status, &mut events);
        }
        let output = self
            .items
            .iter()
            .map(|item| item.json(item.status))
            .collect();
        let response = self
            .options
            .envelope(output, status, self.usage.clone(), Value::Null);
        events.push(self.event(json!({"type":if status == "incomplete" { "response.incomplete" } else { "response.completed" },"response":response})));
        self.terminal = true;
        events
    }

    fn fail(&mut self, code: &str, message: &str) -> Vec<String> {
        if self.terminal {
            return Vec::new();
        }
        self.terminal = true;
        let output = self
            .items
            .iter()
            .map(|item| {
                item.json(if item.status == "in_progress" {
                    "incomplete"
                } else {
                    item.status
                })
            })
            .collect();
        let response = self.options.envelope(
            output,
            "failed",
            self.usage.clone(),
            json!({"code":code,"message":kproxy_translate::sanitize_error_message(message)}),
        );
        vec![self.event(json!({"type":"response.failed","response":response}))]
    }
}

pub(super) fn stream_response(response: Response, options: ResponsesOptions) -> Response {
    let (mut parts, body) = response.into_parts();
    parts.headers.remove(header::CONTENT_LENGTH);
    let stream = async_stream::stream! {
        let mut state = ResponsesStream::new(options);
        for event in state.start() { yield Ok::<Bytes, Infallible>(Bytes::from(event)); }
        let mut source = body.into_data_stream();
        let mut buffer = BytesMut::new();
        while let Some(chunk) = source.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => {
                    for event in state.fail("incomplete_stream", "upstream stream was interrupted") { yield Ok(Bytes::from(event)); }
                    break;
                }
            };
            buffer.extend_from_slice(&chunk);
            while let Some((end, delimiter)) = frame_end(&buffer) {
                let frame = buffer.split_to(end + delimiter);
                let events = match std::str::from_utf8(&frame[..end]) {
                    Ok(frame) => {
                        let data = frame.lines().filter_map(|line| line.strip_prefix("data:").map(|data| data.strip_prefix(' ').unwrap_or(data))).collect::<Vec<_>>().join("\n");
                        if data.is_empty() {
                            if state.terminal { Vec::new() } else { vec![": keepalive\n\n".into()] }
                        } else if data == "[DONE]" {
                            state.finish()
                        } else {
                            match serde_json::from_str(&data) {
                                Ok(value) => state.chunk(value),
                                Err(_) => state.fail("invalid_stream", "invalid internal OpenAI event JSON"),
                            }
                        }
                    }
                    Err(_) => state.fail("invalid_stream", "invalid internal OpenAI event encoding"),
                };
                for event in events { yield Ok(Bytes::from(event)); }
            }
            // The shared decoder already bounds generated output. Also bound
            // an unterminated adapter frame so a broken stream cannot grow it.
            if buffer.len() > 16 * 1024 * 1024 {
                for event in state.fail("invalid_stream", "internal OpenAI event exceeds the frame limit") { yield Ok(Bytes::from(event)); }
                break;
            }
        }
        if !state.terminal {
            for event in state.fail("incomplete_stream", "upstream stream ended before completion") { yield Ok(Bytes::from(event)); }
        }
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

fn frame_end(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer
        .windows(2)
        .position(|bytes| bytes == b"\n\n")
        .map(|index| (index, 2));
    let crlf = buffer
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .map(|index| (index, 4));
    lf.into_iter().chain(crlf).min_by_key(|(index, _)| *index)
}

#[cfg(test)]
mod tests;
