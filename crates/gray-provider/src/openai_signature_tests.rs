use super::*;
use gray_core::message::{Message, ThoughtSignature, ToolDef};
use gray_core::{Agent, ToolContext, ToolExecutor, ToolOutput};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "google/gemini-4-argon";
const SIGNATURE: &str = "opaque+/=signature";

fn request(messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        system: None,
        messages,
        tools: Vec::new(),
        max_tokens: None,
    }
}

fn sse(chunks: Vec<Value>) -> ResponseTemplate {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn signed_delta(signature: &str) -> Value {
    json!({"choices":[{"delta":{"tool_calls":[{
        "index":0,"extra_content":{"google":{"thought_signature":signature}}
    }]}}]})
}

struct EchoExecutor;

impl ToolExecutor for EchoExecutor {
    fn execute(
        &self,
        _ctx: &ToolContext,
        _name: &str,
        args: Value,
    ) -> futures::future::BoxFuture<'static, ToolOutput> {
        Box::pin(async move { ToolOutput::ok(args.to_string()) })
    }
}

#[tokio::test]
async fn gemini_signature_survives_late_chunk_parallel_tools_and_session_reload() {
    let server = MockServer::start().await;
    let counter = AtomicUsize::new(0);
    Mock::given(wiremock::matchers::method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                sse(vec![
                    json!({"choices":[{"delta":{"tool_calls":[{
                        "index":0,"id":"call-a","type":"function",
                        "function":{"name":"lookup","arguments":"{\"q\":\""}
                    },{
                        "index":1,"id":"call-b","type":"function",
                        "function":{"name":"lookup","arguments":"{\"q\":\"b\"}"}
                    }]}}]}),
                    json!({"choices":[{"delta":{"tool_calls":[{
                        "index":0,"function":{"arguments":"a\"}"}
                    }]}}]}),
                    // Gemini can send the signature after the argument chunks.
                    signed_delta(SIGNATURE),
                    signed_delta(SIGNATURE),
                    json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
                ])
            } else {
                sse(vec![json!({"choices":[{
                    "delta":{"content":"done"},"finish_reason":"stop"
                }]})])
            }
        })
        .mount(&server)
        .await;

    let provider = || OpenAiProvider::new("test-key", MODEL, server.uri(), None, None).unwrap();
    let mut agent =
        Agent::new(Box::new(provider()), Arc::new(EchoExecutor)).with_tools(vec![ToolDef::new(
            "lookup",
            "Echo a query",
            json!({"type":"object","properties":{"q":{"type":"string"}}}),
        )]);
    agent
        .run(Message::user("look up a and b"), ToolContext::default())
        .await
        .unwrap();

    let saved = serde_json::to_string(agent.messages()).unwrap();
    let restored: Vec<Message> = serde_json::from_str(&saved).unwrap();
    let mut resumed =
        Agent::new(Box::new(provider()), Arc::new(EchoExecutor)).with_messages(restored);
    resumed
        .run(Message::user("continue"), ToolContext::default())
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    for req in &requests[1..] {
        let body: Value = req.body_json().unwrap();
        let assistant = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m.get("tool_calls").is_some())
            .unwrap();
        let calls = assistant["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], "call-a");
        assert_eq!(calls[0]["function"]["arguments"], r#"{"q":"a"}"#);
        assert_eq!(
            calls[0]["extra_content"]["google"]["thought_signature"],
            SIGNATURE
        );
        assert!(
            calls[1].get("extra_content").is_none(),
            "unsigned parallel call must stay unsigned"
        );
        let result_ids: Vec<_> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .map(|m| m["tool_call_id"].as_str().unwrap())
            .collect();
        assert_eq!(result_ids, ["call-a", "call-b"]);
    }
}

#[test]
fn gemini_signature_is_bound_to_its_model_and_old_history_stays_compatible() {
    let old = json!({"type":"tool_use","id":"call-a","name":"lookup","args":{}});
    let unsigned: ContentBlock = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(serde_json::to_value(&unsigned).unwrap(), old);

    let signed = ContentBlock::ToolUse {
        id: "call-a".into(),
        name: "lookup".into(),
        args: json!({}),
        thought_signature: Some(ThoughtSignature {
            signature: SIGNATURE.into(),
            model: MODEL.into(),
        }),
    };
    for (model, block, expected) in [
        (MODEL, signed.clone(), true),
        ("google/gemini-3.8-flash", signed.clone(), false),
        ("other-model", signed, false),
        (MODEL, unsigned, false),
    ] {
        let mapped = map_chat_request(
            request(vec![Message::new(Role::Assistant, vec![block])]),
            model,
            None,
        )
        .unwrap();
        let body = serde_json::to_value(mapped).unwrap();
        assert_eq!(
            body["messages"][0]["tool_calls"][0]
                .get("extra_content")
                .is_some(),
            expected
        );
    }
}

#[tokio::test]
async fn conflicting_gemini_signatures_fail_without_exposing_or_executing_them() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(sse(vec![
            json!({"choices":[{"delta":{"tool_calls":[{
                "index":0,"id":"call-a","function":{"name":"lookup","arguments":"{}"}
            }]}}]}),
            signed_delta("first-secret-signature"),
            signed_delta("second-secret-signature"),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        ]))
        .mount(&server)
        .await;
    let provider = OpenAiProvider::new("test-key", MODEL, server.uri(), None, None).unwrap();
    let events: Vec<_> = provider.stream(request(Vec::new())).collect().await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Ok(StreamEvent::ToolCallDelta { .. })))
    );
    let err = events
        .iter()
        .find_map(|e| e.as_ref().err())
        .unwrap()
        .to_string();
    assert!(err.contains("changed thought signature"));
    assert!(!err.contains("secret-signature"));
}
