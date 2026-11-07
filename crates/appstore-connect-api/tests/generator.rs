//! Детерминизм генератора: одна и та же пара «снимок + дополнение» даёт тот же код,
//! и он совпадает с тем, что собрал `build.rs`.
//!
//! Тест подключает модули генератора из `build/` напрямую.

#[path = "../build/access.rs"]
mod access;
#[path = "../build/generate.rs"]
mod generate;
#[path = "../build/naming.rs"]
mod naming;
#[path = "../build/overlay.rs"]
mod overlay;
#[path = "../build/schema.rs"]
mod schema;

const SPEC: &str = include_str!("../spec/openapi.oas.json");
const OVERLAY: &str = include_str!("../spec/overlay.toml");
const BUILT: &str = include_str!(concat!(env!("OUT_DIR"), "/operations.rs"));

fn run() -> generate::Output {
    let spec: serde_json::Value = serde_json::from_str(SPEC).expect("спецификация — JSON");
    let overlay = overlay::parse(OVERLAY);
    generate::generate(&spec, &overlay)
}

#[test]
fn generation_is_deterministic_and_matches_build() {
    let first = run();
    let second = run();
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    assert_eq!(
        first.code, second.code,
        "два прогона генератора различаются"
    );
    assert_eq!(
        first.report, second.report,
        "отчёты двух прогонов различаются"
    );
    assert_eq!(
        first.code, BUILT,
        "код build.rs отличается от прогона генератора"
    );
}

#[test]
fn naming_rules() {
    use naming::{OperationName, abbreviate, to_snake_case};

    let name = |id: &str| {
        OperationName::parse(id)
            .unwrap_or_else(|| panic!("{id} не разобран"))
            .base_name()
    };
    assert_eq!(name("apps_getCollection"), "list_apps");
    assert_eq!(name("apps_getInstance"), "get_app");
    assert_eq!(name("appCategories_getInstance"), "get_app_category");
    assert_eq!(
        name("betaRecruitmentCriteria_createInstance"),
        "create_beta_recruitment_criterion"
    );
    assert_eq!(name("apps_builds_getToManyRelated"), "list_app_builds");
    assert_eq!(name("builds_app_getToOneRelated"), "get_build_app");
    assert_eq!(
        name("betaGroups_builds_createToManyRelationship"),
        "add_beta_group_builds"
    );
    assert_eq!(
        name("betaGroups_builds_deleteToManyRelationship"),
        "remove_beta_group_builds"
    );
    assert_eq!(
        name("appStoreVersions_build_updateToOneRelationship"),
        "set_app_store_version_build"
    );
    // Слова связи, повторяющие ресурс, отбрасываются.
    assert_eq!(
        name("appStoreVersions_appStoreVersionLocalizations_getToManyRelated"),
        "list_app_store_version_localizations"
    );
    // Версия ресурса не входит в базовое имя.
    assert_eq!(name("inAppPurchasesV2_getInstance"), "get_in_app_purchase");
    assert_eq!(
        OperationName::parse("inAppPurchasesV2_getInstance").and_then(|n| n.version),
        Some(2)
    );
    assert!(OperationName::parse("apps_doSomething").is_none());
    assert!(OperationName::parse("apps_builds_getCollection").is_none());

    assert_eq!(to_snake_case("ciMacOsVersions"), "ci_mac_os_versions");
    let rules = vec![("game_center".to_owned(), "gc".to_owned())];
    // Короткое имя не сокращается.
    assert_eq!(
        abbreviate("get_game_center_detail", &rules),
        "get_game_center_detail"
    );
    assert_eq!(
        abbreviate("list_game_center_leaderboard_set_member_locs", &rules),
        "list_gc_leaderboard_set_member_locs"
    );
    // Только по границам слов.
    assert_eq!(
        abbreviate("list_endgame_center_leaderboard_set_member_locs", &rules),
        "list_endgame_center_leaderboard_set_member_locs"
    );
}

#[test]
fn summaries_are_english_sentences() {
    let summary = |id: &str| {
        naming::OperationName::parse(id)
            .unwrap_or_else(|| panic!("{id} не разобран"))
            .summary()
    };
    assert_eq!(summary("apps_getCollection"), "List apps.");
    assert_eq!(summary("apps_getInstance"), "Get an app.");
    assert_eq!(
        summary("userInvitations_getInstance"),
        "Get a user invitation."
    );
    assert_eq!(
        summary("appStoreVersions_createInstance"),
        "Create an App Store version."
    );
    assert_eq!(
        summary("betaGroups_builds_createToManyRelationship"),
        "Add builds to a beta group."
    );
    assert_eq!(
        summary("ciProducts_buildRuns_getToManyRelated"),
        "List the build runs of a CI product."
    );
}
