use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;

pub(crate) fn json_from_text_output<T: DeserializeOwned>(output: &str) -> Result<T> {
    if let Ok(value) = serde_json::from_str::<T>(output) {
        return Ok(value);
    }

    if let Ok(value) = serde_json::from_str::<Value>(output) {
        if let Some(parsed) = structured_output_from_value(&value)? {
            return Ok(parsed);
        }

        match value {
            Value::Object(_) => {
                return serde_json::from_value(value)
                    .context("Failed to parse extractor JSON object as structured output");
            }
            Value::Array(_) => {
                bail!("Extractor JSON output did not contain a structured plan payload");
            }
            _ => {}
        }
    }

    let candidates = json_object_slices(output);
    if candidates.is_empty() {
        bail!("Extractor output did not contain a JSON object");
    }

    let mut last_error = None;
    for slice in candidates.iter().rev() {
        match serde_json::from_str::<T>(slice) {
            Ok(value) => return Ok(value),
            Err(err) => last_error = Some(err),
        }

        if let Ok(value) = serde_json::from_str::<Value>(slice) {
            if let Some(parsed) = structured_output_from_value(&value)? {
                return Ok(parsed);
            }
        }
    }

    let err = last_error
        .map(|err| err.to_string())
        .unwrap_or_else(|| "no parseable JSON object candidate".to_string());
    bail!("Failed to parse extracted JSON payload from extractor output: {err}")
}

fn structured_output_from_value<T: DeserializeOwned>(value: &Value) -> Result<Option<T>> {
    match value {
        Value::Object(object) => {
            if let Some(structured) = object.get("structured_output") {
                return serde_json::from_value(structured.clone())
                    .map(Some)
                    .context("Failed to parse extractor structured_output payload");
            }

            if let Some(input) = structured_tool_input(value) {
                return serde_json::from_value(input.clone())
                    .map(Some)
                    .context("Failed to parse extractor StructuredOutput tool payload");
            }

            if let Some(result) = object.get("result") {
                return match result {
                    Value::String(text) => {
                        json_from_text_output(text).map(Some).with_context(|| {
                            "Failed to parse extractor result text as structured output"
                        })
                    }
                    Value::Object(_) => serde_json::from_value(result.clone())
                        .map(Some)
                        .context("Failed to parse extractor result object as structured output"),
                    _ => Ok(None),
                };
            }

            Ok(None)
        }
        Value::Array(events) => {
            for event in events.iter().rev() {
                if let Some(parsed) = structured_output_from_value(event)? {
                    return Ok(Some(parsed));
                }
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn structured_tool_input(value: &Value) -> Option<&Value> {
    let object_is_structured_tool = value.get("type").and_then(|value| value.as_str())
        == Some("tool_use")
        && value.get("name").and_then(|value| value.as_str()) == Some("StructuredOutput");
    if object_is_structured_tool {
        return value.get("input");
    }

    let contents = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_array())?;

    contents.iter().rev().find_map(|content| {
        let is_structured_tool = content.get("type").and_then(|value| value.as_str())
            == Some("tool_use")
            && content.get("name").and_then(|value| value.as_str()) == Some("StructuredOutput");
        is_structured_tool.then(|| content.get("input")).flatten()
    })
}

fn json_object_slices(output: &str) -> Vec<&str> {
    let mut slices = Vec::new();
    let mut start = None;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (index, ch) in output.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' if depth > 0 => in_string = true,
            '{' => {
                if depth == 0 {
                    start = Some(index);
                }
                depth += 1;
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = start.take() {
                        slices.push(&output[start..index + ch.len_utf8()]);
                    }
                }
            }
            _ => {}
        }
    }

    slices
}
