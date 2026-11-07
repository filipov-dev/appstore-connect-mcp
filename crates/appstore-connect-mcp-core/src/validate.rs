//! Проверка аргументов составных инструментов по их JSON Schema.
//!
//! Составной инструмент получает аргументы уже проверенными: сервер отклоняет неизвестные
//! (`additionalProperties: false`), недостающие (`required`) и аргументы неверного типа до
//! вызова, с понятным агенту сообщением. Поддерживается подмножество JSON Schema, которого
//! хватает схемам инструментов: `type` (строка или список), `enum`, `const`, `properties`,
//! `required`, `additionalProperties: false`, `items`, `minimum`/`maximum`,
//! `minLength`/`maxLength`, `minItems`/`maxItems`. Остальные ключевые слова не проверяются:
//! их проверяет сам инструмент.
//!
//! `null` в аргументе верхнего уровня — то же, что непереданный аргумент (так некоторые
//! клиенты передают необязательные параметры), если схема свойства не разрешает `null` явно.

use rmcp::model::JsonObject;
use serde_json::Value;

use crate::error::ToolError;
use crate::request::kind;

/// Глубина вложенности, до которой проверяются значения.
const MAX_DEPTH: usize = 16;

/// Проверяет аргументы `args` по схеме `schema` (`type: object`). Аргументы `null`, которые
/// схема не разрешает, удаляются.
pub fn check_arguments(schema: &Value, args: &mut JsonObject) -> Result<(), ToolError> {
    // `null` верхнего уровня считается непереданным аргументом.
    let properties = schema.get("properties").and_then(Value::as_object);
    args.retain(|name, value| {
        !value.is_null()
            || properties
                .and_then(|p| p.get(name))
                .is_some_and(|property| allows_type(property, "null"))
    });
    check_object("", schema, args, 0)
}

fn check_object(
    path: &str,
    schema: &Value,
    object: &JsonObject,
    depth: usize,
) -> Result<(), ToolError> {
    let properties = schema.get("properties").and_then(Value::as_object);
    if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
        let unknown: Vec<String> = object
            .keys()
            .filter(|key| properties.is_none_or(|p| !p.contains_key(key.as_str())))
            .map(|key| format!("`{}`", join(path, key)))
            .collect();
        if !unknown.is_empty() {
            let allowed = properties
                .map(|p| {
                    p.keys()
                        .map(|key| format!("`{}`", join(path, key)))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|list| !list.is_empty())
                .unwrap_or_else(|| "none".to_owned());
            return Err(ToolError::new(format!(
                "unknown argument(s) {}; allowed arguments: {allowed}",
                unknown.join(", ")
            )));
        }
    }
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        let missing: Vec<String> = required
            .iter()
            .filter_map(Value::as_str)
            .filter(|name| !object.contains_key(*name))
            .map(|name| format!("`{}`", join(path, name)))
            .collect();
        if !missing.is_empty() {
            return Err(ToolError::new(format!(
                "missing required argument(s): {}",
                missing.join(", ")
            )));
        }
    }
    if let Some(properties) = properties {
        for (name, value) in object {
            if let Some(property) = properties.get(name) {
                check_value(&join(path, name), property, value, depth + 1)?;
            }
        }
    }
    Ok(())
}

fn check_value(path: &str, schema: &Value, value: &Value, depth: usize) -> Result<(), ToolError> {
    if depth > MAX_DEPTH || !schema.is_object() {
        return Ok(());
    }
    let invalid = |expected: String| {
        ToolError::new(format!(
            "argument `{path}` must be {expected}, got {}",
            kind(value)
        ))
    };
    if let Some(types) = schema_types(schema)
        && !types.iter().any(|t| matches_type(t, value))
    {
        let names: Vec<String> = types.iter().map(|t| article(t)).collect();
        return Err(invalid(names.join(" or ")));
    }
    if let Some(expected) = schema.get("const")
        && expected != value
    {
        return Err(invalid(format!("{expected}")));
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        let list: Vec<String> = allowed.iter().map(Value::to_string).collect();
        return Err(ToolError::new(format!(
            "argument `{path}`: {value} is not an allowed value; allowed values: {}",
            list.join(", ")
        )));
    }
    match value {
        Value::Number(n) => {
            let number = n.as_f64().unwrap_or_default();
            if let Some(min) = schema.get("minimum").and_then(Value::as_f64)
                && number < min
            {
                return Err(invalid(format!("at least {min}")));
            }
            if let Some(max) = schema.get("maximum").and_then(Value::as_f64)
                && number > max
            {
                return Err(invalid(format!("at most {max}")));
            }
        }
        Value::String(s) => {
            let len = s.chars().count() as u64;
            if let Some(min) = schema.get("minLength").and_then(Value::as_u64)
                && len < min
            {
                return Err(ToolError::new(format!(
                    "argument `{path}` must be at least {min} characters long"
                )));
            }
            if let Some(max) = schema.get("maxLength").and_then(Value::as_u64)
                && len > max
            {
                return Err(ToolError::new(format!(
                    "argument `{path}` must be at most {max} characters long"
                )));
            }
        }
        Value::Array(items) => {
            let len = items.len() as u64;
            if let Some(min) = schema.get("minItems").and_then(Value::as_u64)
                && len < min
            {
                return Err(ToolError::new(format!(
                    "argument `{path}` must have at least {min} item(s)"
                )));
            }
            if let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
                && len > max
            {
                return Err(ToolError::new(format!(
                    "argument `{path}` must have at most {max} item(s)"
                )));
            }
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    check_value(&format!("{path}[{index}]"), item_schema, item, depth + 1)?;
                }
            }
        }
        Value::Object(object) => check_object(path, schema, object, depth)?,
        Value::Null | Value::Bool(_) => {}
    }
    Ok(())
}

fn schema_types(schema: &Value) -> Option<Vec<&str>> {
    match schema.get("type")? {
        Value::String(t) => Some(vec![t.as_str()]),
        Value::Array(types) => Some(types.iter().filter_map(Value::as_str).collect()),
        _ => None,
    }
}

fn allows_type(schema: &Value, name: &str) -> bool {
    schema_types(schema).is_some_and(|types| types.contains(&name))
}

fn matches_type(name: &str, value: &Value) -> bool {
    match name {
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => match value {
            Value::Number(n) => {
                n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
            }
            _ => false,
        },
        // Неизвестный тип не проверяем.
        _ => true,
    }
}

fn article(name: &str) -> String {
    match name {
        "integer" | "object" | "array" => format!("an {name}"),
        "null" => "null".to_owned(),
        other => format!("a {other}"),
    }
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_owned()
    } else {
        format!("{path}.{name}")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["app_id"],
            "properties": {
                "app_id": {"type": "string", "minLength": 1},
                "platform": {"type": "string", "enum": ["IOS", "MAC_OS"]},
                "count": {"type": "integer", "minimum": 1, "maximum": 10},
                "dry_run": {"type": "boolean"},
                "locales": {"type": "array", "items": {"type": "string"}, "maxItems": 2},
                "options": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["mode"],
                    "properties": {"mode": {"type": "string"}}
                },
                "note": {"type": ["string", "null"]}
            }
        })
    }

    fn check(value: Value) -> Result<JsonObject, ToolError> {
        let Value::Object(mut args) = value else {
            panic!("не объект");
        };
        check_arguments(&schema(), &mut args).map(|()| args)
    }

    #[test]
    fn accepts_valid_arguments_and_drops_nulls() {
        let args = check(json!({
            "app_id": "1",
            "platform": "IOS",
            "count": 3.0,
            "dry_run": true,
            "locales": ["en-US"],
            "options": {"mode": "x"},
            "note": null,
            "dry_run_extra": null
        }));
        // Неизвестный аргумент `null` тоже отбрасывается как непереданный.
        let args = args.unwrap();
        assert!(args.contains_key("note"));
        assert!(!args.contains_key("dry_run_extra"));
    }

    #[test]
    fn rejects_bad_arguments() {
        let cases = [
            (json!({}), "missing required argument(s): `app_id`"),
            (
                json!({"app_id": null}),
                "missing required argument(s): `app_id`",
            ),
            (json!({"app_id": "1", "x": 1}), "unknown argument(s) `x`"),
            (
                json!({"app_id": 1}),
                "`app_id` must be a string, got a number",
            ),
            (json!({"app_id": ""}), "at least 1 characters"),
            (
                json!({"app_id": "1", "platform": "TV"}),
                "not an allowed value",
            ),
            (json!({"app_id": "1", "count": 1.5}), "must be an integer"),
            (json!({"app_id": "1", "count": 11}), "at most 10"),
            (
                json!({"app_id": "1", "dry_run": "yes"}),
                "must be a boolean",
            ),
            (
                json!({"app_id": "1", "locales": [1]}),
                "`locales[0]` must be a string",
            ),
            (
                json!({"app_id": "1", "locales": ["a", "b", "c"]}),
                "at most 2 item(s)",
            ),
            (json!({"app_id": "1", "options": {}}), "`options.mode`"),
            (
                json!({"app_id": "1", "options": {"mode": "x", "y": 1}}),
                "`options.y`",
            ),
            (json!({"app_id": "1", "note": 5}), "a string or null"),
        ];
        for (value, expected) in cases {
            let err = check(value.clone()).unwrap_err();
            assert!(err.message().contains(expected), "{value}: {err}");
        }
    }
}
