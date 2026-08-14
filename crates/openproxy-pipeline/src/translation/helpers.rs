pub use openproxy_types::message::{
    extract_content_part_text as openai_content_part_to_text,
    extract_content_text as message_content_to_text,
};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_message_content_to_text() {
        assert_eq!(message_content_to_text(&Some(json!("hello"))), "hello");
        assert_eq!(
            message_content_to_text(&Some(
                json!([{"type": "text", "text": "hello "}, {"type": "text", "text": "world"}])
            )),
            "hello world"
        );
        assert_eq!(message_content_to_text(&Some(json!(null))), "");
        assert_eq!(message_content_to_text(&None), "");
        assert_eq!(message_content_to_text(&Some(json!(42))), "42");
    }

    #[test]
    fn test_openai_content_part_to_text() {
        assert_eq!(
            openai_content_part_to_text(&json!({"text": "hello"})),
            "hello"
        );
        assert_eq!(
            openai_content_part_to_text(&json!({"content": "world"})),
            "world"
        );
        assert_eq!(openai_content_part_to_text(&json!("string")), "string");
        assert_eq!(openai_content_part_to_text(&json!(null)), "");
        assert_eq!(openai_content_part_to_text(&json!(42)), "42");
    }
}
