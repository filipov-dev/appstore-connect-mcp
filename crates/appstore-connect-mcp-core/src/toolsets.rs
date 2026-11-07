//! Наборы инструментов (`--toolsets`).
//!
//! Набор — несколько тегов спецификации под коротким английским slug из
//! `appstore_connect_api::spec::toolsets()`. Набор выбирается точным совпадением slug; `all`
//! включает все наборы. Неизвестный набор — ошибка запуска со списком допустимых.
//!
//! Без выбора включается [`DEFAULT_TOOLSETS`]: приложения, версии и ревью, сборки и TestFlight
//! — то, с чем работают каждый день. Остальные наборы (метаданные и медиа, покупки и подписки,
//! цены, отчёты, подпись, Xcode Cloud, Game Center и т. д.) нужны реже, а описания их
//! инструментов заметно увеличивают список инструментов в контексте агента.

use appstore_connect_api::spec::{find_toolset, operations, toolsets};

/// Значение `--toolsets`, которое включает все наборы.
pub const ALL_TOOLSETS: &str = "all";

/// Наборы, включённые без `--toolsets` и без настройки профиля.
pub const DEFAULT_TOOLSETS: &[&str] = &["apps", "versions-review", "builds", "testflight"];

/// Набор инструментов: slug, описание и число операций в спецификации.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolset {
    pub slug: &'static str,
    pub description: &'static str,
    /// Теги спецификации, входящие в набор.
    pub tags: &'static [&'static str],
    /// Число инструментов операций набора (без исключённых операций).
    pub operations: usize,
    /// Входит в [`DEFAULT_TOOLSETS`].
    pub default: bool,
}

/// Все наборы в порядке файла-дополнения.
pub fn available_toolsets() -> Vec<Toolset> {
    toolsets()
        .iter()
        .map(|toolset| Toolset {
            slug: toolset.slug,
            description: toolset.description,
            tags: toolset.tags,
            operations: operations()
                .iter()
                .filter(|op| op.toolset == toolset.slug)
                .count(),
            default: DEFAULT_TOOLSETS.contains(&toolset.slug),
        })
        .collect()
}

/// Проверяет имена наборов: пробелы по краям и пустые элементы отбрасываются, повторы
/// убираются, `all` раскрывается во все наборы. Неизвестный набор — ошибка.
pub fn normalize_toolsets<S: AsRef<str>>(names: &[S]) -> Result<Vec<String>, String> {
    let mut result: Vec<String> = Vec::new();
    for name in names {
        let name = name.as_ref().trim();
        if name.is_empty() {
            continue;
        }
        if name == ALL_TOOLSETS {
            return Ok(toolsets().iter().map(|t| t.slug.to_owned()).collect());
        }
        if find_toolset(name).is_none() {
            let list: Vec<&str> = toolsets().iter().map(|t| t.slug).collect();
            return Err(format!(
                "unknown toolset `{name}`; available toolsets: {}, or `{ALL_TOOLSETS}`",
                list.join(", ")
            ));
        }
        if !result.iter().any(|known| known == name) {
            result.push(name.to_owned());
        }
    }
    Ok(result)
}

/// Наборы, которые действуют при выборе `selected` (`None` или пусто — по умолчанию).
pub fn effective_toolsets(selected: Option<&[String]>) -> Vec<String> {
    match selected {
        Some(names) if !names.is_empty() => names.to_vec(),
        _ => DEFAULT_TOOLSETS.iter().map(|s| (*s).to_owned()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_exist() {
        for slug in DEFAULT_TOOLSETS {
            assert!(find_toolset(slug).is_some(), "{slug}");
        }
        let all = available_toolsets();
        assert_eq!(
            all.iter().map(|t| t.operations).sum::<usize>(),
            operations().len()
        );
        assert_eq!(
            all.iter().filter(|t| t.default).count(),
            DEFAULT_TOOLSETS.len()
        );
    }

    #[test]
    fn normalize_validates_names() {
        assert_eq!(
            normalize_toolsets(&[" apps ", "", "builds", "apps"]).unwrap(),
            ["apps", "builds"]
        );
        let err = normalize_toolsets(&["Приложения"]).unwrap_err();
        assert!(err.contains("unknown toolset `Приложения`"), "{err}");
        assert!(err.contains("testflight") && err.contains("`all`"), "{err}");
        let all = normalize_toolsets(&["apps", "all"]).unwrap();
        assert_eq!(all.len(), toolsets().len());
        assert!(normalize_toolsets::<&str>(&[]).unwrap().is_empty());
    }

    #[test]
    fn effective_falls_back_to_defaults() {
        assert_eq!(effective_toolsets(None), DEFAULT_TOOLSETS);
        assert_eq!(effective_toolsets(Some(&[])), DEFAULT_TOOLSETS);
        let chosen = vec!["reviews".to_owned()];
        assert_eq!(effective_toolsets(Some(&chosen)), ["reviews"]);
    }
}
