//! Перевод схем OpenAPI 3.0 в самодостаточную JSON Schema для аргументов MCP-инструмента.
//!
//! `$ref` раскрываются на месте: MCP-клиенты не получают `components`, поэтому ссылки
//! в схеме инструмента были бы неразрешимы. Переносятся только ключевые слова JSON Schema.
//! Отбрасываются: `title` (у Apple это имя типа, агенту оно не нужно), `example`, `examples`,
//! `discriminator`, расширения `x-*`, свойства с `readOnly` и `deprecated`.
//! `nullable: true` (OpenAPI 3.0) превращается в тип-список с `null`: в запросах
//! обновления Apple `null` очищает значение.

use serde_json::{Map, Value, json};

/// Максимальная вложенность раскрытия `$ref`. Глубже схема заменяется разрешающим объектом.
const MAX_REF_DEPTH: usize = 8;

/// Ключи схемы, которые переносятся как есть (без рекурсии).
const PLAIN_KEYS: &[&str] = &[
    "type",
    "format",
    "enum",
    "const",
    "default",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "minLength",
    "maxLength",
    "pattern",
    "minItems",
    "maxItems",
    "uniqueItems",
];

/// Ключи, которые генератор понимает. Неизвестный ключ — ошибка сборки: новая конструкция
/// спецификации должна быть обработана осознанно, а не потеряна молча.
const KNOWN_KEYS: &[&str] = &[
    "$ref",
    "title",
    "description",
    "required",
    "items",
    "properties",
    "additionalProperties",
    "allOf",
    "anyOf",
    "oneOf",
    "nullable",
    "deprecated",
    "readOnly",
    "discriminator",
    "example",
    "examples",
];

pub struct Resolver<'a> {
    spec: &'a Value,
    /// Цепочка раскрываемых ссылок — защита от рекурсивных схем.
    stack: Vec<String>,
}

impl<'a> Resolver<'a> {
    pub fn new(spec: &'a Value) -> Self {
        Self {
            spec,
            stack: Vec::new(),
        }
    }

    /// Раскрывает схему в JSON Schema без `$ref`.
    pub fn resolve(&mut self, schema: &Value) -> Value {
        let Some(object) = schema.as_object() else {
            return schema.clone();
        };

        if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
            // Описание рядом с `$ref` относится к конкретному полю и точнее описания типа.
            let local_description = description_of(object);
            let mut resolved = self.resolve_ref(reference);
            if let (Some(text), Some(target)) = (local_description, resolved.as_object_mut()) {
                target.insert("description".into(), Value::String(text));
            }
            return resolved;
        }

        for key in object.keys() {
            assert!(
                PLAIN_KEYS.contains(&key.as_str())
                    || KNOWN_KEYS.contains(&key.as_str())
                    || key.starts_with("x-"),
                "неподдерживаемый ключ схемы {key}"
            );
        }

        let mut out = Map::new();
        for key in PLAIN_KEYS {
            if let Some(value) = object.get(*key) {
                out.insert((*key).into(), value.clone());
            }
        }
        if let Some(text) = description_of(object) {
            out.insert("description".into(), Value::String(text));
        }
        if let Some(items) = object.get("items") {
            out.insert("items".into(), self.resolve(items));
        } else if is_array_type(object.get("type")) {
            // Часть клиентов (API OpenAI, на котором работает Codex) отклоняет массив
            // без `items`: разрешаем любые элементы.
            out.insert("items".into(), json!({}));
        }
        let mut dropped = Vec::new();
        if let Some(properties) = object.get("properties").and_then(Value::as_object) {
            let mut resolved = Map::new();
            for (name, property) in properties {
                if is_hidden(self.spec, property) {
                    dropped.push(name.as_str());
                    continue;
                }
                resolved.insert(name.clone(), self.resolve(property));
            }
            out.insert("properties".into(), Value::Object(resolved));
        }
        // Пустой `required` допустим не во всех черновиках JSON Schema — не переносим.
        if let Some(required) = object.get("required").and_then(Value::as_array) {
            let required: Vec<Value> = required
                .iter()
                .filter(|name| !name.as_str().is_some_and(|name| dropped.contains(&name)))
                .cloned()
                .collect();
            if !required.is_empty() {
                out.insert("required".into(), Value::Array(required));
            }
        }
        if let Some(additional) = object.get("additionalProperties") {
            let value = if additional.is_object() {
                self.resolve(additional)
            } else {
                additional.clone()
            };
            out.insert("additionalProperties".into(), value);
        }
        for key in ["allOf", "anyOf", "oneOf"] {
            if let Some(variants) = object.get(key).and_then(Value::as_array) {
                let resolved = variants.iter().map(|v| self.resolve(v)).collect();
                out.insert(key.into(), Value::Array(resolved));
            }
        }
        if object.get("nullable").and_then(Value::as_bool) == Some(true) {
            make_nullable(&mut out);
        }
        Value::Object(out)
    }

    fn resolve_ref(&mut self, reference: &str) -> Value {
        let name = reference.rsplit('/').next().unwrap_or(reference).to_owned();
        if self.stack.iter().any(|seen| seen == reference) || self.stack.len() >= MAX_REF_DEPTH {
            // Рекурсия или слишком глубокая вложенность: структура не раскрывается,
            // агент может передать объект произвольной формы.
            return json!({
                "type": "object",
                "description": format!("`{name}` object (nested structure is not expanded)"),
            });
        }
        let target = lookup(self.spec, reference);
        self.stack.push(reference.to_owned());
        let resolved = self.resolve(target);
        self.stack.pop();
        resolved
    }
}

/// Свойство не нужно в аргументах: только для чтения или устарело.
fn is_hidden(spec: &Value, property: &Value) -> bool {
    let flagged = |value: &Value| {
        ["readOnly", "deprecated"]
            .iter()
            .any(|key| value.get(*key).and_then(Value::as_bool) == Some(true))
    };
    flagged(property) || flagged(deref(spec, property))
}

/// `nullable: true` → `type: [T, "null"]`, а в `enum` добавляется `null`.
fn make_nullable(out: &mut Map<String, Value>) {
    match out.get("type").cloned() {
        Some(Value::String(kind)) => {
            out.insert("type".into(), json!([kind, "null"]));
        }
        Some(Value::Array(mut kinds)) => {
            if !kinds.iter().any(|k| k == "null") {
                kinds.push(json!("null"));
            }
            out.insert("type".into(), Value::Array(kinds));
        }
        _ => {}
    }
    if let Some(Value::Array(values)) = out.get_mut("enum")
        && !values.iter().any(Value::is_null)
    {
        values.push(Value::Null);
    }
}

/// `type: array` или тип-список, в котором есть `array`.
fn is_array_type(kind: Option<&Value>) -> bool {
    match kind {
        Some(Value::String(kind)) => kind == "array",
        Some(Value::Array(kinds)) => kinds.iter().any(|k| k == "array"),
        _ => false,
    }
}

/// Объект, на который указывает локальная ссылка `#/...`.
pub fn lookup<'s>(spec: &'s Value, reference: &str) -> &'s Value {
    let pointer = reference
        .strip_prefix('#')
        .unwrap_or_else(|| panic!("неподдерживаемая ссылка {reference}: нужна локальная #/..."));
    spec.pointer(pointer)
        .unwrap_or_else(|| panic!("ссылка {reference} никуда не ведёт"))
}

/// Объект, на который указывает `$ref` значения, или само значение.
pub fn deref<'s>(spec: &'s Value, value: &'s Value) -> &'s Value {
    match value.get("$ref").and_then(Value::as_str) {
        Some(reference) => deref(spec, lookup(spec, reference)),
        None => value,
    }
}

/// Описание схемы (`description`). `title` у Apple — имя типа, в описание не попадает.
pub fn description_of(object: &Map<String, Value>) -> Option<String> {
    object
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
