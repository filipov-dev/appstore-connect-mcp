//! Аргумент `flatten`: связанные объекты из `included` на место ссылок.
//!
//! Ответ JSON:API с `include` хранит связанные ресурсы отдельно, в `included`, а в
//! `relationships` оставляет только ссылки `{type, id}`: агенту приходится сопоставлять их
//! вручную. С `flatten: true` каждый ресурс превращается в плоский объект
//! `{id, type, ...attributes, <связь>: <объект или список>}`:
//!
//! - атрибуты поднимаются на уровень ресурса (JSON:API запрещает совпадение имён атрибутов,
//!   связей, `id` и `type`, поэтому конфликтов нет);
//! - связь с данными заменяется связанным объектом из `included` (тоже плоским), если он там
//!   есть, иначе остаётся ссылкой `{type, id}`; связь без данных (только `links`) убирается;
//! - вложенность ограничена [`MAX_DEPTH`], а цикл (ресурс ссылается на уже раскрытого предка)
//!   оставляет ссылку;
//! - `links` ресурсов и верхнего уровня и сам `included` убираются; `meta` остаётся.
//!
//! Постраничная выдача при этом не теряется: сервер берёт `links.next` и `meta.paging` из
//! исходного ответа и показывает их в обёртке результата.

use std::collections::HashMap;

use serde_json::{Map, Value};

/// Глубина раскрытия связей: ресурс → связанный → связанный со связанным.
pub const MAX_DEPTH: usize = 3;

/// Преобразует тело ответа JSON:API (см. описание модуля). Тело без `data` не меняется.
pub fn flatten(body: &mut Value) {
    let Value::Object(top) = body else {
        return;
    };
    let Some(data) = top.remove("data") else {
        return;
    };
    let included = match top.remove("included") {
        Some(Value::Array(items)) => items,
        _ => Vec::new(),
    };
    let mut index: HashMap<(String, String), &Value> = HashMap::new();
    for item in &included {
        if let Some(key) = resource_key(item) {
            index.insert(key, item);
        }
    }
    let mut stack = Vec::new();
    let flat = match &data {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| flatten_resource(item, &index, &mut stack, 0))
                .collect(),
        ),
        Value::Object(_) => flatten_resource(&data, &index, &mut stack, 0),
        other => other.clone(),
    };
    top.remove("links");
    top.insert("data".to_owned(), flat);
}

fn resource_key(value: &Value) -> Option<(String, String)> {
    let kind = value.get("type")?.as_str()?;
    let id = value.get("id")?.as_str()?;
    Some((kind.to_owned(), id.to_owned()))
}

fn flatten_resource(
    resource: &Value,
    index: &HashMap<(String, String), &Value>,
    stack: &mut Vec<(String, String)>,
    depth: usize,
) -> Value {
    let Value::Object(object) = resource else {
        return resource.clone();
    };
    let mut out = Map::new();
    for key in ["id", "type"] {
        if let Some(value) = object.get(key) {
            out.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(Value::Object(attributes)) = object.get("attributes") {
        for (name, value) in attributes {
            out.insert(name.clone(), value.clone());
        }
    }
    let key = resource_key(resource);
    if let Some(key) = &key {
        stack.push(key.clone());
    }
    if let Some(Value::Object(relationships)) = object.get("relationships") {
        for (name, relationship) in relationships {
            let Some(data) = relationship.get("data") else {
                // Связь без данных: только ссылки на API, агенту они не нужны.
                continue;
            };
            let resolved = match data {
                Value::Array(items) => Value::Array(
                    items
                        .iter()
                        .map(|item| resolve(item, index, stack, depth))
                        .collect(),
                ),
                Value::Null => Value::Null,
                item => resolve(item, index, stack, depth),
            };
            out.insert(name.clone(), resolved);
        }
    }
    if key.is_some() {
        stack.pop();
    }
    if let Some(meta) = object.get("meta") {
        out.insert("meta".to_owned(), meta.clone());
    }
    Value::Object(out)
}

/// Ссылка `{type, id}` → плоский связанный объект, если он есть в `included`.
fn resolve(
    linkage: &Value,
    index: &HashMap<(String, String), &Value>,
    stack: &mut Vec<(String, String)>,
    depth: usize,
) -> Value {
    let Some(key) = resource_key(linkage) else {
        return linkage.clone();
    };
    let Some(resource) = index.get(&key) else {
        return linkage.clone();
    };
    if depth + 1 >= MAX_DEPTH || stack.contains(&key) {
        let mut short = Map::new();
        short.insert("type".to_owned(), Value::String(key.0));
        short.insert("id".to_owned(), Value::String(key.1));
        return Value::Object(short);
    }
    flatten_resource(resource, index, stack, depth + 1)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn replaces_linkage_with_included_objects() {
        let mut body = json!({
            "data": [{
                "type": "apps",
                "id": "1",
                "attributes": {"name": "Demo", "bundleId": "dev.demo"},
                "relationships": {
                    "builds": {
                        "data": [{"type": "builds", "id": "b1"}, {"type": "builds", "id": "b2"}],
                        "links": {"related": "https://api.appstoreconnect.apple.com/v1/apps/1/builds"}
                    },
                    "appInfos": {"links": {"self": "x"}},
                    "primaryLocale": {"data": null}
                },
                "links": {"self": "https://api.appstoreconnect.apple.com/v1/apps/1"}
            }],
            "included": [
                {
                    "type": "builds", "id": "b1",
                    "attributes": {"version": "42"},
                    "relationships": {"app": {"data": {"type": "apps", "id": "1"}}}
                }
            ],
            "links": {"self": "x", "next": "y"},
            "meta": {"paging": {"total": 1, "limit": 50}}
        });
        flatten(&mut body);
        assert_eq!(
            body,
            json!({
                "data": [{
                    "id": "1",
                    "type": "apps",
                    "name": "Demo",
                    "bundleId": "dev.demo",
                    "builds": [
                        {"id": "b1", "type": "builds", "version": "42", "app": {"type": "apps", "id": "1"}},
                        {"type": "builds", "id": "b2"}
                    ],
                    "primaryLocale": null
                }],
                "meta": {"paging": {"total": 1, "limit": 50}}
            })
        );
    }

    #[test]
    fn single_resource_and_depth_limit() {
        let mut body = json!({
            "data": {
                "type": "a", "id": "1",
                "relationships": {"next": {"data": {"type": "b", "id": "2"}}}
            },
            "included": [
                {"type": "b", "id": "2", "relationships": {"next": {"data": {"type": "c", "id": "3"}}}},
                {"type": "c", "id": "3", "relationships": {"next": {"data": {"type": "d", "id": "4"}}}},
                {"type": "d", "id": "4", "attributes": {"deep": true}}
            ]
        });
        flatten(&mut body);
        assert_eq!(body["data"]["next"]["next"]["id"], "3");
        // Глубже MAX_DEPTH — только ссылка.
        assert_eq!(
            body["data"]["next"]["next"]["next"],
            json!({"type": "d", "id": "4"})
        );
        assert!(body.get("included").is_none());
    }

    #[test]
    fn body_without_data_is_kept() {
        let mut body = json!({"errors": []});
        flatten(&mut body);
        assert_eq!(body, json!({"errors": []}));
    }
}
