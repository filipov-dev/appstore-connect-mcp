//! Ошибки вызова инструмента.
//!
//! Любая ошибка вызова возвращается MCP-клиенту как `CallToolResult` с `isError: true`
//! и текстом для агента на английском: агент должен понять, что исправить в аргументах
//! или что попросить у пользователя. Тип ошибки — [`ToolError`] контракта составных
//! инструментов: у всех инструментов сервера одна и та же ошибка.
//!
//! Ключ и JWT в тексты ошибок не попадают: [`ApiError`] их не содержит, аргументы
//! инструментов ключа не принимают, а сервер дополнительно вырезает JWT клиента и значения,
//! похожие на секреты, из итогового текста.

use appstore_connect_api::executor::{ApiError, HttpError};

pub use crate::composite::ToolError;

/// Текст ошибки исполнителя запросов для агента: текст [`ApiError`] (он на английском)
/// и подсказка, что делать дальше.
pub fn describe_api_error(err: &ApiError) -> String {
    match err {
        ApiError::Http(http) => describe_http_error(http),
        ApiError::MissingPathParam(name) => format!("missing path argument `{name}`"),
        ApiError::InvalidPath(_) => "an identifier argument does not form a valid API path: \
             empty values and values like `.` or `..` are rejected"
            .to_owned(),
        ApiError::InvalidConfig(detail) => {
            // Подробности — для лога: они относятся к настройке сервера.
            tracing::warn!(%detail, "некорректная конфигурация клиента App Store Connect API");
            "the server is misconfigured (invalid API address or timeouts); ask the user to \
             check the server log"
                .to_owned()
        }
        ApiError::MissingKey => "no App Store Connect API key is configured for this server; \
             ask the user to store one as described in the server instructions"
            .to_owned(),
        ApiError::Token(_) => "could not sign a JWT with the stored App Store Connect API key, \
             so the request was not sent; ask the user to store the key again as described in \
             the server instructions"
            .to_owned(),
        ApiError::ResponseTooLarge { limit } => format!(
            "the App Store Connect API response is larger than {limit} bytes and was not \
             returned. Narrow the request: ask only for the fields you need (`fields.<type>`), \
             use a smaller page (`limit`), fewer `include` relationships or stricter `filter.*` \
             arguments, or select less with `jq`"
        ),
        ApiError::Transport(detail) => format!(
            "could not reach the App Store Connect API (network error or timeout); retry later. \
             Details: {detail}"
        ),
        other => other.to_string(),
    }
}

/// Текст ответа App Store Connect со статусом не 2xx: статус, ошибки JSON:API и подсказка.
fn describe_http_error(err: &HttpError) -> String {
    let hint = match err.status {
        300..=399 => {
            " The resource is served from another host, and redirects outside the API are not \
             followed."
        }
        400 | 409 | 422 => {
            " The request was rejected: check the arguments and the request body against the \
             tool schema (`api_search` with `tool_name` shows it); the `source` of each error \
             names the wrong field or parameter."
        }
        401 => {
            " The API key was rejected: it may be revoked or the profile has a wrong Key ID or \
             Issuer ID. Ask the user to store the key again as described in the server \
             instructions; never ask for the key in the conversation."
        }
        403 => {
            " The API key's role does not allow this operation, or the resource belongs to \
             another team. Ask the user to check the key's role in App Store Connect (Users \
             and Access > Integrations)."
        }
        404 => " Not found: check the IDs, and that the key's team can see the resource.",
        429 => " Rate limited: the hourly request quota is used up; retry later.",
        500..=599 => " App Store Connect server error: retry later.",
        _ => "",
    };
    format!("{err}.{hint}")
}

#[cfg(test)]
mod tests {
    use appstore_connect_api::executor::{ErrorSource, JsonApiError, RateLimit};

    use super::*;

    fn http(status: u16, errors: Vec<JsonApiError>, body: &str) -> ApiError {
        ApiError::Http(Box::new(HttpError {
            status,
            errors,
            body: body.to_owned(),
            retry_after: None,
            rate_limit: None,
        }))
    }

    #[test]
    fn http_errors_have_status_details_and_hint() {
        let text = describe_api_error(&http(
            409,
            vec![JsonApiError {
                code: Some("ENTITY_ERROR.ATTRIBUTE.INVALID".into()),
                title: Some("An attribute value is invalid.".into()),
                detail: Some("'versionString' is not valid".into()),
                source: Some(ErrorSource::Pointer(
                    "/data/attributes/versionString".into(),
                )),
                ..JsonApiError::default()
            }],
            "{}",
        ));
        assert!(text.contains("HTTP 409"), "{text}");
        assert!(text.contains("ENTITY_ERROR.ATTRIBUTE.INVALID"), "{text}");
        assert!(text.contains("/data/attributes/versionString"), "{text}");
        assert!(text.contains("tool schema"), "{text}");

        let text = describe_api_error(&http(401, Vec::new(), ""));
        assert!(text.contains("server instructions"), "{text}");
        assert!(text.contains("never ask for the key"), "{text}");

        let text = describe_api_error(&ApiError::Http(Box::new(HttpError {
            status: 429,
            errors: Vec::new(),
            body: String::new(),
            retry_after: Some(40),
            rate_limit: Some(RateLimit {
                limit: Some(3600),
                remaining: Some(0),
            }),
        })));
        assert!(text.contains("retry after 40 s"), "{text}");
        assert!(text.contains("0 of 3600"), "{text}");
        assert!(text.contains("Rate limited"), "{text}");
    }

    #[test]
    fn other_errors() {
        assert_eq!(
            describe_api_error(&ApiError::MissingPathParam("id".into())),
            "missing path argument `id`"
        );
        assert!(describe_api_error(&ApiError::InvalidPath("x".into())).contains("valid API path"));
        let text = describe_api_error(&ApiError::ResponseTooLarge { limit: 1024 });
        assert!(
            text.contains("1024 bytes") && text.contains("fields."),
            "{text}"
        );
        assert!(
            describe_api_error(&ApiError::Transport("timeout".into()))
                .contains("could not reach the App Store Connect API")
        );
        assert!(describe_api_error(&ApiError::MissingKey).contains("server instructions"));
        assert_eq!(
            describe_api_error(&ApiError::InvalidRequest("bad".into())),
            "invalid request: bad"
        );
    }
}
