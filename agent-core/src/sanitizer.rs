/// Response sanitizers for different LLM models.
/// Each sanitizer attempts to clean up and repair model output before JSON parsing.

/// Try to repair truncated or malformed JSON from LLM responses.
/// Returns the cleaned text, or the original if no repair was possible.
pub fn sanitize_response(model: &str, raw: &str) -> String {
    let mut text = raw.trim().to_string();

    // Step 1: Strip markdown fences
    text = strip_markdown_fences(&text);

    // Step 2: Strip XML/native tool call wrappers
    if text.starts_with("<") {
        if let Some(json) = extract_json_from_xml(&text) {
            text = json;
        }
    }

    // Step 3: Model-specific sanitization
    if model.contains("qwen") {
        text = sanitize_qwen(&text);
    } else if model.contains("minimax") {
        text = sanitize_minimax(&text);
    }

    // Step 4: Generic truncation repair (works for all models)
    text = repair_truncated_json(&text);

    text
}

/// Strip ``` and ```json fences
fn strip_markdown_fences(text: &str) -> String {
    let mut s = text.trim();
    if s.starts_with("```json") {
        s = s.strip_prefix("```json").unwrap().trim();
    } else if s.starts_with("```") {
        s = s.strip_prefix("```").unwrap().trim();
    }
    if s.ends_with("```") {
        s = s.strip_suffix("```").unwrap().trim();
    }
    s.to_string()
}

/// Try to extract JSON from XML-wrapped tool calls like <minimax:tool_call> or <FunctionCall>
fn extract_json_from_xml(text: &str) -> Option<String> {
    // Look for JSON object embedded in the XML
    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            if end > start {
                return Some(text[start..=end].to_string());
            }
        }
    }
    None
}

/// Qwen-specific: sometimes emits trailing text after JSON, or wraps in commentary
fn sanitize_qwen(text: &str) -> String {
    // Qwen sometimes prefixes with text before the JSON
    if !text.starts_with('{') {
        if let Some(start) = text.find('{') {
            return text[start..].to_string();
        }
    }
    text.to_string()
}

/// Minimax-specific: may emit <FunctionCall> XML
fn sanitize_minimax(text: &str) -> String {
    if text.contains("<FunctionCall>") || text.contains("<minimax:tool_call>") {
        if let Some(json) = extract_json_from_xml(text) {
            return json;
        }
    }
    text.to_string()
}

/// Attempt to repair truncated JSON by closing open strings, arrays, and objects.
/// This is the key fix for models that get their output cut short.
pub fn repair_truncated_json(text: &str) -> String {
    let trimmed = text.trim();

    // If it already parses, return as-is
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return trimmed.to_string();
    }

    // Only attempt repair if it looks like it started as JSON
    if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
        return trimmed.to_string();
    }

    let mut repaired = String::from(trimmed);
    let mut open_braces = 0i32;
    let mut open_brackets = 0i32;
    let mut in_string = false;
    let mut last_char = ' ';

    for ch in trimmed.chars() {
        if in_string {
            if ch == '"' && last_char != '\\' {
                in_string = false;
            }
        } else {
            match ch {
                '"' => in_string = true,
                '{' => open_braces += 1,
                '}' => open_braces -= 1,
                '[' => open_brackets += 1,
                ']' => open_brackets -= 1,
                _ => {}
            }
        }
        last_char = ch;
    }

    // Close an open string
    if in_string {
        repaired.push('"');
    }

    // If the last meaningful token is a colon or comma, we need a value
    let last_meaningful = repaired.trim_end().chars().last().unwrap_or(' ');
    if last_meaningful == ':' || last_meaningful == ',' {
        repaired.push_str("null");
    }

    // Close open brackets and braces
    for _ in 0..open_brackets {
        repaired.push(']');
    }
    for _ in 0..open_braces {
        repaired.push('}');
    }

    // Verify the repair worked
    if serde_json::from_str::<serde_json::Value>(&repaired).is_ok() {
        repaired
    } else {
        // Return original if repair failed
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_markdown_fences() {
        let input = "```json\n{\"key\": \"value\"}\n```";
        assert_eq!(strip_markdown_fences(input), "{\"key\": \"value\"}");
    }

    #[test]
    fn test_repair_truncated_string() {
        let input = r#"{"reasoning": "test", "action": {"type": "ReturnResult", "summary": "Today is Saturday, February "#;
        let repaired = repair_truncated_json(input);
        assert!(serde_json::from_str::<serde_json::Value>(&repaired).is_ok());
    }

    #[test]
    fn test_repair_truncated_at_colon() {
        let input = r#"{"reasoning": "test", "action": {"type": "ExecuteTool", "tool_name": "bash", "parameters":"#;
        let repaired = repair_truncated_json(input);
        assert!(serde_json::from_str::<serde_json::Value>(&repaired).is_ok());
    }

    #[test]
    fn test_valid_json_unchanged() {
        let input = r#"{"reasoning": "ok", "action": {"type": "ReturnResult", "summary": "done"}}"#;
        assert_eq!(repair_truncated_json(input), input);
    }

    #[test]
    fn test_extract_json_from_xml() {
        let input = r#"<FunctionCall>
tool_name: bash
parameters: {"cmd": "date"}
</FunctionCall>"#;
        let extracted = extract_json_from_xml(input);
        assert_eq!(extracted, Some(r#"{"cmd": "date"}"#.to_string()));
    }

    #[test]
    fn test_sanitize_full_pipeline() {
        let input = "```json\n{\"reasoning\": \"thinking\", \"action\": {\"type\": \"ReturnResult\", \"summary\": \"hello\"}}\n```";
        let result = sanitize_response("qwen/qwen-coder", input);
        assert!(serde_json::from_str::<serde_json::Value>(&result).is_ok());
    }
}
