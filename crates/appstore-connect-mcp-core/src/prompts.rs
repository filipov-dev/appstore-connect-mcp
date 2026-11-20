//! MCP-промпты: готовые сценарии для частых задач (подготовка релиза, разбор отзывов,
//! раздача сборки в TestFlight).
//!
//! Контракт с сервером: [`all`] возвращает список промптов, сервер отдаёт его в
//! `prompts/list` и рендерит текст в `prompts/get` через [`PromptDef::render`].
//! Промпты только описывают шаги для агента и ссылаются на инструменты по имени;
//! сами они запросов не делают.
//!
//! Тексты — для агента, на английском. Имена инструментов в тексте пишутся в обратных
//! кавычках: тест проверяет, что каждое такое имя существует (операции, составные
//! инструменты или служебные инструменты сервера).

use std::collections::BTreeMap;

/// Аргумент промпта.
#[derive(Debug, Clone, Copy)]
pub struct PromptArg {
    pub name: &'static str,
    pub description: &'static str,
    pub required: bool,
}

/// Описание промпта.
#[derive(Clone, Copy)]
pub struct PromptDef {
    /// Имя промпта (snake_case).
    pub name: &'static str,
    /// Заголовок для клиента.
    pub title: &'static str,
    /// Описание для клиента и агента (английский).
    pub description: &'static str,
    pub arguments: &'static [PromptArg],
    /// Текст сообщения пользователя по аргументам. Обязательные аргументы сервер
    /// проверяет до вызова.
    pub render: fn(&BTreeMap<String, String>) -> String,
}

impl std::fmt::Debug for PromptDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptDef")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Все промпты сервера.
pub fn all() -> Vec<PromptDef> {
    vec![PREPARE_RELEASE, TRIAGE_REVIEWS, TESTFLIGHT_DISTRIBUTION]
}

/// Значение аргумента без пробелов по краям; пустая строка — аргумента нет.
fn arg<'a>(args: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    args.get(name).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// Обязательный аргумент. Сервер проверяет его до рендера; если его всё же нет, в тексте
/// остаётся просьба уточнить значение у пользователя.
fn required<'a>(args: &'a BTreeMap<String, String>, name: &str) -> &'a str {
    arg(args, name).unwrap_or("<ask the user>")
}

const DYNAMIC_NOTE: &str = "If a tool below is not available, the server may run with dynamic \
toolsets: call `list_toolsets` and turn on the needed toolsets with `enable_toolset`. If a \
tool is above the server's access level, tell the user which access it needs (write or \
destructive, set for the server's profile with `appstore-connect-mcp ui`) instead of working \
around it with `api_request`.";

// ---------------------------------------------------------------------------------------
// prepare_release
// ---------------------------------------------------------------------------------------

const PREPARE_RELEASE: PromptDef = PromptDef {
    name: "prepare_release",
    title: "Prepare an App Store release",
    description: "Checklist before submitting an App Store version for review: version, build, \
                  metadata in every locale, screenshots, age rating, App Review details, export \
                  compliance, pricing and availability. Submits only after the user's explicit \
                  confirmation.",
    arguments: &[
        PromptArg {
            name: "app_id",
            description: "App Store Connect app ID (from list_apps).",
            required: true,
        },
        PromptArg {
            name: "platform",
            description: "IOS (default), MAC_OS, TV_OS or VISION_OS.",
            required: false,
        },
        PromptArg {
            name: "version",
            description: "Version string to release, for example 2.4.0.",
            required: false,
        },
    ],
    render: render_prepare_release,
};

fn render_prepare_release(args: &BTreeMap<String, String>) -> String {
    let app_id = required(args, "app_id");
    let platform = arg(args, "platform").unwrap_or("IOS");
    let version = match arg(args, "version") {
        Some(v) => format!("version {v}"),
        None => "the next version (ask the user for the version string if none is in \
                 preparation)"
            .to_string(),
    };
    format!(
        "Prepare {version} of app {app_id} on platform {platform} for App Review. Work \
through the checklist, fix what you can with the user's agreement, and report each item as \
done / missing / needs the user.

{DYNAMIC_NOTE} Toolsets used here: versions-review, apps, metadata, media, builds, \
pricing-availability.

1. Current state: call `release_status` (app_id {app_id}, platform {platform}). Note the \
live version, the version in preparation, its build, the latest review submission and the \
next_steps hints.
2. Version: if no version is in preparation, create it with `create_app_store_version` \
(write). If the version string differs from the requested one, change it with \
`update_app_store_version`. Check the release type (MANUAL, AFTER_APPROVAL, SCHEDULED with \
earliestReleaseDate) with the user.
3. Build: the version must have a build with processing state VALID that is not expired. \
Find candidates with `list_builds` (filter.app, sort -uploadedDate) and attach one with \
`set_app_store_version_build`. A build that is still PROCESSING cannot be selected yet.
4. Export compliance: the selected build must answer usesNonExemptEncryption. If it is \
missing, ask the user and set it with `update_build` (destructive-level) or attach a \
declaration from `list_app_encryption_declarations` with \
`set_build_app_encryption_declaration`.
5. Metadata in every locale: run `metadata_export` (or `list_app_store_version_localizations` \
and `list_app_info_localizations`). Every locale needs a description, keywords, support URL \
and, for updates, What's New; app info needs name and privacy policy URL. Fix gaps with \
`metadata_apply`, `update_app_store_version_localization` or \
`update_app_info_localization` using texts the user approved.
6. Screenshots: for each version localization check the screenshot sets with \
`list_version_loc_screenshot_sets` and `list_app_screenshot_set_screenshots`. The required \
display sizes depend on the devices: for iPhone apps APP_IPHONE_67 (6.9\"/6.7\") or \
APP_IPHONE_65, for iPad apps also APP_IPAD_PRO_3GEN_129; other platforms need their own type \
(APP_DESKTOP, APP_APPLE_TV, APP_APPLE_VISION_PRO). Upload missing screenshots with \
`asset_upload` from files the user provides.
7. Age rating: find the app info with `list_app_infos`, read \
`get_app_info_age_rating_declaration` and confirm the answers with the user; change them \
with `update_age_rating_declaration`.
8. App Review details: read `get_app_store_version_review_detail`. Contact first and last \
name, phone and email are required; if the app needs a login, a demo account is required \
too. Create or fix them with `create_app_store_review_detail` or \
`update_app_store_review_detail`. Never invent contact data or demo credentials: ask the \
user, and do not repeat passwords back in the conversation.
9. Pricing and availability (important for the first release): check \
`get_app_price_schedule_for_app` and `get_app_availability_for_app`; changes there are \
destructive-level, discuss them with the user first.
10. Optional phased release for an update: `create_app_store_version_phased_release` \
(destructive-level), only if the user asks for it.
11. Summary: show the user the checklist with the result of every item and list what will \
be submitted (app, platform, version, build). Ask for an explicit confirmation to submit. \
Do not submit without a clear yes.
12. Submission (only after the confirmation): `create_review_submission` for the platform, \
`create_review_submission_item` with the App Store version, then \
`update_review_submission` with submitted: true. If an open draft submission already \
exists (see `release_status`), add the item to it instead of creating a new one. These \
tools are destructive-level: the server needs destructive access. Finally call \
`release_status` again and report the review state."
    )
}

// ---------------------------------------------------------------------------------------
// triage_reviews
// ---------------------------------------------------------------------------------------

const TRIAGE_REVIEWS: PromptDef = PromptDef {
    name: "triage_reviews",
    title: "Triage customer reviews",
    description: "Fetch recent App Store customer reviews, group them by theme and rating, and \
                  draft responses. Posts a response only after the user approves its exact \
                  text.",
    arguments: &[
        PromptArg {
            name: "app_id",
            description: "App Store Connect app ID (from list_apps).",
            required: true,
        },
        PromptArg {
            name: "days",
            description: "How many recent days to cover (default 30).",
            required: false,
        },
        PromptArg {
            name: "territory",
            description: "Territory code to limit the reviews to, for example USA or DEU.",
            required: false,
        },
    ],
    render: render_triage_reviews,
};

fn render_triage_reviews(args: &BTreeMap<String, String>) -> String {
    let app_id = required(args, "app_id");
    let days = arg(args, "days").unwrap_or("30");
    let territory = match arg(args, "territory") {
        Some(t) => format!(" with filter.territory {t}"),
        None => String::new(),
    };
    format!(
        "Triage the customer reviews of app {app_id} from the last {days} days.

{DYNAMIC_NOTE} Toolsets used here: apps (reading reviews) and reviews (responses).

1. Fetch: call `list_app_customer_reviews` (id {app_id}, sort -createdDate, limit 200, \
include response){territory}. The API has no date filter: keep requesting the next page \
only while the oldest review on the page is newer than {days} days ago, then drop older \
reviews. Use the jq argument to keep only rating, title, body, territory, createdDate and \
whether a response exists. `list_app_customer_review_summarizations` gives Apple's summary \
when it is available.
2. Group: count reviews by rating (1-5) and group them by theme (crashes and bugs, \
performance, login or account, payments and subscriptions, feature requests, UX, praise, \
other). For each theme give the count, the typical rating and one or two short paraphrased \
examples. Point out themes that grew recently or that mention a specific version.
3. Prioritise: list the reviews that deserve a response first (low ratings with a concrete \
problem, questions, reviews without a response).
4. Draft responses: one per selected review, in the language of the review, short, polite, \
specific to what the customer wrote, without promises of dates, without personal data and \
without asking the customer to contact you with secrets. Show each draft next to the review \
it answers.
5. Approval: ask the user to approve, edit or reject every draft individually. Never post a \
response the user has not approved in its final wording.
6. Posting (only approved texts): `create_customer_review_response` with the review ID and \
the approved text. It is destructive-level (the response is public): the server needs \
destructive access. A review has at most one response; replacing or removing one \
(`delete_customer_review_response`) also needs the user's explicit approval.
7. Report what was posted and what is left.

Reviews contain customers' personal data (nicknames, texts): use them only for this task and \
do not copy them into files, commits or other systems unless the user asks."
    )
}

// ---------------------------------------------------------------------------------------
// testflight_distribution
// ---------------------------------------------------------------------------------------

const TESTFLIGHT_DISTRIBUTION: PromptDef = PromptDef {
    name: "testflight_distribution",
    title: "Distribute a build in TestFlight",
    description: "Find a build, check processing and export compliance, set What to Test, add \
                  the build to beta groups and submit it for beta review for external groups. \
                  Explains which steps write or are destructive and confirms them with the \
                  user.",
    arguments: &[
        PromptArg {
            name: "app_id",
            description: "App Store Connect app ID (from list_apps).",
            required: true,
        },
        PromptArg {
            name: "build",
            description: "Build number (CFBundleVersion) or build ID; default — the latest \
                          build.",
            required: false,
        },
        PromptArg {
            name: "groups",
            description: "Comma-separated beta group names; default — ask the user.",
            required: false,
        },
    ],
    render: render_testflight_distribution,
};

fn render_testflight_distribution(args: &BTreeMap<String, String>) -> String {
    let app_id = required(args, "app_id");
    let build = match arg(args, "build") {
        Some(b) => format!(
            "build {b} (a build number: `list_builds` with filter.app {app_id} and \
             filter.version {b}; a build ID: `get_build`)"
        ),
        None => format!(
            "the latest build (`list_builds` with filter.app {app_id}, sort -uploadedDate, \
             limit 5)"
        ),
    };
    let groups = match arg(args, "groups") {
        Some(g) => format!("the beta groups {g}"),
        None => "the beta groups the user chooses".to_string(),
    };
    format!(
        "Distribute {build} of app {app_id} in TestFlight to {groups}.

{DYNAMIC_NOTE} Toolsets used here: apps, builds, testflight and versions-review.

Access levels: reading is always allowed; steps marked (write) need write access; steps \
marked (destructive) need destructive access. Before any write or destructive step, tell \
the user what will change and get a yes; destructive steps notify testers or Apple and \
cannot be taken back.

1. Find the build: use the lookup above (or `release_status` for an overview). Include \
buildBetaDetail and preReleaseVersion to see the TestFlight states and the app version.
2. Processing: the build must have processing state VALID and must not be expired. If it is \
PROCESSING, wait and check again; FAILED or INVALID builds cannot be distributed.
3. Export compliance: if usesNonExemptEncryption is not answered or the beta state is \
MISSING_EXPORT_COMPLIANCE, ask the user and set it with `update_build` (destructive) or \
attach an approved declaration with `set_build_app_encryption_declaration` (write).
4. What to Test: read `list_beta_build_localizations` (filter.build). Ask the user for the \
text and create or update it with `create_beta_build_localization` or \
`update_beta_build_localization` (write).
5. Groups: list the app's groups with `list_beta_groups` (filter.app {app_id}) and match the \
requested names; show which are internal (isInternalGroup) and which are external. Do not \
create groups unless the user asks (`create_beta_group`, write).
6. Internal groups: add the build with `add_beta_group_builds` (write). Groups with access \
to all builds get it automatically.
7. External groups need beta app review for the first build of a version: check \
`get_app_beta_app_review_detail` (contact and demo account; fix with \
`update_beta_app_review_detail`, write, never inventing credentials) and the beta app \
description with `list_beta_app_localizations`. Add the build to the external groups with \
`add_beta_group_builds` (write), then, after the user's confirmation, submit it with \
`create_beta_app_review_submission` (destructive). Check the result with \
`get_build_beta_app_review_submission`.
8. Notify testers only if the user wants it and automatic notifications are off: \
`create_build_beta_notification` (destructive).
9. Report the build, the groups it was added to and the beta review state."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn prompts_have_unique_names_and_required_app_id() {
        let prompts = all();
        let names: Vec<_> = prompts.iter().map(|p| p.name).collect();
        assert_eq!(
            names,
            [
                "prepare_release",
                "triage_reviews",
                "testflight_distribution"
            ]
        );
        for prompt in &prompts {
            let required: Vec<_> = prompt
                .arguments
                .iter()
                .filter(|a| a.required)
                .map(|a| a.name)
                .collect();
            assert_eq!(required, ["app_id"], "{}", prompt.name);
            assert!(!prompt.title.is_empty() && !prompt.description.is_empty());
        }
    }

    #[test]
    fn render_with_required_args_only() {
        for prompt in all() {
            let text = (prompt.render)(&args(&[("app_id", "1234567890")]));
            assert!(text.contains("1234567890"), "{}: {text}", prompt.name);
            assert!(!text.contains("<ask the user>"), "{}", prompt.name);
            assert!(text.contains("enable_toolset"), "{}", prompt.name);
        }
        let release = render_prepare_release(&args(&[("app_id", "1")]));
        assert!(release.contains("platform IOS"));
        let reviews = render_triage_reviews(&args(&[("app_id", "1")]));
        assert!(reviews.contains("last 30 days"));
        assert!(!reviews.contains("filter.territory"));
    }

    #[test]
    fn render_with_optional_args() {
        let release = render_prepare_release(&args(&[
            ("app_id", "1"),
            ("platform", "MAC_OS"),
            ("version", "2.4.0"),
        ]));
        assert!(release.contains("version 2.4.0"));
        assert!(release.contains("platform MAC_OS"));
        assert!(release.contains("explicit confirmation"));

        let reviews = render_triage_reviews(&args(&[
            ("app_id", "1"),
            ("days", "7"),
            ("territory", "DEU"),
        ]));
        assert!(reviews.contains("last 7 days"));
        assert!(reviews.contains("filter.territory DEU"));
        assert!(reviews.contains("approve"));

        let tf = render_testflight_distribution(&args(&[
            ("app_id", "1"),
            ("build", "42"),
            ("groups", "QA, Friends"),
        ]));
        assert!(tf.contains("filter.version 42"));
        assert!(tf.contains("QA, Friends"));
    }

    #[test]
    fn missing_required_arg_is_visible() {
        // Сервер отклоняет вызов без обязательного аргумента; если рендер всё же вызван,
        // текст не должен молча подставить пустое значение.
        let text = render_triage_reviews(&BTreeMap::new());
        assert!(text.contains("<ask the user>"));
        let blank = render_prepare_release(&args(&[("app_id", "  ")]));
        assert!(blank.contains("<ask the user>"));
    }
}
