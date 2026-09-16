use super::normalizer::{ReasoningNormalizer, ToolCallAccumulator};
use crate::streaming::{StreamAction, StreamingChunkStage};

#[test]
fn test_reasoning_normalizer_stage_mutates_think_tags() {
    let mut normalizer = ReasoningNormalizer::new();
    let payload = r#"{"choices":[{"delta":{"content":"<think>reasoning</think>answer"}}]}"#;
    let action = normalizer.process_chunk(payload);
    let StreamAction::Mutate(mutated) = action else {
        panic!("expected StreamAction::Mutate, got {action:?}");
    };
    assert!(mutated.contains("\"reasoning_content\":\"reasoning\""));
    assert!(mutated.contains("\"content\":\"answer\""));
}

#[test]
fn test_reasoning_normalizer_stage_passthrough_clean_chunk() {
    let mut normalizer = ReasoningNormalizer::new();
    let payload = r#"{"choices":[{"delta":{"content":"clean chunk"}}]}"#;
    let action = normalizer.process_chunk(payload);
    assert_eq!(action, StreamAction::Passthrough);
}

#[test]
fn test_tool_call_accumulator_handles_fragments() {
    let mut acc = ToolCallAccumulator::new();
    let f1 = acc.process(0, "{\"location\":");
    assert_eq!(f1, "{\"location\":");
    let f2 = acc.process(0, " \"Paris\"}");
    assert_eq!(f2, " \"Paris\"}");
}
