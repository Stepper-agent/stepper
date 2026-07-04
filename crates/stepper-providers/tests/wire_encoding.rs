//! Pure request-encoding tests: assert each dialect projects the neutral
//! `ChatRequest` into the right wire body (system placement differs per dialect,
//! tool calls/results round-trip into the dialect's envelope, stop sequences and
//! thinking/effort controls map per dialect, and thinking history replays
//! without ever serializing an empty assistant content array).

use serde_json::{json, Value};
use stepper_provider::{
    ChatRequest, ContentBlock, Message, Role, ThinkingConfig, ToolContent, ToolSpec,
};
use stepper_providers::wire;

fn convo() -> ChatRequest {
    let tool = ToolSpec {
        name: "get_weather".into(),
        description: "look up weather".into(),
        input_schema: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
        read_only: true,
        parallel_safe: true,
    };
    ChatRequest::new("m")
        .with_system("you are helpful")
        .with_tools(vec![tool])
        .with_messages(vec![
            Message::user("weather in seoul?"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "get_weather".into(),
                    input: json!({"city": "seoul"}),
                }],
            },
            Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_call_id: "call_1".into(),
                    content: vec![ToolContent::text("18C")],
                    is_error: false,
                }],
            },
        ])
}

#[test]
fn openai_puts_system_first_and_serializes_tool_calls() {
    let body = wire::openai::build_request_body(&convo(), "m", true);
    let messages = body["messages"].as_array().unwrap();

    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "you are helpful");
    assert_eq!(messages[1]["role"], "user");

    let assistant = &messages[2];
    assert_eq!(assistant["role"], "assistant");
    let tc = &assistant["tool_calls"][0];
    assert_eq!(tc["id"], "call_1");
    assert_eq!(tc["function"]["name"], "get_weather");
    // OpenAI requires arguments as a JSON *string*, not an object.
    assert_eq!(tc["function"]["arguments"], "{\"city\":\"seoul\"}");

    let tool_msg = &messages[3];
    assert_eq!(tool_msg["role"], "tool");
    assert_eq!(tool_msg["tool_call_id"], "call_1");
    assert_eq!(tool_msg["content"], "18C");

    assert_eq!(body["stream_options"]["include_usage"], json!(true));
    assert_eq!(body["tools"][0]["function"]["name"], "get_weather");
}

#[test]
fn anthropic_hoists_system_and_uses_tool_result_user_block() {
    let body = wire::anthropic::build_request_body(&convo(), "m", true);

    // system is top-level, never a message.
    assert_eq!(body["system"], "you are helpful");
    assert_eq!(body["max_tokens"], json!(8192));

    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "user");

    let assistant = &messages[1];
    assert_eq!(assistant["content"][0]["type"], "tool_use");
    assert_eq!(assistant["content"][0]["input"], json!({"city": "seoul"}));

    // tool result becomes a user message carrying a tool_result block.
    let tool_turn = &messages[2];
    assert_eq!(tool_turn["role"], "user");
    assert_eq!(tool_turn["content"][0]["type"], "tool_result");
    assert_eq!(tool_turn["content"][0]["tool_use_id"], "call_1");

    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    assert!(body.get("messages").is_some());
    assert!(messages_have_no_system(&body));
}

#[test]
fn anthropic_marks_cache_breakpoints_when_caching_requested() {
    // Default (cache off): system is a plain string, no cache_control anywhere.
    let plain = wire::anthropic::build_request_body(&convo(), "m", true);
    assert_eq!(plain["system"], "you are helpful");
    assert!(plain["tools"][0].get("cache_control").is_none());
    let plain_msgs = plain["messages"].as_array().unwrap();
    assert!(plain_msgs
        .last()
        .unwrap()["content"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .get("cache_control")
        .is_none());

    // cache on: system becomes a content-block array with an ephemeral breakpoint,
    // the last tool carries one too (caching system + tools as the prefix), and a
    // second breakpoint lands on the last block of the last message (the tail).
    let mut req = convo();
    req.cache = true;
    let body = wire::anthropic::build_request_body(&req, "m", true);
    assert_eq!(body["system"][0]["type"], "text");
    assert_eq!(body["system"][0]["text"], "you are helpful");
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(
        tools.last().unwrap()["cache_control"]["type"],
        "ephemeral",
        "the last tool carries the prefix cache breakpoint"
    );
    let messages = body["messages"].as_array().unwrap();
    let last_block = messages
        .last()
        .unwrap()["content"]
        .as_array()
        .unwrap()
        .last()
        .unwrap();
    assert_eq!(
        last_block["cache_control"]["type"], "ephemeral",
        "the conversation tail carries the second cache breakpoint"
    );
}

fn messages_have_no_system(body: &Value) -> bool {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .all(|m| m["role"] != "system")
}

#[test]
fn responses_uses_instructions_and_function_items() {
    let body = wire::responses::build_request_body(&convo(), "m", true, false);

    assert_eq!(body["instructions"], "you are helpful");
    assert_eq!(body["store"], json!(false));

    let input = body["input"].as_array().unwrap();
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["role"], "user");

    let call = &input[1];
    assert_eq!(call["type"], "function_call");
    assert_eq!(call["call_id"], "call_1");
    assert_eq!(call["arguments"], "{\"city\":\"seoul\"}");

    let output = &input[2];
    assert_eq!(output["type"], "function_call_output");
    assert_eq!(output["call_id"], "call_1");
    assert_eq!(output["output"], "18C");

    // store:true when not codex.
    let openai_body = wire::responses::build_request_body(&convo(), "m", true, true);
    assert_eq!(openai_body["store"], json!(true));
}
#[test]
fn responses_assistant_text_precedes_its_function_call() {
    // An assistant turn that produced text and then a tool call must encode the
    // assistant message BEFORE the function_call item — the order the model
    // emitted them. (Regression for the mixed-content ordering bug.)
    let req = ChatRequest::new("m").with_messages(vec![
        Message::user("what is the weather?"),
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text("Let me check that for you.".into()),
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "get_weather".into(),
                    input: json!({"city": "Seoul"}),
                },
            ],
        },
    ]);

    let body = wire::responses::build_request_body(&req, "m", true, false);
    let input = body["input"].as_array().unwrap();

    let message_idx = input.iter().position(|it| {
        it["type"] == "message" && it["role"] == "assistant"
    });
    let call_idx = input.iter().position(|it| it["type"] == "function_call");

    let message_idx = message_idx.expect("assistant message item present");
    let call_idx = call_idx.expect("function_call item present");
    assert!(
        message_idx < call_idx,
        "assistant message must precede its function_call (msg={message_idx}, call={call_idx})"
    );
}

#[test]
fn responses_maps_stop_sequences_when_non_empty_and_omits_when_empty() {
    // Regression IMP-16/GAP-26: Responses/Codex silently discarded req.stop
    // while OpenAI/Anthropic honored it.
    let mut req = convo();
    req.stop = vec!["END".into(), "STOP".into()];
    let body = wire::responses::build_request_body(&req, "m", true, false);
    assert_eq!(body["stop"], json!(["END", "STOP"]));

    let empty = wire::responses::build_request_body(&convo(), "m", true, false);
    assert!(
        empty.get("stop").is_none(),
        "an empty stop list stays off the wire"
    );
}

#[test]
fn anthropic_emits_top_level_thinking_config_only_when_requested() {
    let plain = wire::anthropic::build_request_body(&convo(), "m", true);
    assert!(plain.get("thinking").is_none());

    let mut req = convo();
    req.thinking = Some(ThinkingConfig {
        budget_tokens: 4096,
    });
    let body = wire::anthropic::build_request_body(&req, "m", true);
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["budget_tokens"], json!(4096));
}

#[test]
fn anthropic_drops_sampling_params_when_thinking_is_active_on_a_legacy_model() {
    // A legacy model (`haiku`) with a set temperature AND a thinking budget must
    // not send `temperature`/`top_p` alongside thinking — Anthropic 400s.
    let mut req = convo();
    req.temperature = Some(0.0);
    req.top_p = Some(0.5);
    let no_thinking = wire::anthropic::build_request_body(&req, "claude-haiku-4-5", true);
    assert_eq!(no_thinking["temperature"], json!(0.0), "sampling kept without thinking");

    req.thinking = Some(ThinkingConfig { budget_tokens: 4096 });
    let with_thinking = wire::anthropic::build_request_body(&req, "claude-haiku-4-5", true);
    assert_eq!(with_thinking["thinking"]["type"], "enabled");
    assert!(with_thinking.get("temperature").is_none(), "temperature dropped with thinking");
    assert!(with_thinking.get("top_p").is_none(), "top_p dropped with thinking");
}

#[test]
fn openai_reasoning_models_drop_sampling_params() {
    let mut req = convo();
    req.temperature = Some(0.5);
    req.top_p = Some(0.25);

    // Non-reasoning model keeps them.
    let chat = wire::openai::build_request_body(&req, "gpt-4o", true);
    assert!(chat.get("temperature").is_some(), "gpt-4o keeps temperature");
    let resp = wire::responses::build_request_body(&req, "gpt-4o", true, false);
    assert!(resp.get("temperature").is_some(), "gpt-4o responses keeps temperature");

    // Reasoning model by name (no effort set) drops them.
    for m in ["o3-mini", "gpt-5", "provider/o1"] {
        let chat = wire::openai::build_request_body(&req, m, true);
        assert!(chat.get("temperature").is_none(), "{m}: chat drops temperature");
        assert!(chat.get("top_p").is_none(), "{m}: chat drops top_p");
        let resp = wire::responses::build_request_body(&req, m, true, false);
        assert!(resp.get("temperature").is_none(), "{m}: responses drops temperature");
    }

    // A configured effort marks any model as reasoning.
    req.reasoning_effort = Some("high".into());
    let chat = wire::openai::build_request_body(&req, "gpt-4o", true);
    assert!(chat.get("temperature").is_none(), "effort set → drop temperature");
}

#[test]
fn openai_and_responses_map_reasoning_effort_per_dialect() {
    let mut req = convo();
    req.reasoning_effort = Some("high".into());

    let openai = wire::openai::build_request_body(&req, "m", true);
    assert_eq!(openai["reasoning_effort"], "high");

    let responses = wire::responses::build_request_body(&req, "m", true, false);
    assert_eq!(responses["reasoning"]["effort"], "high");

    // Anthropic uses the thinking budget, never reasoning_effort.
    let anthropic = wire::anthropic::build_request_body(&req, "m", true);
    assert!(anthropic.get("reasoning_effort").is_none());
    assert!(anthropic.get("reasoning").is_none());

    let plain_openai = wire::openai::build_request_body(&convo(), "m", true);
    assert!(plain_openai.get("reasoning_effort").is_none());
    let plain_responses = wire::responses::build_request_body(&convo(), "m", true, false);
    assert!(plain_responses.get("reasoning").is_none());
}

fn thinking_history_request() -> ChatRequest {
    ChatRequest::new("m").with_messages(vec![
        Message::user("question"),
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    text: "signed reasoning".into(),
                    signature: Some("sig-1".into()),
                },
                ContentBlock::Text("the answer".into()),
            ],
        },
        Message::user("follow-up"),
    ])
}

fn thinking_only_unsigned_request() -> ChatRequest {
    ChatRequest::new("m").with_messages(vec![
        Message::user("question"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking {
                text: "unsigned-only turn".into(),
                signature: None,
            }],
        },
        Message::user("follow-up"),
    ])
}

#[test]
fn anthropic_replays_signed_thinking_blocks_and_drops_unsigned_ones() {
    let body = wire::anthropic::build_request_body(&thinking_history_request(), "m", true);
    let messages = body["messages"].as_array().unwrap();
    let assistant = &messages[1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"][0]["type"], "thinking");
    assert_eq!(assistant["content"][0]["thinking"], "signed reasoning");
    assert_eq!(assistant["content"][0]["signature"], "sig-1");
    assert_eq!(assistant["content"][1]["type"], "text");

    let mixed_unsigned = ChatRequest::new("m").with_messages(vec![Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Thinking {
                text: "unsigned".into(),
                signature: None,
            },
            ContentBlock::Text("kept".into()),
        ],
    }]);
    let body = wire::anthropic::build_request_body(&mixed_unsigned, "m", true);
    let blocks = body["messages"][0]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1, "the unsigned thinking block is dropped");
    assert_eq!(blocks[0]["type"], "text");
}

#[test]
fn anthropic_drops_the_whole_assistant_message_when_only_unsigned_thinking_remains() {
    // Regression IMP-25: an assistant message must never serialize with an
    // empty content array — the API rejects it.
    let body = wire::anthropic::build_request_body(&thinking_only_unsigned_request(), "m", true);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(
        messages.len(),
        2,
        "user / user — the empty assistant turn vanished"
    );
    assert!(
        messages.iter().all(|m| m["role"] == "user"),
        "no assistant message with empty content survives: {messages:?}"
    );
    assert!(
        messages
            .iter()
            .all(|m| !m["content"].as_array().unwrap().is_empty()),
        "no message carries an empty content array"
    );
}

#[test]
fn openai_drops_a_thinking_only_assistant_turn_instead_of_sending_null_content() {
    let body = wire::openai::build_request_body(&thinking_only_unsigned_request(), "m", true);
    let messages = body["messages"].as_array().unwrap();
    assert!(
        messages.iter().all(|m| m["role"] != "assistant"),
        "a thinking-only turn produces no assistant message: {messages:?}"
    );
    // A turn with text or tool calls still encodes.
    let body = wire::openai::build_request_body(&thinking_history_request(), "m", true);
    let assistant = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant with text survives");
    assert_eq!(assistant["content"], "the answer");
}

#[test]
fn responses_drops_a_thinking_only_assistant_turn_entirely() {
    let body = wire::responses::build_request_body(&thinking_only_unsigned_request(), "m", true, false);
    let input = body["input"].as_array().unwrap();
    assert!(
        input
            .iter()
            .all(|it| !(it["type"] == "message" && it["role"] == "assistant")),
        "a thinking-only turn produces no assistant item: {input:?}"
    );
    assert_eq!(input.len(), 2, "only the two user messages remain");
}

#[test]
fn image_attachments_encode_per_dialect() {
    let req = ChatRequest::new("m").with_messages(vec![Message {
        role: Role::User,
        content: vec![
            ContentBlock::Text("describe this".into()),
            ContentBlock::Image { media_type: "image/png".into(), data: "AAAB".into() },
        ],
    }]);

    // Anthropic: a base64 `source` image block alongside the text block.
    let a = wire::anthropic::build_request_body(&req, "m", true);
    let ac = &a["messages"][0]["content"];
    assert_eq!(ac[0]["type"], "text");
    assert_eq!(ac[1]["type"], "image");
    assert_eq!(ac[1]["source"]["type"], "base64");
    assert_eq!(ac[1]["source"]["media_type"], "image/png");
    assert_eq!(ac[1]["source"]["data"], "AAAB");

    // OpenAI: a content array with an `image_url` data: URL.
    let o = wire::openai::build_request_body(&req, "m", true);
    let oc = &o["messages"][0]["content"];
    assert_eq!(oc[0]["type"], "text");
    assert_eq!(oc[1]["type"], "image_url");
    assert_eq!(oc[1]["image_url"]["url"], "data:image/png;base64,AAAB");

    // Responses: an `input_image` with a data: URL.
    let r = wire::responses::build_request_body(&req, "m", true, false);
    let rc = &r["input"][0]["content"];
    assert_eq!(rc[0]["type"], "input_text");
    assert_eq!(rc[1]["type"], "input_image");
    assert_eq!(rc[1]["image_url"], "data:image/png;base64,AAAB");

    // A text-only user message keeps OpenAI's plain-string content (back-compat).
    let plain = wire::openai::build_request_body(
        &ChatRequest::new("m").with_messages(vec![Message::user("hi")]),
        "m",
        true,
    );
    assert_eq!(plain["messages"][0]["content"], "hi");
}

#[test]
fn anthropic_date_suffixed_legacy_ids_stay_on_the_legacy_surface() {
    // `claude-opus-4-20250514` is the canonical id for Opus 4.0 (alias
    // `claude-opus-4-0`) — the date must NOT be parsed as minor 20250514, which
    // would mis-classify it as adaptive-capable and 400 (output_config.effort is
    // unsupported there) while dropping temperature/top_p.
    let mut req = ChatRequest::new("claude-opus-4-20250514");
    req.reasoning_effort = Some("high".into());
    req.thinking = Some(ThinkingConfig { budget_tokens: 8_192 });
    req.temperature = Some(0.5);
    let body = wire::anthropic::build_request_body(&req, "claude-opus-4-20250514", true);
    assert_eq!(body["thinking"]["type"], "enabled", "Opus 4.0 keeps legacy extended thinking");
    assert_eq!(body["thinking"]["budget_tokens"], json!(8_192));
    assert!(body.get("output_config").is_none(), "no output_config.effort on a pre-4.6 model");
    assert!(
        body.get("temperature").is_none(),
        "sampling params 400 alongside active thinking even on a legacy model — suppressed"
    );

    // A date-suffixed MODERN id still classifies by its real minor (4.6), adaptive.
    let mut modern = ChatRequest::new("claude-opus-4-6-20251101");
    modern.reasoning_effort = Some("high".into());
    let body = wire::anthropic::build_request_body(&modern, "claude-opus-4-6-20251101", true);
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["output_config"]["effort"], "high");
}

#[test]
fn anthropic_modern_models_use_adaptive_thinking_and_output_config_effort() {
    // Modern Claude (Opus >= 4.6 / Sonnet >= 4.6 / Fable·Mythos 5) drives reasoning
    // with adaptive thinking + output_config.effort; budget_tokens and sampling
    // params are removed (they 400 on Opus 4.7+).
    let mut req = ChatRequest::new("claude-opus-4-8");
    req.reasoning_effort = Some("max".into());
    req.thinking = Some(ThinkingConfig { budget_tokens: 16_384 });
    req.temperature = Some(0.7);
    req.top_p = Some(0.9);
    let body = wire::anthropic::build_request_body(&req, "claude-opus-4-8", true);
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert!(body["thinking"].get("budget_tokens").is_none(), "budget_tokens gone on modern Claude");
    assert_eq!(body["output_config"]["effort"], "max");
    assert!(body.get("temperature").is_none(), "temperature 400s on Opus 4.7+ — suppressed");
    assert!(body.get("top_p").is_none(), "top_p 400s on Opus 4.7+ — suppressed");
}

#[test]
fn anthropic_xhigh_clamps_to_high_where_unsupported() {
    let mut req = ChatRequest::new("x");
    req.reasoning_effort = Some("xhigh".into());
    let opus = wire::anthropic::build_request_body(&req, "claude-opus-4-8", true);
    assert_eq!(opus["output_config"]["effort"], "xhigh");
    let sonnet = wire::anthropic::build_request_body(&req, "claude-sonnet-4-6", true);
    assert_eq!(sonnet["output_config"]["effort"], "high", "xhigh clamps to high on Sonnet 4.6");
}

#[test]
fn anthropic_legacy_models_keep_budget_tokens_thinking() {
    let mut req = ChatRequest::new("claude-sonnet-4-5");
    req.thinking = Some(ThinkingConfig { budget_tokens: 8_192 });
    req.temperature = Some(0.3);
    let body = wire::anthropic::build_request_body(&req, "claude-sonnet-4-5", true);
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["budget_tokens"], json!(8_192));
    assert!(body.get("output_config").is_none());
    assert!(
        body.get("temperature").is_none(),
        "temperature 400s alongside legacy thinking — suppressed"
    );

    // Without thinking, the same legacy model keeps its sampling params.
    let mut plain = ChatRequest::new("claude-sonnet-4-5");
    plain.temperature = Some(0.3);
    let body = wire::anthropic::build_request_body(&plain, "claude-sonnet-4-5", true);
    assert!(body.get("temperature").is_some(), "legacy keeps sampling params without thinking");
}

#[test]
fn openai_clamps_anthropic_only_effort_levels() {
    let mut req = ChatRequest::new("gpt");
    req.reasoning_effort = Some("xhigh".into());
    let chat = wire::openai::build_request_body(&req, "gpt", true);
    assert_eq!(chat["reasoning_effort"], "high", "xhigh clamps to high for OpenAI");
    req.reasoning_effort = Some("max".into());
    let resp = wire::responses::build_request_body(&req, "gpt", true, false);
    assert_eq!(resp["reasoning"]["effort"], "high", "max clamps to high for Responses");
}
