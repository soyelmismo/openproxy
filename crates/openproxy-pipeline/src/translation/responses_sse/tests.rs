use super::*;
use bytes::Bytes;
use futures_util::StreamExt;

#[tokio::test]
async fn test_responses_stream_text_and_completion() {
    let incoming = vec![
        Bytes::from("data: {\"choices\":[{\"delta\":{\"content\":\"Hello \"}}]}\n\n"),
        Bytes::from("data: {\"choices\":[{\"delta\":{\"content\":\"world!\"}}]}\n\n"),
        Bytes::from(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\n",
        ),
        Bytes::from("data: [DONE]\n\n"),
    ];
    let stream = futures_util::stream::iter(incoming);
    let responses_stream =
        OpenAIToResponsesSseStream::new(stream, "resp_123".into(), "gpt-4o".into());
    let items: Vec<Bytes> = responses_stream.map(|r| r.unwrap()).collect().await;

    let combined = items
        .iter()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .collect::<String>();
    assert!(combined.contains("event: response.created"));
    assert!(combined.contains("event: response.output_item.added"));
    assert!(combined.contains("event: response.output_text.delta"));
    assert!(combined.contains("event: response.output_item.done"));
    assert!(combined.contains("event: response.completed"));
    assert!(combined.contains("\"status\":\"completed\""));
    assert!(combined.contains("\"input_tokens\":10"));
    assert!(combined.contains("\"output_tokens\":2"));
    assert!(combined.contains("data: [DONE]"));
}

#[tokio::test]
async fn test_responses_stream_tool_calls() {
    let incoming = vec![
        Bytes::from(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"function\":{\"name\":\"test_fn\",\"arguments\":\"{\\\"x\\\":\"}}]}}]}\n\n",
        ),
        Bytes::from(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]}}]}\n\n",
        ),
        Bytes::from("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n"),
        Bytes::from("data: [DONE]\n\n"),
    ];
    let stream = futures_util::stream::iter(incoming);
    let responses_stream =
        OpenAIToResponsesSseStream::new(stream, "resp_456".into(), "gpt-4o".into());
    let items: Vec<Bytes> = responses_stream.map(|r| r.unwrap()).collect().await;

    let combined = items
        .iter()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .collect::<String>();
    assert!(combined.contains("event: response.created"));
    assert!(combined.contains("event: response.output_item.added"));
    assert!(combined.contains("\"function_call\""));
    assert!(combined.contains("event: response.function_call_arguments.delta"));
    assert!(combined.contains("event: response.function_call_arguments.done"));
    assert!(combined.contains("event: response.output_item.done"));
    assert!(combined.contains("event: response.completed"));
    assert!(combined.contains("data: [DONE]"));
}

#[tokio::test]
async fn test_responses_stream_error_frame() {
    let incoming = vec![Bytes::from(
        "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"unauthorized\",\"message\":\"invalid key\"}}}\n\n",
    )];
    let stream = futures_util::stream::iter(incoming);
    let responses_stream =
        OpenAIToResponsesSseStream::new(stream, "resp_err".into(), "gpt-4o".into());
    let items: Vec<Bytes> = responses_stream.map(|r| r.unwrap()).collect().await;

    let combined = items
        .iter()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .collect::<String>();
    assert!(combined.contains("event: response.failed"));
    assert!(combined.contains("\"status\":\"failed\""));
}

#[tokio::test]
async fn test_responses_stream_reasoning_and_cached_tokens() {
    let incoming = vec![
        Bytes::from("data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"I think therefore I code.\"}}]}\n\n"),
        Bytes::from("data: {\"choices\":[{\"delta\":{\"content\":\"Result.\"}}]}\n\n"),
        Bytes::from(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":15,\"total_tokens\":115,\"input_tokens_details\":{\"cache_read_input_tokens\":80}}}\n\n",
        ),
        Bytes::from("data: [DONE]\n\n"),
    ];
    let stream = futures_util::stream::iter(incoming);
    let responses_stream =
        OpenAIToResponsesSseStream::new(stream, "resp_reasoning".into(), "gpt-5".into());
    let items: Vec<Bytes> = responses_stream.map(|r| r.unwrap()).collect().await;

    let combined = items
        .iter()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .collect::<String>();
    assert!(combined.contains("event: response.created"));
    assert!(combined.contains("event: response.output_item.added"));
    assert!(combined.contains("\"type\":\"reasoning\""));
    assert!(combined.contains("event: response.reasoning_text.delta"));
    assert!(combined.contains("I think therefore I code."));
    assert!(combined.contains("event: response.output_item.done"));
    assert!(combined.contains("event: response.output_text.delta"));
    assert!(combined.contains("Result."));
    assert!(combined.contains("event: response.completed"));
    assert!(combined.contains("\"cached_tokens\":80"));
    assert!(combined.contains("data: [DONE]"));
}

