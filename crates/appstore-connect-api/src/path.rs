//! Проверка и сборка относительных путей App Store Connect API.
//!
//! Это граница безопасности: всё, что уходит в [`crate::executor::ApiClient::execute`],
//! проходит через [`validate_relative_path`], поэтому запрос не может уйти на другой хост
//! или выйти за пределы API через `..`, в том числе в percent-кодированном виде.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

use crate::executor::ApiError;

/// Символы, которые кодируются в значении параметра пути.
///
/// Оставляем только unreserved-символы RFC 3986, поэтому `/`, `?`, `#`, `%`, `:` и прочие
/// разделители внутри значения становятся `%XX` и не могут изменить структуру пути.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Сколько раз раскодировать сегмент при проверке. Защищает от двойного кодирования
/// (`%252e%252e`) на случай, если что-то между нами и API раскодирует путь повторно.
const MAX_DECODE_ROUNDS: usize = 3;

/// Первый сегмент пути — версия API: `v1`, `v2`, `v3` и т. д. (`v` и число от 1 до 99
/// без ведущего нуля). Сейчас в спецификации Apple есть `/v1`, `/v2` и `/v3`.
pub fn is_api_version(segment: &str) -> bool {
    let Some(number) = segment.strip_prefix('v') else {
        return false;
    };
    !number.is_empty()
        && number.len() <= 2
        && !number.starts_with('0')
        && number.bytes().all(|b| b.is_ascii_digit())
}

/// Проверяет относительный путь App Store Connect API, например `/v1/apps/123/builds`.
///
/// Требования:
/// - начинается с версии API (`/v1/`, `/v2/`, ... — см. [`is_api_version`]), поэтому не
///   может быть `//`, схемой или хостом;
/// - содержит только символы `pchar` из RFC 3986 и `/`: `?`, `#`, `\`, пробелы,
///   управляющие и не-ASCII символы отклоняются (query передаётся отдельно);
/// - `%` всегда образует корректную тройку `%XX`;
/// - нет пустых сегментов (`//`) кроме завершающего `/`;
/// - ни один сегмент ни в исходном, ни в раскодированном виде не равен `.` или `..`,
///   а раскодированный сегмент не содержит `.`/`..` между `/` или `\` и управляющих символов.
pub fn validate_relative_path(path: &str) -> Result<(), ApiError> {
    let invalid = |reason: &str| {
        Err(ApiError::InvalidPath(format!(
            "{reason}: `{}`",
            shorten(path)
        )))
    };

    if !path.starts_with('/') {
        return invalid("the path must be relative and start with `/v1/` (or another API version)");
    }
    if path.starts_with("//") {
        return invalid("the path must not start with `//`");
    }
    if let Some(c) = path.chars().find(|c| !is_allowed_char(*c)) {
        return Err(ApiError::InvalidPath(format!(
            "character {c:?} is not allowed in the path `{}`; pass query parameters \
             separately and encode other characters as %XX",
            shorten(path)
        )));
    }
    if !has_valid_percent_triplets(path) {
        return invalid("invalid percent-encoding in the path");
    }

    let segments: Vec<&str> = path[1..].split('/').collect();
    let version_ok = segments.len() >= 2 && is_api_version(segments[0]) && !segments[1].is_empty();
    if !version_ok {
        return invalid(
            "the path must start with an API version (`/v1/`, `/v2/`, ...) and a resource",
        );
    }
    let last = segments.len().saturating_sub(1);
    for (index, segment) in segments.iter().enumerate() {
        if segment.is_empty() {
            // Пустой сегмент допустим только в конце (`/v1/apps/`).
            if index == last {
                continue;
            }
            return invalid("empty path segment");
        }
        if !segment_is_safe(segment) {
            return invalid("`.` and `..` segments are not allowed, encoded or not");
        }
    }
    Ok(())
}

/// Подставляет параметры в шаблон пути вида `/v1/apps/{id}/relationships/builds`
/// и проверяет результат через [`validate_relative_path`].
///
/// Значение параметра кодируется как один сегмент пути: `a/b` превращается в `a%2Fb`,
/// поэтому значение не может добавить сегменты или выйти из пути. Отсутствующий или
/// пустой параметр даёт [`ApiError::MissingPathParam`]. Значения `.` и `..` отклоняются
/// проверкой итогового пути.
pub fn render_path<F>(template: &str, mut lookup: F) -> Result<String, ApiError>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(open) = rest.find(['{', '}']) {
        if rest.as_bytes()[open] == b'}' {
            return Err(ApiError::InvalidPath(format!(
                "unmatched `}}` in the path template `{}`",
                shorten(template)
            )));
        }
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err(ApiError::InvalidPath(format!(
                "unclosed `{{` in the path template `{}`",
                shorten(template)
            )));
        };
        let name = &after[..close];
        if name.is_empty() || name.contains('{') {
            return Err(ApiError::InvalidPath(format!(
                "invalid parameter in the path template `{}`",
                shorten(template)
            )));
        }
        let value = lookup(name)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::MissingPathParam(name.to_string()))?;
        out.extend(utf8_percent_encode(&value, SEGMENT));
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    validate_relative_path(&out)?;
    Ok(out)
}

/// `pchar` из RFC 3986 плюс разделитель `/`; `%` проверяется отдельно.
fn is_allowed_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '.'
                | '_'
                | '~'
                | '!'
                | '$'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | '+'
                | ','
                | ';'
                | '='
                | ':'
                | '@'
                | '/'
                | '%'
        )
}

fn has_valid_percent_triplets(path: &str) -> bool {
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let ok = bytes
                .get(i + 1..i + 3)
                .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit));
            if !ok {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    true
}

/// Проверяет сегмент и его раскодированные варианты.
///
/// Закодированный `/` внутри значения (`a%2Fb`) допустим — так `render_path` передаёт
/// значения со слешем. Но если сервер раскодирует `%2F` до маршрутизации, `..%2F..`
/// превратится в обход каталога, поэтому раскодированный сегмент дополнительно делим
/// по `/` и `\` и ищем `.`/`..`.
fn segment_is_safe(segment: &str) -> bool {
    let mut current = segment.to_string();
    for _ in 0..=MAX_DECODE_ROUNDS {
        if current
            .split(['/', '\\'])
            .any(|part| part == "." || part == "..")
        {
            return false;
        }
        if current.chars().any(char::is_control) {
            return false;
        }
        if !current.contains('%') {
            return true;
        }
        let decoded = percent_decode_str(&current)
            .decode_utf8_lossy()
            .into_owned();
        if decoded == current {
            return true;
        }
        current = decoded;
    }
    // Слишком глубокое вложенное кодирование — подозрительно, отклоняем.
    false
}

/// Укорачивает путь для текста ошибки, чтобы не раздувать ответ MCP.
pub(crate) fn shorten(path: &str) -> String {
    const MAX: usize = 200;
    if path.len() <= MAX {
        return path.escape_debug().to_string();
    }
    let mut end = MAX;
    while !path.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", path[..end].escape_debug())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn api_versions() {
        for good in ["v1", "v2", "v3", "v10", "v99"] {
            assert!(is_api_version(good), "{good}");
        }
        for bad in ["", "v", "v0", "v01", "v100", "V1", "v1a", "1"] {
            assert!(!is_api_version(bad), "{bad}");
        }
    }

    #[test]
    fn accepts_valid_paths() {
        for path in [
            "/v1/apps",
            "/v2/inAppPurchases/123",
            "/v3/appPricePoints/abc",
            "/v1/apps/123/relationships/builds",
            "/v1/apps/",
            "/v1/files/file%20name.png",
            "/v1/a/b..c/.hidden/d.",
            "/v1/entities/id:1@x",
        ] {
            assert!(validate_relative_path(path).is_ok(), "{path} must pass");
        }
    }

    #[test]
    fn rejects_unsafe_paths() {
        for path in [
            "",
            "/",
            "/v1",
            "/v1/",
            "/v0/apps",
            "/V1/apps",
            "/apps",
            "v1/apps",
            "https://evil.example/v1/apps",
            "https://api.appstoreconnect.apple.com/v1/apps",
            "http://evil.example",
            "//evil.example/v1/apps",
            "///evil",
            "/v1//apps",
            "/v1/apps/../admin",
            "/v1/../v2/apps",
            "/v1/./x",
            "/v1/apps/..",
            "/v1/apps/%2e%2e/admin",
            "/v1/apps/%2E%2E/admin",
            "/v1/apps/.%2e/admin",
            "/v1/apps/%2e/admin",
            "/v1/apps/..%2fadmin",
            "/v1/apps/x%2F..%2Fadmin",
            "/v1/apps/..%5cadmin",
            "/v1/apps/%252e%252e/admin",
            "/v1/apps/%25252e%25252e/admin",
            "/v1/apps?limit=1",
            "/v1/apps#frag",
            "/v1/apps\\..\\admin",
            "/v1/apps/a b",
            "/v1/apps/\n",
            "/v1/apps/%00",
            "/v1/apps/%0a",
            "/v1/apps/%zz",
            "/v1/apps/%2",
            "/v1/apps/{id}",
            "/v1/apps/кириллица",
            "/v1/apps/x\u{0}",
        ] {
            assert!(
                matches!(validate_relative_path(path), Err(ApiError::InvalidPath(_))),
                "{path:?} must be rejected"
            );
        }
    }

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn render_substitutes_and_encodes() {
        let p = params(&[("id", "123"), ("other", "a/b"), ("name", "K?#% x.png")]);
        let rendered =
            render_path("/v1/apps/{id}/things/{other}/{name}", |n| p.get(n).cloned()).unwrap();
        assert_eq!(rendered, "/v1/apps/123/things/a%2Fb/K%3F%23%25%20x.png");
    }

    #[test]
    fn render_encodes_non_ascii() {
        let p = params(&[("id", "файл")]);
        let rendered = render_path("/v1/apps/{id}", |n| p.get(n).cloned()).unwrap();
        assert_eq!(rendered, "/v1/apps/%D1%84%D0%B0%D0%B9%D0%BB");
    }

    #[test]
    fn render_reports_missing_and_empty_params() {
        let p = params(&[("id", "1"), ("empty", "")]);
        let lookup = |n: &str| p.get(n).cloned();
        assert!(matches!(
            render_path("/v1/apps/{id}/{missing}", lookup),
            Err(ApiError::MissingPathParam(name)) if name == "missing"
        ));
        assert!(matches!(
            render_path("/v1/apps/{id}/builds/{empty}", lookup),
            Err(ApiError::MissingPathParam(name)) if name == "empty"
        ));
    }

    #[test]
    fn render_rejects_traversal_values() {
        for value in ["..", ".", "../admin", "..\\x"] {
            let result = render_path("/v1/apps/{x}/builds", |_| Some(value.to_string()));
            assert!(
                matches!(result, Err(ApiError::InvalidPath(_))),
                "value {value:?} must be rejected"
            );
        }
    }

    #[test]
    fn render_rejects_malformed_templates() {
        for template in [
            "/v1/apps/{id",
            "/v1/apps/id}",
            "/v1/apps/{}",
            "/v1/apps/{{x}}",
        ] {
            assert!(
                matches!(
                    render_path(template, |_| Some("v".into())),
                    Err(ApiError::InvalidPath(_))
                ),
                "{template} must be rejected"
            );
        }
    }

    #[test]
    fn render_rejects_absolute_template() {
        assert!(render_path("https://evil/{x}", |_| Some("v".into())).is_err());
        // Значение параметра на месте версии не может увести путь с API: `/` кодируется.
        assert!(render_path("/{version}/apps", |_| Some("evil.example".into())).is_err());
        assert!(render_path("/{version}/apps", |_| Some("/v1".into())).is_err());
    }
}
