//! Представление ответа API в результате инструмента.
//!
//! - JSON без постраничной выдачи и с запасом квоты — компактный JSON ответа как есть
//!   (документ JSON:API: `data`, `included`, `links`, `meta`);
//! - JSON со следующей страницей или числом записей — `{"result": <ответ>, "pagination":
//!   {"next", "total", "limit"}}`: агент сразу видит адрес следующей страницы (`next` —
//!   для `api_request`) и общее число записей, не разбирая `links` и `meta.paging`;
//! - если квота запросов (`X-Rate-Limit`) почти исчерпана — меньше
//!   [`RATE_LIMIT_WARNING_PERCENT`] процентов, — в обёртку добавляется
//!   `"rate_limit": {"remaining", "limit"}`;
//! - текст — как есть (постраничная выдача и квота — отдельной строкой после текста);
//! - сохранённый файл — `{"saved_to", "bytes", "content_type"}`;
//! - пустое тело — `{"status"}`.

use appstore_connect_api::executor::{ApiResponse, Pagination, RateLimit, ResponseBody};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Map, Value, json};

use crate::error::ToolError;

/// Порог квоты запросов, ниже которого она показывается агенту.
pub const RATE_LIMIT_WARNING_PERCENT: u64 = 10;

/// Квота, о которой стоит сказать агенту: осталось меньше
/// [`RATE_LIMIT_WARNING_PERCENT`] процентов лимита.
pub fn low_rate_limit(rate: Option<&RateLimit>) -> Option<&RateLimit> {
    let rate = rate?;
    let remaining = rate.remaining?;
    let low = match rate.limit {
        Some(limit) if limit > 0 => {
            remaining.saturating_mul(100) < limit * RATE_LIMIT_WARNING_PERCENT
        }
        _ => remaining < 100,
    };
    low.then_some(rate)
}

/// Поля обёртки результата: `pagination` и `rate_limit`, если их нужно показать.
pub fn envelope(
    pagination: &Pagination,
    rate_limit: Option<&RateLimit>,
) -> Option<Map<String, Value>> {
    let mut extra = Map::new();
    if !pagination.is_empty() {
        extra.insert("pagination".to_owned(), to_json(pagination));
    }
    if let Some(rate) = low_rate_limit(rate_limit) {
        extra.insert("rate_limit".to_owned(), to_json(rate));
    }
    (!extra.is_empty()).then_some(extra)
}

/// Оборачивает JSON-результат в `{"result", ...extra}`, если есть что добавить.
pub fn wrap(result: Value, extra: Option<Map<String, Value>>) -> Value {
    match extra {
        None => result,
        Some(extra) => {
            let mut wrapped = Map::new();
            wrapped.insert("result".to_owned(), result);
            wrapped.extend(extra);
            Value::Object(wrapped)
        }
    }
}

/// Текст успешного ответа (см. описание модуля).
pub fn response_text(response: &ApiResponse) -> String {
    let extra = envelope(&response.pagination, response.rate_limit.as_ref());
    match &response.body {
        ResponseBody::Json(value) => wrap(value.clone(), extra).to_string(),
        ResponseBody::Text(text) => match extra {
            None => text.clone(),
            Some(extra) => format!("{text}\n\n{}", Value::Object(extra)),
        },
        ResponseBody::SavedToFile {
            path,
            bytes,
            content_type,
        } => json!({
            "saved_to": path.display().to_string(),
            "bytes": bytes,
            "content_type": content_type,
        })
        .to_string(),
        // Двоичный ответ без `save_to` сервер не запрашивает (`save_to` обязателен);
        // на всякий случай содержимое агенту не отдаётся.
        ResponseBody::Binary {
            bytes,
            content_type,
        } => json!({
            "status": response.status,
            "bytes": bytes.len(),
            "content_type": content_type,
            "note": "binary content is not returned; pass `save_to` to save it to a file",
        })
        .to_string(),
        ResponseBody::Empty => {
            let mut object = Map::new();
            object.insert("status".to_owned(), json!(response.status));
            object.extend(extra.unwrap_or_default());
            Value::Object(object).to_string()
        }
    }
}

fn to_json<T: serde::Serialize>(value: &T) -> Value {
    // Сериализация `Pagination` и `RateLimit` не может завершиться ошибкой.
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// Успешный результат инструмента.
pub fn success(text: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

/// Результат инструмента с `isError: true`.
pub fn failure(err: &ToolError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(err.message())])
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn response(
        body: ResponseBody,
        pagination: Pagination,
        rate: Option<RateLimit>,
    ) -> ApiResponse {
        ApiResponse {
            status: 204,
            body,
            pagination,
            rate_limit: rate,
        }
    }

    #[test]
    fn formats_bodies_without_pagination() {
        let none = Pagination::default;
        let plenty = Some(RateLimit {
            limit: Some(3600),
            remaining: Some(3500),
        });
        assert_eq!(
            response_text(&response(
                ResponseBody::Json(json!({"data": [1]})),
                none(),
                plenty
            )),
            r#"{"data":[1]}"#
        );
        assert_eq!(
            response_text(&response(ResponseBody::Text("plain".into()), none(), None)),
            "plain"
        );
        assert_eq!(
            response_text(&response(ResponseBody::Empty, none(), None)),
            r#"{"status":204}"#
        );
        let saved = response_text(&response(
            ResponseBody::SavedToFile {
                path: PathBuf::from("/tmp/x.gz"),
                bytes: 42,
                content_type: Some("application/a-gzip".into()),
            },
            none(),
            None,
        ));
        assert_eq!(
            saved,
            r#"{"bytes":42,"content_type":"application/a-gzip","saved_to":"/tmp/x.gz"}"#
        );
        let binary = response_text(&response(
            ResponseBody::Binary {
                bytes: vec![1, 2, 3],
                content_type: None,
            },
            none(),
            None,
        ));
        assert!(
            binary.contains("\"bytes\":3") && binary.contains("save_to"),
            "{binary}"
        );
    }

    #[test]
    fn shows_pagination_and_low_rate_limit() {
        let pagination = Pagination {
            next: Some("https://api.appstoreconnect.apple.com/v1/apps?cursor=AB".into()),
            total: Some(120),
            limit: Some(50),
        };
        let text = response_text(&response(
            ResponseBody::Json(json!({"data": []})),
            pagination.clone(),
            Some(RateLimit {
                limit: Some(3600),
                remaining: Some(12),
            }),
        ));
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["result"], json!({"data": []}));
        assert_eq!(value["pagination"]["total"], 120);
        assert_eq!(value["pagination"]["limit"], 50);
        assert!(
            value["pagination"]["next"]
                .as_str()
                .unwrap()
                .contains("cursor=AB")
        );
        assert_eq!(value["rate_limit"], json!({"limit": 3600, "remaining": 12}));

        // Только квота.
        let text = response_text(&response(
            ResponseBody::Json(json!({"data": {}})),
            Pagination::default(),
            Some(RateLimit {
                limit: None,
                remaining: Some(3),
            }),
        ));
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["rate_limit"]["remaining"], 3);
        assert!(value.get("pagination").is_none());

        let text = response_text(&response(ResponseBody::Text("t".into()), pagination, None));
        assert!(text.starts_with("t\n\n{\"pagination\":"), "{text}");
    }

    #[test]
    fn error_result_is_flagged() {
        let result = failure(&ToolError::new("boom"));
        assert_eq!(result.is_error, Some(true));
        assert_eq!(success("ok".into()).is_error, Some(false));
    }
}
