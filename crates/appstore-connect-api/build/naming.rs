//! Имена MCP-инструментов и краткие описания операций.
//!
//! `operationId` Apple строятся по шаблону `<ресурс>[Vn]_<действие>` или
//! `<ресурс>[Vn]_<связь>_<действие>`: `apps_getCollection`, `apps_builds_getToManyRelated`,
//! `betaGroups_builds_createToManyRelationship`. Из них выводится имя «глагол_ресурс»:
//! `list_apps`, `list_app_builds`, `add_beta_group_builds`. Сокращения для длинных имён
//! и явные имена — в `spec/overlay.toml`.

/// Предел длины имени: клиенты добавляют префикс `mcp__appstore-connect__` (23 символа),
/// а полное имя инструмента не должно быть длиннее 64.
pub const MAX_TOOL_NAME: usize = 40;

/// Действие операции (последняя часть `operationId`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `getCollection`: список ресурсов верхнего уровня.
    List,
    /// `getInstance`.
    Get,
    /// `createInstance`.
    Create,
    /// `updateInstance`.
    Update,
    /// `deleteInstance`.
    Delete,
    /// `getToManyRelated`: список связанных ресурсов.
    ListRelated,
    /// `getToOneRelated`: связанный ресурс.
    GetRelated,
    /// `getToManyRelationship` / `getToOneRelationship`: только идентификаторы связанных
    /// ресурсов (linkage).
    ReadLinkage,
    /// `createToManyRelationship`.
    Add,
    /// `deleteToManyRelationship`.
    Remove,
    /// `replaceToManyRelationship`.
    Replace,
    /// `updateToOneRelationship`.
    Set,
    /// `getMetrics`: метрики ресурса (`/metrics/…`).
    Metrics,
}

impl Action {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "getCollection" => Self::List,
            "getInstance" => Self::Get,
            "createInstance" => Self::Create,
            "updateInstance" => Self::Update,
            "deleteInstance" => Self::Delete,
            "getToManyRelated" => Self::ListRelated,
            "getToOneRelated" => Self::GetRelated,
            "getToManyRelationship" | "getToOneRelationship" => Self::ReadLinkage,
            "createToManyRelationship" => Self::Add,
            "deleteToManyRelationship" => Self::Remove,
            "replaceToManyRelationship" => Self::Replace,
            "updateToOneRelationship" => Self::Set,
            "getMetrics" => Self::Metrics,
            _ => return None,
        })
    }

    fn verb(self) -> &'static str {
        match self {
            Self::List | Self::ListRelated => "list",
            Self::Get | Self::GetRelated | Self::Metrics => "get",
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::ReadLinkage => "get",
            Self::Add => "add",
            Self::Remove => "remove",
            Self::Replace => "replace",
            Self::Set => "set",
        }
    }

    /// Действие над связью (`…/{id}/<связь>`), а не над самим ресурсом.
    fn has_relationship(self) -> bool {
        !matches!(
            self,
            Self::List | Self::Get | Self::Create | Self::Update | Self::Delete
        )
    }
}

/// Разобранный `operationId`.
#[derive(Debug, Clone)]
pub struct OperationName {
    pub action: Action,
    /// Слова ресурса в snake_case, без суффикса версии: `beta`, `groups`.
    resource: Vec<String>,
    /// Слова связи без суффикса версии: `builds`.
    relationship: Vec<String>,
    /// Версия из суффикса `V2`/`V3` у ресурса или связи (`inAppPurchasesV2`).
    pub version: Option<u32>,
}

impl OperationName {
    /// Разбирает `operationId`; `None`, если он не следует шаблону Apple.
    pub fn parse(operation_id: &str) -> Option<Self> {
        let parts: Vec<&str> = operation_id.split('_').collect();
        let (resource, relationship, action) = match parts.as_slice() {
            [resource, action] => (*resource, None, Action::parse(action)?),
            [resource, relationship, action] => {
                (*resource, Some(*relationship), Action::parse(action)?)
            }
            _ => return None,
        };
        if action.has_relationship() != relationship.is_some() {
            return None;
        }
        let (resource, resource_version) = split_version(resource);
        let (relationship, relationship_version) = match relationship {
            Some(relationship) => split_version(relationship),
            None => ("", None),
        };
        Some(Self {
            action,
            resource: words(resource),
            relationship: words(relationship),
            version: resource_version.max(relationship_version),
        })
    }

    /// Базовое имя инструмента (без суффикса версии и сокращений).
    ///
    /// Слова связи, повторяющие начало имени ресурса, отбрасываются:
    /// `appStoreVersions_appStoreVersionLocalizations` → `list_app_store_version_localizations`,
    /// а не `list_app_store_version_app_store_version_localizations`.
    pub fn base_name(&self) -> String {
        let mut out = vec![self.action.verb().to_owned()];
        if self.action == Action::List {
            out.extend(self.resource.iter().cloned());
        } else {
            let parent = singular_words(&self.resource);
            let relationship = strip_common_prefix(&parent, &self.relationship);
            out.extend(parent);
            out.extend(relationship.iter().cloned());
        }
        out.join("_")
    }

    /// Краткое описание операции на английском: в спецификации Apple нет `summary`.
    pub fn summary(&self) -> String {
        let plural = humanize(&self.resource);
        let parent = humanize(&singular_words(&self.resource));
        let parent = with_article(&parent);
        let related = humanize(&self.relationship);
        match self.action {
            Action::List => format!("List {plural}."),
            Action::Get => format!("Get {parent}."),
            Action::Create => format!("Create {parent}."),
            Action::Update => format!("Update {parent}."),
            Action::Delete => format!("Delete {parent}."),
            Action::ListRelated => format!("List the {related} of {parent}."),
            Action::GetRelated => format!("Get the {related} of {parent}."),
            Action::ReadLinkage => format!("Get the IDs of the {related} of {parent}."),
            Action::Add => format!("Add {related} to {parent}."),
            Action::Remove => format!("Remove {related} from {parent}."),
            Action::Replace => format!("Replace the {related} of {parent}."),
            Action::Set => format!("Set the {related} of {parent}."),
            Action::Metrics => format!("Get {related} metrics of {parent}."),
        }
    }
}

/// Отделяет суффикс версии: `inAppPurchasesV2` → (`inAppPurchases`, `Some(2)`).
fn split_version(name: &str) -> (&str, Option<u32>) {
    let digits = name.len() - name.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return (name, None);
    }
    let without_digits = &name[..name.len() - digits];
    match without_digits.strip_suffix('V') {
        Some(base) if !base.is_empty() => (base, name[name.len() - digits..].parse().ok()),
        _ => (name, None),
    }
}

fn words(name: &str) -> Vec<String> {
    if name.is_empty() {
        return Vec::new();
    }
    to_snake_case(name).split('_').map(str::to_owned).collect()
}

/// camelCase → snake_case с учётом аббревиатур: `betaGroups` → `beta_groups`,
/// `ciMacOsVersions` → `ci_mac_os_versions`, `URLInfo` → `url_info`.
pub fn to_snake_case(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(chars.len() + 8);
    for (index, &ch) in chars.iter().enumerate() {
        if ch.is_ascii_uppercase() && index > 0 {
            let prev = chars[index - 1];
            let next_is_lower = chars.get(index + 1).is_some_and(|c| c.is_ascii_lowercase());
            if prev.is_ascii_lowercase()
                || prev.is_ascii_digit()
                || (prev.is_ascii_uppercase() && next_is_lower)
            {
                out.push('_');
            }
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
}

/// Единственное число последнего слова: `beta_groups` → `beta_group`,
/// `app_categories` → `app_category`, `beta_recruitment_criteria` → `…_criterion`.
fn singular_words(words: &[String]) -> Vec<String> {
    let mut out = words.to_vec();
    if let Some(last) = out.last_mut() {
        *last = singular(last);
    }
    out
}

fn singular(word: &str) -> String {
    const SAME: &[&str] = &["data", "status", "analytics", "news"];
    if SAME.contains(&word) || word.ends_with("ss") {
        return word.to_owned();
    }
    if word == "criteria" {
        return "criterion".to_owned();
    }
    if let Some(stem) = word.strip_suffix("ies") {
        return format!("{stem}y");
    }
    for suffix in ["sses", "shes", "ches", "xes", "uses"] {
        if word.ends_with(suffix) {
            return word[..word.len() - 2].to_owned();
        }
    }
    word.strip_suffix('s').unwrap_or(word).to_owned()
}

/// Отбрасывает из `relationship` слова, которыми начинается `parent` (общий префикс);
/// у связи остаётся хотя бы одно слово.
fn strip_common_prefix<'r>(parent: &[String], relationship: &'r [String]) -> &'r [String] {
    let common = parent
        .iter()
        .zip(relationship)
        .take_while(|(a, b)| a == b)
        .count();
    let common = common.min(relationship.len().saturating_sub(1));
    &relationship[common..]
}

/// Слова для текста: `ci_build_runs` → `CI build runs`, `in_app_purchases` → `in-app purchases`.
fn humanize(words: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut index = 0;
    while index < words.len() {
        let word = words[index].as_str();
        let next = words.get(index + 1).map(String::as_str);
        let pair = match (word, next) {
            ("in", Some("app")) => Some("in-app"),
            ("mac", Some("os")) => Some("macOS"),
            ("game", Some("center")) => Some("Game Center"),
            ("app", Some("store")) => Some("App Store"),
            ("app", Some("clip")) => Some("App Clip"),
            ("pre", Some("release")) => Some("pre-release"),
            _ => None,
        };
        if let Some(pair) = pair {
            out.push(pair.to_owned());
            index += 2;
            continue;
        }
        out.push(
            match word {
                "ci" => "CI",
                "scm" => "SCM",
                "id" => "ID",
                "ids" => "IDs",
                "ios" => "iOS",
                "xcode" => "Xcode",
                "url" => "URL",
                "eula" => "EULA",
                other => other,
            }
            .to_owned(),
        );
        index += 1;
    }
    out.join(" ")
}

/// Неопределённый артикль по первой букве. `u` — согласная: у Apple это `user`.
fn with_article(text: &str) -> String {
    let article = match text.chars().next().map(|c| c.to_ascii_lowercase()) {
        Some('a' | 'e' | 'i' | 'o') => "an",
        _ => "a",
    };
    format!("{article} {text}")
}

/// Применяет сокращения по порядку, пока имя длиннее [`MAX_TOOL_NAME`].
pub fn abbreviate(name: &str, rules: &[(String, String)]) -> String {
    let mut name = name.to_owned();
    for (from, to) in rules {
        if name.len() <= MAX_TOOL_NAME {
            break;
        }
        name = replace_words(&name, from, to);
    }
    name
}

/// Заменяет `from` на `to` только по границам слов snake_case.
fn replace_words(name: &str, from: &str, to: &str) -> String {
    let padded = format!("_{name}_");
    let replaced = padded.replace(&format!("_{from}_"), &format!("_{to}_"));
    replaced
        .trim_start_matches('_')
        .trim_end_matches('_')
        .to_owned()
}

/// Имя MCP-инструмента: строчные латинские буквы, цифры и `_`, не длиннее [`MAX_TOOL_NAME`].
pub fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_TOOL_NAME
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}
