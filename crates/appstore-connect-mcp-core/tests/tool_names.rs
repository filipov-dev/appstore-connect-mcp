//! Имена инструментов в промптах и подсказках `release_status` существуют.

use std::collections::{BTreeMap, BTreeSet};

use appstore_connect_api::spec;
use appstore_connect_mcp_core::{composite, prompts};

/// Служебные инструменты сервера (не операции и не составные).
const SERVER_TOOLS: [&str; 4] = [
    "api_search",
    "api_request",
    "enable_toolset",
    "list_toolsets",
];

fn known_tools() -> BTreeSet<String> {
    spec::operations()
        .iter()
        .map(|op| op.tool_name.to_string())
        .chain(composite::all().iter().map(|t| t.name.to_string()))
        .chain(SERVER_TOOLS.iter().map(|s| s.to_string()))
        .collect()
}

/// Слова в обратных кавычках, похожие на имена инструментов (флаги `--...` не берутся).
fn backticked_names(text: &str) -> Vec<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter(|w| {
            !w.is_empty()
                && w.starts_with(|c: char| c.is_ascii_lowercase())
                && w.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
        .map(str::to_string)
        .collect()
}

fn renders(prompt: &prompts::PromptDef) -> Vec<String> {
    let mut all_args = BTreeMap::new();
    for arg in prompt.arguments {
        all_args.insert(arg.name.to_string(), "42".to_string());
    }
    let mut required = BTreeMap::new();
    required.insert("app_id".to_string(), "42".to_string());
    vec![
        (prompt.render)(&required),
        (prompt.render)(&all_args),
        prompt.description.to_string(),
    ]
}

#[test]
fn prompt_tool_names_exist() {
    let known = known_tools();
    for prompt in prompts::all() {
        let mut found = 0;
        for text in renders(&prompt) {
            for name in backticked_names(&text) {
                assert!(
                    known.contains(&name),
                    "prompt {} mentions unknown tool `{name}`",
                    prompt.name
                );
                found += 1;
            }
        }
        assert!(found > 5, "prompt {} mentions too few tools", prompt.name);
    }
}

/// Инструменты операций из промпта входят в наборы, которые промпт просит включить.
#[test]
fn prompt_tools_belong_to_listed_toolsets() {
    for prompt in prompts::all() {
        let text = (prompt.render)(&BTreeMap::from([("app_id".into(), "42".into())]));
        let line = text
            .split("Toolsets used here:")
            .nth(1)
            .and_then(|rest| rest.split('\n').next())
            .unwrap_or_else(|| panic!("prompt {} lists no toolsets", prompt.name));
        for name in backticked_names(&text) {
            let toolset = spec::find(&name).map(|op| op.toolset).or_else(|| {
                composite::all()
                    .into_iter()
                    .find(|t| t.name == name)
                    .map(|t| t.toolset)
            });
            if let Some(toolset) = toolset {
                assert!(
                    line.contains(toolset),
                    "prompt {}: `{name}` is in toolset {toolset}, not listed in: {line}",
                    prompt.name
                );
            }
        }
    }
}

#[test]
fn release_status_hint_tools_exist() {
    let known = known_tools();
    for name in composite::release_status::REFERENCED_TOOLS {
        assert!(known.contains(*name), "unknown tool {name}");
    }
}

#[test]
fn destructive_steps_match_access_levels() {
    use appstore_connect_api::AccessLevel;
    // Промпты называют эти шаги destructive-level: так и должно быть в таблице операций.
    for name in [
        "create_review_submission",
        "create_review_submission_item",
        "update_review_submission",
        "create_customer_review_response",
        "create_beta_app_review_submission",
        "create_build_beta_notification",
        "update_build",
    ] {
        let op = spec::find(name).unwrap_or_else(|| panic!("{name}"));
        assert_eq!(op.access, AccessLevel::Destructive, "{name}");
    }
    for name in [
        "add_beta_group_builds",
        "create_beta_build_localization",
        "set_app_store_version_build",
        "set_build_app_encryption_declaration",
    ] {
        let op = spec::find(name).unwrap_or_else(|| panic!("{name}"));
        assert_eq!(op.access, AccessLevel::Write, "{name}");
    }
}
