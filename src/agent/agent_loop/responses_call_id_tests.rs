//! Responses item IDs and tool correlation IDs can differ.
use super::inflight::InflightSet;
use super::message::{assistant_to_value, loop_message_to_value, tool_result_to_value};
use super::tool::{AbortSignal, LoopToolUpdate};
use super::*;
use crate::session::{MessageRole, Session, ToolCallEntry, ToolCallState};
use futures::StreamExt;
use rig::client::CompletionClient;
use rig::completion::message::{ToolCall as RigToolCall, ToolFunction};
use serde_json::{Value, json};
use std::future::Future;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn completed() -> Value {
    json!({"type":"response.completed","sequence_number":99,"response":{
        "id":"resp_test","object":"response","created_at":0,"status":"completed",
        "model":"test-model","output":[],"tools":[]}})
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
        .collect()
}

fn two_calls() -> String {
    let mut events = Vec::new();
    for (index, suffix) in ["first", "second"].iter().enumerate() {
        let item_id = format!("fc_{suffix}");
        let mut item = json!({"type":"function_call","id":item_id,
            "call_id":format!("call_{suffix}"),"name":"read_fixture",
            "arguments":"","status":"in_progress"});
        events.push(
            json!({"type":"response.output_item.added","output_index":index,
            "sequence_number":index*4,"item":item}),
        );
        let args = json!({"file":suffix}).to_string();
        for (offset, delta) in [&args[..8], &args[8..]].iter().enumerate() {
            events.push(json!({"type":"response.function_call_arguments.delta",
                "item_id":item_id,"output_index":index,"sequence_number":index*4+offset+1,"delta":delta}));
        }
        item["arguments"] = json!(args);
        item["status"] = json!("completed");
        events.push(
            json!({"type":"response.output_item.done","output_index":index,
            "sequence_number":index*4+3,"item":item}),
        );
    }
    events.push(completed());
    sse(&events)
}

fn read_request(socket: &mut TcpStream) -> Value {
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buf = [0; 4096];
    let (end, len) = loop {
        let count = socket.read(&mut buf).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buf[..count]);
        if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..i]);
            let len = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|s| s.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            break (i + 4, len);
        }
    };
    while bytes.len() < end + len {
        let count = socket.read(&mut buf).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buf[..count]);
    }
    serde_json::from_slice(&bytes[end..end + len]).unwrap()
}

#[derive(Debug)]
struct ReadFixture {
    dir: std::path::PathBuf,
    parameters: Value,
    calls: Mutex<Vec<String>>,
}

impl Drop for ReadFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl LoopTool for ReadFixture {
    fn name(&self) -> &str {
        "read_fixture"
    }
    fn label(&self) -> &str {
        "Read fixture"
    }
    fn description(&self) -> &str {
        "Read a local fixture"
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }
    fn execute<'a>(
        &'a self,
        id: &'a str,
        args: Value,
        _: AbortSignal,
        _: LoopToolUpdate,
    ) -> Pin<Box<dyn Future<Output = Result<LoopToolResult, String>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(id.to_string());
            let text =
                std::fs::read_to_string(self.dir.join(args["file"].as_str().unwrap())).unwrap();
            Ok(LoopToolResult {
                content: vec![json!({"type":"text","text":text})],
                details: Value::Null,
                terminate: None,
            })
        })
    }
}

async fn response(stream: &StreamFn, messages: Vec<Value>) -> AssistantMessage {
    let mut events = stream(
        LlmContext {
            system_prompt: String::new(),
            messages,
            asset_dir: None,
        },
        StreamOptions::from_signal(AbortSignal::new()),
    );
    let mut result = None;
    while let Some(event) = events.next().await {
        match event {
            StreamEvent::Done { message, .. } => result = Some(message),
            StreamEvent::Error { error } => panic!("{error}"),
            _ => {}
        }
    }
    result.expect("completed response")
}

async fn round_trip(resume: bool, gemini: bool) {
    // Both requests use the real provider adapter and Dirge's history factory.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        let bodies = if gemini {
            let call = json!({"candidates":[{"content":{"role":"model","parts":[{
                "functionCall":{"id":"gem-1","name":"read_fixture","args":{"file":"first"}},
                "thoughtSignature":"gemini-signature"}]},"finishReason":"STOP","index":0}]});
            let done = json!({"candidates":[{"content":{"role":"model","parts":[{"text":"done"}]},"finishReason":"STOP","index":0}]});
            [format!("data: {call}\n\n"), format!("data: {done}\n\n")]
        } else {
            [two_calls(), sse(&[completed()])]
        };
        for body in bodies {
            let (mut socket, _) = listener.accept().unwrap();
            requests.push(read_request(&mut socket));
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
        requests
    });
    let tool = Arc::new(ReadFixture {
        dir: std::env::temp_dir().join(format!("dirge-call-id-{}", uuid::Uuid::new_v4())),
        parameters: json!({"type":"object","properties":{"file":{"type":"string"}},"required":["file"]}),
        calls: Mutex::new(Vec::new()),
    });
    std::fs::create_dir_all(&tool.dir).unwrap();
    for suffix in ["first", "second"] {
        std::fs::write(tool.dir.join(suffix), format!("contents-{suffix}")).unwrap();
    }
    let stream = if gemini {
        let client = rig::providers::gemini::Client::builder()
            .api_key("local-test-no-credentials")
            .base_url(format!("http://{address}"))
            .build()
            .unwrap();
        rig_stream_fn_from_model_with_provider(
            client.completion_model("test-model"),
            vec![loop_tool_to_rig_definition(tool.as_ref())],
            Some(Duration::from_secs(10)),
            Some("gemini".to_string()),
            Some("test-model".to_string()),
        )
    } else {
        let client = rig::providers::openai::Client::builder()
            .api_key("local-test-no-credentials")
            .base_url(format!("http://{address}"))
            .http_client(crate::provider::codex_http::CodexHttpClient::default())
            .build()
            .unwrap();
        rig_stream_fn_from_model_with_provider(
            client.completion_model("test-model"),
            vec![loop_tool_to_rig_definition(tool.as_ref())],
            Some(Duration::from_secs(10)),
            Some("openai".to_string()),
            Some("test-model".to_string()),
        )
    };
    let user = json!({"role":"user","content":"Read both fixtures"});
    let assistant = response(&stream, vec![user.clone()]).await;
    let context = Context {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![tool.clone()],
    };
    let config = LoopConfig::for_tests(Arc::new(|messages| messages.to_vec()));
    let (tx, _rx) = tokio::sync::mpsc::channel(32);
    let batch = execute_tool_calls_from_msg(
        &context,
        &assistant,
        &config,
        &AbortSignal::new(),
        &tx,
        &InflightSet::new(),
    )
    .await;
    let calls = extract_tool_calls(&assistant);
    let messages = if resume {
        let mut session = Session::new("openai", "test-model", 0);
        session.add_message(MessageRole::User, "Read both fixtures");
        session.add_message_with_tool_calls(
            MessageRole::Assistant,
            "",
            calls
                .iter()
                .map(|call| {
                    let result = batch
                        .messages
                        .iter()
                        .find(|result| result.tool_call_id == call.id)
                        .unwrap();
                    ToolCallEntry {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        args: call.arguments.clone(),
                        state: ToolCallState::Completed {
                            result: result
                                .content
                                .iter()
                                .filter_map(|c| match c {
                                    ContentBlock::Text { text } => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        },
                    }
                })
                .collect(),
        );
        crate::session::storage::save_session(&mut session).unwrap();
        let restored = crate::session::storage::load_session(&session.id).unwrap();
        crate::session::storage::delete_session(&session.id).unwrap();
        rig_history_to_loop_messages(crate::agent::runner::convert_history(&restored))
            .iter()
            .map(loop_message_to_value)
            .collect()
    } else {
        let mut messages = vec![user, assistant_to_value(&assistant)];
        messages.extend(batch.messages.iter().map(tool_result_to_value));
        messages
    };
    response(&stream, messages).await;
    let requests = server.join().unwrap();
    let mut executed = tool.calls.lock().unwrap().clone();
    executed.sort();
    if gemini {
        assert_eq!(executed, ["read_fixture"]);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "read_fixture");
        assert_eq!(batch.messages[0].tool_call_id, "read_fixture");
        let parts: Vec<&Value> = requests[1]["contents"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|content| content["parts"].as_array().unwrap())
            .collect();
        let call = parts
            .iter()
            .find(|part| part.get("functionCall").is_some())
            .unwrap();
        assert_eq!(call["functionCall"]["name"], "read_fixture");
        assert_eq!(call["thoughtSignature"], "gemini-signature");
        let result = parts
            .iter()
            .find(|part| part.get("functionResponse").is_some())
            .unwrap();
        assert_eq!(result["functionResponse"]["name"], "read_fixture");
        return;
    }
    assert_eq!(executed, ["call_first", "call_second"]);
    assert_eq!(
        calls.len(),
        2,
        "delta and completed events must not duplicate calls"
    );
    assert_eq!(batch.messages.len(), 2);
    let input = requests[1]["input"].as_array().unwrap();
    for suffix in ["first", "second"] {
        let id = format!("call_{suffix}");
        let call = input
            .iter()
            .find(|v| v["type"] == "function_call" && v["call_id"] == id)
            .unwrap();
        assert_eq!(call["name"], "read_fixture");
        assert_eq!(
            serde_json::from_str::<Value>(call["arguments"].as_str().unwrap()).unwrap(),
            json!({"file":suffix})
        );
        let result = input
            .iter()
            .find(|v| v["type"] == "function_call_output" && v["call_id"] == id)
            .unwrap();
        assert_eq!(result["output"], format!("contents-{suffix}"));
    }
}

#[tokio::test]
async fn responses_call_id_survives_two_streamed_calls_and_execution() {
    round_trip(false, false).await;
}

#[tokio::test]
async fn responses_call_id_survives_session_resume() {
    round_trip(true, false).await;
}

#[tokio::test]
async fn responses_call_id_does_not_replace_gemini_function_name() {
    round_trip(false, true).await;
}

#[tokio::test]
async fn responses_call_id_absent_preserves_id_and_gemini_signature() {
    use rig::streaming::StreamedAssistantContent;
    let raw = futures::stream::iter(vec![Ok(StreamedAssistantContent::<
        rig::providers::openai::responses_api::streaming::StreamingCompletionResponse,
    >::ToolCall {
        tool_call: RigToolCall {
            id: "chat-call".to_string(),
            call_id: None,
            function: ToolFunction {
                name: "read_fixture".to_string(),
                arguments: json!({"file":"first"}),
            },
            signature: Some("gemini-signature".to_string()),
            additional_params: None,
        },
        internal_call_id: "internal".to_string(),
    })]);
    let mut events = rig_stream::wrap_streamed_assistant(Box::pin(raw), None, None);
    let mut done = None;
    while let Some(event) = events.next().await {
        if let StreamEvent::Done { message, .. } = event {
            done = Some(message);
        }
    }
    let message = done.unwrap();
    let ContentBlock::ToolCall {
        id,
        signature,
        arguments,
        ..
    } = &message.content[0]
    else {
        panic!("tool call")
    };
    assert_eq!(id, "chat-call");
    assert_eq!(signature.as_deref(), Some("gemini-signature"));
    assert_eq!(arguments, &json!({"file":"first"}));
}
