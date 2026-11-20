//! Обработчик MCP-сервера `rmcp` без привязки к транспорту.
//!
//! Набор инструментов строится из таблицы операций, поэтому `ServerHandler` реализован
//! вручную, без макросов `#[tool]`. Транспорт подключает бинарник:
//! `AppStoreServer::new(...)?.serve(stdio())`.
//!
//! JWT для каждого запроса выпускает клиент API ([`ApiClient`]) из ключа профиля; ядро ключа
//! не видит. Составные инструменты получают общий [`Context`]: тот же клиент API, клиент
//! внешних адресов, политику файлов и хранилище секретов.
//!
//! Постобработка ответа у всех инструментов одна ([`AppStoreServer::finish`]): вырезание
//! учётных данных и лишнего содержимого ([`crate::redact`]) → `flatten` → фильтр `jq` →
//! обёртка с постраничной выдачей и пометкой о вырезанном → лимит размера. Фильтр получает
//! только очищенный ответ, поэтому не может раскрыть вырезанные значения. JWT клиента
//! вырезается ещё и из итогового текста, а тексты ошибок проходят [`redact_message`].
//!
//! Состояние сессии — текущий реестр инструментов: в режиме динамических наборов
//! (`ServerOptions::dynamic_toolsets`) `enable_toolset`/`disable_toolset` подменяют его снимок,
//! и клиент получает `notifications/tools/list_changed`: в сессии с `initialize` — сразу после
//! вызова, в протоколе 2026-07-28 — через подписку `subscriptions/listen`. Клоны сервера делят
//! сессию; stdio-сервер обслуживает одну сессию.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use appstore_connect_api::auth::KeyringVault;
use appstore_connect_api::executor::{ApiClient, ApiError, ApiRequest, ApiResponse, ResponseBody};
use appstore_connect_api::external::ExternalClient;
use appstore_connect_api::spec::{AccessLevel, HttpMethod, OperationDef};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, GetPromptRequestParams,
    GetPromptResponse, GetPromptResult, Implementation, JsonObject, ListPromptsResult,
    ListToolsResult, PaginatedRequestParams, Prompt, PromptArgument, PromptMessage,
    PromptsCapability, ProtocolVersion, Role, ServerCapabilities, ServerConfig, SubscriptionFilter,
    Tool,
};
use rmcp::service::{RequestContext, SubscriptionContext};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Map, Value};

use crate::ServerOptions;
use crate::api_request::{self, API_REQUEST_TOOL};
use crate::api_search;
use crate::composite::{Context, ProfileInfo, SecretVault};
use crate::dynamic_toolsets;
use crate::error::ToolError;
use crate::files::FilePolicy;
use crate::flatten::flatten;
use crate::jq::{JqFilter, take_jq, takes_jq};
use crate::prompts::PromptDef;
use crate::redact::{Mode, Redactions, redact_api_error, redact_json, redact_message, redact_text};
use crate::registry::{ToolKind, ToolRegistry, access_name};
use crate::request::build_request;
use crate::response::{envelope, failure, response_text, success, wrap};
use crate::validate::check_arguments;

/// Имя сервера в `initialize`.
pub const SERVER_NAME: &str = "appstore-connect-mcp";

/// Сколько байт ответа читается для постобработки. Лимит ответа сервера
/// (`max_response_bytes`) применяется к итоговому тексту: `flatten`, вырезание содержимого
/// сертификатов и фильтр `jq` заметно уменьшают ответ.
pub const PROCESSING_INPUT_LIMIT: usize = 16 * 1024 * 1024;

/// Клиенты и хранилища, с которыми работает сервер.
pub struct Services {
    /// Клиент App Store Connect API с JWT профиля.
    pub api: ApiClient,
    /// Клиент адресов, выданных API (загрузка файлов, скачивание отчётов).
    pub external: ExternalClient,
    /// Хранилище секретов, которые создают составные инструменты.
    pub vault: Arc<dyn SecretVault>,
}

/// Секреты составных инструментов в системном keyring (отдельный сервис, записи профиля).
impl SecretVault for KeyringVault {
    fn store(&self, account: &str, value: &str) -> Result<(), ToolError> {
        KeyringVault::store(self, account, value).map_err(|err| {
            ToolError::new(format!(
                "cannot store the secret in the system keychain: {err}"
            ))
        })
    }

    fn contains(&self, account: &str) -> Result<bool, ToolError> {
        KeyringVault::contains(self, account)
            .map_err(|err| ToolError::new(format!("cannot read the system keychain: {err}")))
    }
}

/// MCP-сервер App Store Connect. Дёшево клонируется; клоны делят сессию.
#[derive(Clone)]
pub struct AppStoreServer {
    inner: Arc<Inner>,
    session: Arc<Session>,
}

impl std::fmt::Debug for AppStoreServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppStoreServer")
            .field("tools", &self.registry().len())
            .finish_non_exhaustive()
    }
}

struct Inner {
    context: Arc<Context>,
    options: ServerOptions,
    files: FilePolicy,
    instructions: String,
}

/// Изменяемое состояние MCP-сессии.
struct Session {
    registry: RwLock<Arc<ToolRegistry>>,
    /// Счётчик изменений списка инструментов: по нему подписки `subscriptions/listen` узнают,
    /// что пора отправить `notifications/tools/list_changed`.
    changes: tokio::sync::watch::Sender<u64>,
}

impl Session {
    fn new(registry: Arc<ToolRegistry>) -> Self {
        Self {
            registry: RwLock::new(registry),
            changes: tokio::sync::watch::Sender::new(0),
        }
    }

    fn registry(&self) -> Arc<ToolRegistry> {
        // Отравленная блокировка не мешает: внутри только `Arc`, его замена атомарна.
        self.registry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn generation(&self) -> u64 {
        *self.changes.borrow()
    }
}

impl AppStoreServer {
    /// Создаёт сервер. Ошибка — неизвестный набор инструментов в `options.toolsets`,
    /// некорректные составные инструменты или конфликт имён пользовательских инструментов.
    ///
    /// Без [`ServerOptions::local_files`] политика файлов принудительно
    /// [`FilePolicy::disabled`], какой бы ни передали.
    pub fn new(
        services: Services,
        options: ServerOptions,
        files: FilePolicy,
    ) -> Result<Self, String> {
        let files = if options.local_files {
            files
        } else {
            FilePolicy::disabled()
        };
        check_prompts(&options.prompts)?;
        let registry = Arc::new(ToolRegistry::new(&options)?);
        let instructions = server_instructions(&options, &registry, &files);
        let context = Arc::new(Context {
            api: Arc::new(services.api),
            external: Arc::new(services.external),
            files: Arc::new(files.clone()),
            vault: services.vault,
            profile: ProfileInfo {
                name: options.profile.clone().unwrap_or_default(),
                vendor_number: options.vendor_number.clone(),
            },
            access: options.max_access,
        });
        Ok(Self {
            inner: Arc::new(Inner {
                context,
                options,
                files,
                instructions,
            }),
            session: Arc::new(Session::new(registry)),
        })
    }

    /// Текущий реестр инструментов сессии (снимок).
    pub fn registry(&self) -> Arc<ToolRegistry> {
        self.session.registry()
    }

    pub fn instructions(&self) -> &str {
        &self.inner.instructions
    }

    pub fn options(&self) -> &ServerOptions {
        &self.inner.options
    }

    fn client(&self) -> &ApiClient {
        &self.inner.context.api
    }

    /// Вызов инструмента без транспорта. Ошибки возвращаются как `isError: true`;
    /// `Err` — только для неизвестного инструмента (ошибка протокола).
    pub async fn call(&self, name: &str, args: JsonObject) -> Result<CallToolResult, ErrorData> {
        let registry = self.registry();
        let Some(tool) = registry.get(name) else {
            // Известный, но выключенный инструмент: агенту объясняем, какой флаг нужен.
            return match registry.unavailable_reason(name) {
                Some(reason) => Ok(failure(&ToolError::new(reason))),
                None => Err(ErrorData::invalid_params(
                    format!("unknown tool `{name}`"),
                    None,
                )),
            };
        };
        let result = match tool.kind {
            ToolKind::Operation(op) => self.call_operation(op, &tool.tool, args).await,
            ToolKind::Composite(index) => self.call_composite(index, &tool.tool, args).await,
            ToolKind::Custom(index) => self.call_custom(index, args).await,
            ToolKind::ApiRequest => self.call_api_request(args).await,
            ToolKind::ApiSearch => api_search::search(&args, &registry),
            ToolKind::ListToolsets => dynamic_toolsets::list(&args, &registry),
            ToolKind::EnableToolset => self.toggle_toolsets(&args, true),
            ToolKind::DisableToolset => self.toggle_toolsets(&args, false),
        };
        // Защита в глубину: JWT не попадает в ответ агенту, даже если API его отразил.
        let client = self.client();
        Ok(match result {
            Ok(text) => success(client.scrub_token(text)),
            Err(err) => {
                // Тексты ошибок (в том числе составных инструментов и локальные) проходят
                // проверку значений по форме: в них бывают фрагменты ответов и адреса.
                let err = ToolError::new(redact_message(
                    &client.scrub_token(err.message().to_owned()),
                ));
                tracing::debug!(tool = name, error = %err, "инструмент завершился ошибкой");
                failure(&err)
            }
        })
    }

    /// `enable_toolset`/`disable_toolset`: пересобирает реестр сессии. Замена делается под
    /// блокировкой записи от текущего реестра, чтобы параллельные вызовы не потеряли изменения.
    fn toggle_toolsets(&self, args: &JsonObject, enable: bool) -> Result<String, ToolError> {
        let mut guard = self
            .session
            .registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let toggled = dynamic_toolsets::toggle(&guard, args, enable)?;
        if let Some(next) = toggled.registry {
            tracing::info!(
                toolsets = ?next.enabled_toolsets(),
                tools = next.len(),
                "наборы инструментов изменены"
            );
            *guard = Arc::new(next);
            drop(guard);
            self.session
                .changes
                .send_modify(|generation| *generation += 1);
        }
        Ok(toggled.text)
    }

    async fn call_operation(
        &self,
        op: &'static OperationDef,
        tool: &Tool,
        mut args: JsonObject,
    ) -> Result<String, ToolError> {
        // У операций с двоичным ответом аргумента `jq` нет: его отклонит `build_request`.
        let jq = if takes_jq(op) {
            take_jq(&mut args)?
        } else {
            None
        };
        self.run_operation(op, &tool.input_schema, args, jq).await
    }

    async fn run_operation(
        &self,
        op: &'static OperationDef,
        schema: &JsonObject,
        args: JsonObject,
        jq: Option<JqFilter>,
    ) -> Result<String, ToolError> {
        let call = build_request(op, schema, &args, &self.inner.files)?;
        tracing::debug!(tool = op.tool_name, path = %call.request.path, "вызов операции App Store Connect API");
        self.execute(call.request, call.flatten, jq.as_ref()).await
    }

    /// Пользовательский инструмент: фиксированные аргументы и фильтр определения, дальше —
    /// тот же путь, что у инструмента его операции (со схемой операции).
    async fn call_custom(&self, index: usize, args: JsonObject) -> Result<String, ToolError> {
        let Some(tool) = self.inner.options.custom_tools.get(index) else {
            return Err(ToolError::new("custom tool is not available"));
        };
        let (args, jq) = tool.build_call(args)?;
        let op = tool.operation;
        let schema: JsonObject = serde_json::from_str(op.input_schema).map_err(|err| {
            ToolError::new(format!("{}: invalid input schema: {err}", op.tool_name))
        })?;
        tracing::debug!(tool = %tool.name, operation = op.tool_name, "пользовательский инструмент");
        self.run_operation(op, &schema, args, jq).await
    }

    /// `api_request`: уровень доступа и файлы проверяет [`api_request::prepare`], дальше —
    /// тот же путь, что у инструментов операций.
    async fn call_api_request(&self, mut args: JsonObject) -> Result<String, ToolError> {
        let inner = &self.inner;
        let jq = take_jq(&mut args)?;
        let prepared =
            api_request::prepare(&args, self.client(), inner.options.max_access, &inner.files)?;
        if jq.is_some() && (prepared.request.save_to.is_some() || prepared.json_save_to.is_some()) {
            return Err(ToolError::new(
                "`jq` cannot be combined with `save_to`: a saved response is not filtered",
            ));
        }
        tracing::debug!(
            tool = API_REQUEST_TOOL,
            method = prepared.request.method.as_str(),
            path = %prepared.request.path,
            access = prepared.access.as_str(),
            "произвольный запрос к App Store Connect API"
        );
        match prepared.json_save_to {
            None => self.execute(prepared.request, false, jq.as_ref()).await,
            Some(target) => self.execute_to_file(prepared.request, &target).await,
        }
    }

    /// Запрос с JSON-ответом, который сохраняется в новый файл `target`; агент получает путь,
    /// размер и постраничную выдачу.
    async fn execute_to_file(
        &self,
        request: ApiRequest,
        target: &std::path::Path,
    ) -> Result<String, ToolError> {
        let mut response = self
            .client()
            .execute_with_limit(request, PROCESSING_INPUT_LIMIT)
            .await
            .map_err(api_error)?;
        // В файл попадает полный ответ (содержимое сертификатов и профилей остаётся), но без
        // учётных данных: агент может прочитать файл.
        let (text, redactions) = match &mut response.body {
            ResponseBody::Json(value) => {
                let redactions = redact_json(value, Mode::File);
                let text =
                    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
                (text, redactions)
            }
            ResponseBody::Text(text) => redact_text(text),
            ResponseBody::Empty => (String::new(), Redactions::default()),
            ResponseBody::Binary { .. } | ResponseBody::SavedToFile { .. } => {
                return Ok(response_text(&response));
            }
        };
        let text = self.client().scrub_token(text);
        let bytes = api_request::write_json_file(target, &text).await?;
        let result = serde_json::json!({
            "saved_to": target.display().to_string(),
            "bytes": bytes,
            "status": response.status,
        });
        let extra = with_note(
            envelope(&response.pagination, response.rate_limit.as_ref()),
            &redactions,
        );
        Ok(wrap(result, extra).to_string())
    }

    async fn call_composite(
        &self,
        index: usize,
        tool: &Tool,
        mut args: JsonObject,
    ) -> Result<String, ToolError> {
        let Some(composite) = self.inner.options.composite_tools.get(index) else {
            return Err(ToolError::new("composite tool is not available"));
        };
        let jq = take_jq(&mut args)?;
        check_arguments(&Value::Object((*tool.input_schema).clone()), &mut args)?;
        tracing::debug!(tool = composite.name, "составной инструмент");
        let mut value = (composite.run)(&self.inner.context, Value::Object(args)).await?;
        let redactions = redact_json(&mut value, Mode::Response);
        let changed = composite.access != AccessLevel::Read;
        self.finish(
            value,
            with_note(None, &redactions),
            changed.then_some(200),
            jq.as_ref(),
        )
        .await
    }

    /// Выполняет запрос и превращает ответ в текст для агента.
    pub(crate) async fn execute(
        &self,
        request: ApiRequest,
        flatten_body: bool,
        jq: Option<&JqFilter>,
    ) -> Result<String, ToolError> {
        let changes = request.method != HttpMethod::Get;
        let max = self.client().config().max_response_bytes;
        let response = self
            .client()
            .execute_with_limit(request, PROCESSING_INPUT_LIMIT.max(max))
            .await
            .map_err(api_error)?;
        let status = changes.then_some(response.status);
        match response {
            ApiResponse {
                body: ResponseBody::Json(mut value),
                pagination,
                rate_limit,
                ..
            } => {
                let redactions = redact_json(&mut value, Mode::Response);
                if flatten_body {
                    flatten(&mut value);
                }
                self.finish(
                    value,
                    with_note(envelope(&pagination, rate_limit.as_ref()), &redactions),
                    status,
                    jq,
                )
                .await
            }
            mut other => {
                let mut redactions = Redactions::default();
                if let ResponseBody::Text(text) = &mut other.body {
                    let (redacted, found) = redact_text(text);
                    *text = redacted;
                    redactions = found;
                }
                let mut text = response_text(&other);
                check_size(&text, max, status)?;
                if let Some(note) = redactions.note() {
                    text.push_str("\n\n");
                    text.push_str(&note);
                }
                if jq.is_some() {
                    text.push_str(
                        "\n\n[The `jq` filter was not applied: the response is not JSON.]",
                    );
                }
                Ok(text)
            }
        }
    }

    /// Итоговый текст JSON-результата: фильтр `jq`, обёртка (постраничная выдача, квота) и
    /// лимит размера. `status` — статус изменяющего запроса: агенту говорится, что изменение
    /// выполнено.
    ///
    /// Фильтр получает тело ответа без обёртки, поэтому одинаково работает на любой странице;
    /// постраничная выдача остаётся рядом с его результатом.
    async fn finish(
        &self,
        mut value: Value,
        extra: Option<Map<String, Value>>,
        status: Option<u16>,
        jq: Option<&JqFilter>,
    ) -> Result<String, ToolError> {
        let max = self.client().config().max_response_bytes;
        if let Some(filter) = jq {
            // JWT вырезается до фильтра: итоговая `scrub_token` ищет его только в исходном
            // виде, а фильтр мог бы его преобразовать.
            scrub_token_json(self.client(), &mut value);
            let failed = |problem: String| match status {
                Some(status) => ToolError::new(format!(
                    "the API request succeeded (HTTP {status}), but {problem}. Do not repeat the \
                     request: it changed data, and repeating it would apply the change again"
                )),
                None => ToolError::new(problem),
            };
            let output = self
                .inner
                .options
                .jq
                .run(filter, value, max)
                .await
                .map_err(|err| failed(format!("the `jq` filter failed: {err}. Fix the filter")))?;
            if output.truncated {
                return Err(failed(format!(
                    "the filtered result is larger than {max} bytes; select fewer fields or \
                     items with the filter, or narrow the request"
                )));
            }
            value = serde_json::from_str(&output.text)
                .map_err(|err| failed(format!("the `jq` filter returned invalid JSON: {err}")))?;
        }
        let text = wrap(value, extra).to_string();
        check_size(&text, max, status)?;
        Ok(text)
    }

    fn prompt(&self, name: &str) -> Option<&PromptDef> {
        self.inner.options.prompts.iter().find(|p| p.name == name)
    }
}

/// Ошибка API для агента: тело и ошибки JSON:API без учётных данных, с пометкой.
fn api_error(err: ApiError) -> ToolError {
    let (err, redactions) = redact_api_error(err);
    let mut message = ToolError::from(err).message().to_owned();
    if let Some(note) = redactions.note() {
        message.push_str("\n\n");
        message.push_str(&note);
    }
    ToolError::new(message)
}

/// Добавляет к обёртке ответа пометку `note` о вырезанных значениях.
fn with_note(
    extra: Option<Map<String, Value>>,
    redactions: &Redactions,
) -> Option<Map<String, Value>> {
    let Some(note) = redactions.note() else {
        return extra;
    };
    let mut extra = extra.unwrap_or_default();
    extra.insert("note".to_owned(), Value::String(note));
    Some(extra)
}

/// Заменяет JWT клиента во всех строках и ключах JSON до фильтра `jq`.
fn scrub_token_json(client: &ApiClient, value: &mut Value) {
    match value {
        Value::String(text) => {
            let scrubbed = client.scrub_token(std::mem::take(text));
            *text = scrubbed;
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| scrub_token_json(client, item)),
        Value::Object(map) => {
            let entries = std::mem::take(map);
            for (key, mut item) in entries {
                scrub_token_json(client, &mut item);
                map.insert(client.scrub_token(key), item);
            }
        }
        _ => {}
    }
}

/// Проверяет итоговый размер ответа.
fn check_size(text: &str, max: usize, status: Option<u16>) -> Result<(), ToolError> {
    if text.len() <= max {
        return Ok(());
    }
    let narrow = "Narrow the request: ask only for the fields you need (`fields.<type>`), use a \
                  smaller page (`limit`), fewer `include` relationships or stricter `filter.*` \
                  arguments, or select less with `jq`";
    Err(ToolError::new(match status {
        Some(status) => format!(
            "the API request succeeded (HTTP {status}), but the response is larger than {max} \
             bytes and was not returned. Do not repeat the request: it changed data, and \
             repeating it would apply the change again; read the result with a narrower read \
             request instead. {narrow}"
        ),
        None => format!("the response is larger than {max} bytes and was not returned. {narrow}"),
    }))
}

/// Проверяет промпты: имена уникальны.
fn check_prompts(prompts: &[PromptDef]) -> Result<(), String> {
    for (index, prompt) in prompts.iter().enumerate() {
        if prompts[..index]
            .iter()
            .any(|other| other.name == prompt.name)
        {
            return Err(format!("prompt `{}` is defined twice", prompt.name));
        }
    }
    Ok(())
}

fn prompt_info(prompt: &PromptDef) -> Prompt {
    let arguments = prompt
        .arguments
        .iter()
        .map(|arg| {
            PromptArgument::new(arg.name)
                .with_description(arg.description)
                .with_required(arg.required)
        })
        .collect();
    let mut info = Prompt::new(prompt.name, Some(prompt.description), Some(arguments));
    if !prompt.title.is_empty() {
        info = info.with_title(prompt.title);
    }
    info
}

/// Аргументы промпта строками: неизвестные и недостающие обязательные отклоняются.
fn prompt_arguments(
    prompt: &PromptDef,
    args: Option<JsonObject>,
) -> Result<BTreeMap<String, String>, ErrorData> {
    let mut values = BTreeMap::new();
    for (name, value) in args.unwrap_or_default() {
        if !prompt.arguments.iter().any(|arg| arg.name == name) {
            let allowed: Vec<String> = prompt
                .arguments
                .iter()
                .map(|arg| format!("`{}`", arg.name))
                .collect();
            return Err(ErrorData::invalid_params(
                format!(
                    "prompt `{}` has no argument `{name}`; its arguments: {}",
                    prompt.name,
                    if allowed.is_empty() {
                        "none".to_owned()
                    } else {
                        allowed.join(", ")
                    }
                ),
                None,
            ));
        }
        let text = match value {
            Value::Null => continue,
            Value::String(text) => text,
            other => other.to_string(),
        };
        if !text.trim().is_empty() {
            values.insert(name, text);
        }
    }
    let missing: Vec<String> = prompt
        .arguments
        .iter()
        .filter(|arg| arg.required && !values.contains_key(arg.name))
        .map(|arg| format!("`{}`", arg.name))
        .collect();
    if !missing.is_empty() {
        return Err(ErrorData::invalid_params(
            format!(
                "prompt `{}` needs argument(s): {}",
                prompt.name,
                missing.join(", ")
            ),
            None,
        ));
    }
    Ok(values)
}

impl ServerHandler for AppStoreServer {
    fn get_info(&self) -> ServerConfig {
        let mut capabilities = if self.inner.options.dynamic_toolsets {
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build()
        } else {
            ServerCapabilities::builder().enable_tools().build()
        };
        if !self.inner.options.prompts.is_empty() {
            capabilities.prompts = Some(PromptsCapability::default());
        }
        ServerConfig::new(capabilities)
            .with_server_info(
                Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION"))
                    .with_title("App Store Connect"),
            )
            .with_instructions(self.inner.instructions.clone())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.registry().tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        let before = self.session.generation();
        let result = self.call(&request.name, args).await;
        // В сессии с `initialize` уведомление уходит сразу; в протоколе 2026-07-28 клиент
        // получает его через подписку (`listen`), а незапрошенные уведомления отклоняет.
        if self.session.generation() != before
            && uses_legacy_session(&context)
            && let Err(err) = context.peer.notify_tool_list_changed().await
        {
            tracing::warn!(error = %err, "не удалось отправить tools/list_changed");
        }
        result.map(Into::into)
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        // Подписка нужна только для смены списка инструментов в динамическом режиме.
        self.inner
            .options
            .dynamic_toolsets
            .then(|| requested.supported_by(&self.get_info().capabilities))
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        if context.accepted().tools_list_changed != Some(true) {
            context.cancelled().await;
            return Ok(());
        }
        let mut changes = self.session.changes.subscribe();
        changes.mark_unchanged();
        loop {
            tokio::select! {
                () = context.cancelled() => return Ok(()),
                changed = changes.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    changes.mark_unchanged();
                    if let Err(err) = context.sink().notify_tool_list_changed().await {
                        tracing::debug!(error = %err, "подписка на tools/list_changed закрыта");
                        return Ok(());
                    }
                }
            }
        }
    }

    fn get_tool(&self, _name: &str) -> Option<Tool> {
        // Аргументы проверяют `build_request` и `check_arguments` с понятными агенту
        // сообщениями; проверку схемы на стороне rmcp не включаем.
        None
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(
            self.inner.options.prompts.iter().map(prompt_info).collect(),
        ))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        let Some(prompt) = self.prompt(&request.name) else {
            return Err(ErrorData::invalid_params(
                format!("unknown prompt `{}`", request.name),
                None,
            ));
        };
        let values = prompt_arguments(prompt, request.arguments)?;
        let text = (prompt.render)(&values);
        let mut result = GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)]);
        result.description = Some(prompt.description.to_owned());
        Ok(result.into())
    }
}

/// Сессия с `initialize` (протокол до 2026-07-28): уведомления отправляются без подписки.
fn uses_legacy_session(context: &RequestContext<RoleServer>) -> bool {
    // Версии `YYYY-MM-DD` сравниваются лексикографически так же, как по времени.
    context
        .protocol_version()
        .is_none_or(|version| version.as_str() < ProtocolVersion::V_2026_07_28.as_str())
}

/// Инструкции сервера для агента (поле `instructions` в `initialize`).
pub fn server_instructions(
    options: &ServerOptions,
    registry: &ToolRegistry,
    files: &FilePolicy,
) -> String {
    let access = match options.max_access {
        AccessLevel::Read => {
            "This server is read-only: only tools that read data are available. Tools that \
             create or change data (versions, metadata, TestFlight groups and testers, review \
             submissions, prices) are enabled when the user allows `write` access for the \
             server's profile, and tools that delete data with `destructive` access (the user \
             does it with `appstore-connect-mcp ui`, then restarts the client). If a task needs \
             them, ask the user instead of looking for workarounds."
        }
        AccessLevel::Write => {
            "This server can read, create and change data, but not delete it: deleting tools \
             need `destructive` access for the server's profile (`appstore-connect-mcp ui`). \
             Changes are visible \
             in App Store Connect right away, and submitting for review or releasing affects \
             real users: confirm such changes with the user when they were not explicitly \
             requested."
        }
        AccessLevel::Destructive => {
            "This server can read, change and delete data. Deleting is irreversible, and \
             revoking certificates or deleting apps' resources can break builds and releases: \
             confirm with the user before calling destructive tools unless they explicitly \
             asked for it."
        }
    };
    let profile = options.profile.as_deref().unwrap_or("<profile>");
    let account = match options.profile.as_deref() {
        Some(name) => format!(
            "This server works with one App Store Connect team: the profile `{name}` (its API \
             key). For another team the user starts another server with another profile."
        ),
        None => "This server works with one App Store Connect team (the API key of its \
                 profile)."
            .to_owned(),
    };
    let files_line = if files.is_disabled() {
        "- Local files: local files are disabled on this server, so tools that read or write \
         files are not registered.\n"
            .to_owned()
    } else {
        format!(
            "- Local files: file arguments (`save_to` for downloads, and the files of upload \
             tools) take a path (absolute, or relative to the server's working directory) \
             inside the allowed directories: {roots}. Other paths and symbolic links leading \
             outside are rejected; the user can allow more with --allow-path. Downloads never \
             overwrite existing files. Binary responses (sales and finance reports) need \
             `save_to`.\n",
            roots = files.describe_roots(),
        )
    };
    let enabled = registry.enabled_toolsets();
    let enabled_list = if enabled.is_empty() {
        "none".to_owned()
    } else {
        enabled.join(", ")
    };
    let toolsets = if registry.is_dynamic() {
        format!(
            "Toolsets are loaded on demand; enabled at start: {enabled_list}. Tools of other \
             toolsets are not registered until you enable them, and tools above the access \
             level are never registered; calling them explains what to do."
        )
    } else {
        format!(
            "Enabled toolsets: {enabled_list}; tools of other toolsets (and above the access \
             level) are not registered, and calling them explains which flag enables them."
        )
    };
    let dynamic = if registry.is_dynamic() {
        "- Toolsets: tools are loaded on demand, by toolset. Call `list_toolsets` to see the \
         toolsets, then `enable_toolset` to load the tools you need (`disable_toolset` unloads \
         them); `api_search` shows the toolset of an operation. The client is notified that the \
         tool list changed; if the enabled tools do not appear in your tool list, call the same \
         operations with `api_request`.\n"
    } else {
        ""
    };
    let custom = if registry
        .iter()
        .any(|tool| matches!(tool.kind, ToolKind::Custom(_)))
    {
        "- Custom tools: some tools are defined by the user (their description says so): each \
         calls one API operation with arguments fixed by the user and an optional built-in \
         filter. Prefer them when they fit the task; fixed arguments cannot be passed.\n"
    } else {
        ""
    };
    format!(
        "App Store Connect MCP server ({level} access). Every operation tool calls one App Store \
         Connect API operation and returns its JSON:API response. {account} {toolsets}\n\
         \n\
         - Access: {access}\n\
         - Arguments: path and query parameters are plain arguments; JSON:API query \
         parameters use dots instead of brackets: `filter.bundleId` for `filter[bundleId]`, \
         `fields.apps` for `fields[apps]`, `limit.builds` for `limit[builds]`. Arrays \
         (`include`, `fields.*`, `filter.*`) are JSON arrays. Unknown arguments are rejected. \
         A JSON request body goes in the `body` argument as a JSON object \
         (`{{\"data\": {{\"type\": ..., \"attributes\": {{...}}, \"relationships\": {{...}}}}}}`).\n\
         - JSON:API: `include` adds related resources to `included`; pass `flatten: true` to \
         get them in place of the `{{type, id}}` references. `fields.<type>` limits the \
         attributes per resource type, `filter.*` filters lists, `limit` sets the page size \
         (max 200 for most lists), `sort` sorts (`-` for descending).\n\
         - Pagination: when there are more results, the response is wrapped as \
         `{{\"result\": <response>, \"pagination\": {{\"next\": <URL>, \"total\": <count>, \
         \"limit\": <page size>}}}}`. To get the next page, call `api_request` with `path` set \
         to `pagination.next` (the full URL); repeat until `next` is missing. When the hourly \
         request quota is almost used up, the wrapper also has `rate_limit` (`remaining`, \
         `limit`): slow down then. When values were hidden (see Secrets), the wrapper has a \
         `note` that says what and why.\n\
         - Finding operations: `api_search` searches all App Store Connect API operations by \
         words in English or Russian (tool names, summaries, paths), including tools not \
         enabled here and operations without a tool, and tells which flag or toolset enables \
         them; with `tool_name` it shows the full schema of one tool. Use it when no listed \
         tool fits. `api_request` calls any API path (`/v1/...`, or a `links.next` URL) within \
         the server's access level; toolsets do not limit it. Prefer the dedicated tool when it \
         is registered: it validates arguments and documents the schema.\n\
         {dynamic}\
         {custom}\
         - Filtering: operation tools with a JSON response, composite tools, custom tools and \
         `api_request` accept an optional `jq` argument, a jq filter (jaq dialect) applied to \
         the JSON response body, for example `[.data[] | {{id, name: .attributes.name}}]` or \
         `[.data[] | select(.attributes.processingState == \"VALID\") | .id]`. Use it to keep \
         large responses small. It runs after `flatten`, so with `flatten: true` filter the \
         flat objects (`[.data[] | {{id, version}}]`). Pagination stays outside the filter: a \
         paginated result is still wrapped as `{{\"result\": <filter output>, \"pagination\": \
         {{...}}}}`. The size limit applies to the filtered result. Several outputs are \
         returned as a JSON array, no output as `[]`. Filters cannot read the environment, \
         files or other inputs. A broken filter is reported before the request is sent; a \
         filter that fails at runtime (after the request succeeded) returns an error that says \
         so: do not repeat a request that changed data just to fix the filter. `jq` cannot be \
         combined with `save_to`.\n\
         - Large responses are rejected with an error; narrow the request (`fields.<type>`, a \
         smaller `limit`, fewer `include`, stricter `filter.*`), select less with `jq`, or save \
         the response to a file with `api_request` and `save_to`.\n\
         {files_line}\
         - Secrets: the App Store Connect API key (a `.p8` private key) is stored in the system \
         keychain and used only by the server to sign short-lived tokens; it is never passed \
         in tool arguments. Never ask the user to paste keys, passwords or other secrets into \
         the conversation. If the API rejects the key (HTTP 401), ask the user to run \
         `appstore-connect-mcp auth login --profile {profile}` themselves. Values that look like \
         credentials are replaced with \"[redacted]\" in every response, including errors, \
         and a `note` says when that happened: fields such as `demoAccountPassword`, webhook \
         `secret` and other password, token or key fields, and token-, key- or password-shaped \
         values inside any text (JWTs, `Bearer`/`Basic` values, private keys, passwords and \
         signatures in URLs). Do not try to recover hidden values. Signed upload URLs \
         (`uploadOperations`) are hidden: upload files with `asset_upload` or `build_upload`. \
         Signed download URLs of analytics report segments are hidden: use `report_download`. \
         Large `certificateContent` and `profileContent` (base64) are omitted from responses: \
         use `profile_download` for a provisioning profile, or `api_request` with `save_to` to \
         save the full response (credentials are removed from saved files too). When you set \
         a review demo account password, take it from the user only for that request, and do \
         not repeat it in messages, files or commits.",
        level = access_name(options.max_access),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instructions(options: &ServerOptions, files: &FilePolicy) -> String {
        let registry = ToolRegistry::new(options).unwrap();
        server_instructions(options, &registry, files)
    }

    #[test]
    fn instructions_depend_on_access() {
        let files = FilePolicy::new(&[]).unwrap();
        let read = instructions(&ServerOptions::default(), &files);
        assert!(read.contains("(read-only access)"), "{read}");
        assert!(read.contains("`write` access") && read.contains("`destructive` access"));
        assert!(read.contains("allowed directories: none"), "{read}");
        assert!(
            read.contains("Enabled toolsets: apps, versions-review, builds, testflight;"),
            "{read}"
        );
        assert!(read.contains("auth login --profile <profile>"), "{read}");
        assert!(read.contains("\"pagination\""), "{read}");
        assert!(read.contains("`filter.bundleId`") && read.contains("`flatten: true`"));
        assert!(read.contains("Never ask the user to paste keys"), "{read}");
        assert!(
            read.contains("- Filtering:") && read.contains("`jq`"),
            "{read}"
        );
        assert!(!read.contains("- Toolsets:") && !read.contains("- Custom tools:"));
        assert!(read.contains("\"[redacted]\"") && read.contains("`demoAccountPassword`"));
        assert!(read.contains("`uploadOperations`") && read.contains("`profile_download`"));

        let write = instructions(
            &ServerOptions {
                max_access: AccessLevel::Write,
                toolsets: Some(vec!["analytics-reports".into()]),
                profile: Some("work".into()),
                ..ServerOptions::default()
            },
            &files,
        );
        assert!(write.contains("(write access)"), "{write}");
        assert!(
            write.contains("Enabled toolsets: analytics-reports;"),
            "{write}"
        );
        assert!(write.contains("auth login --profile work"), "{write}");
        assert!(write.contains("the profile `work`"), "{write}");

        let all = instructions(
            &ServerOptions {
                max_access: AccessLevel::Destructive,
                local_files: false,
                ..ServerOptions::default()
            },
            &FilePolicy::disabled(),
        );
        assert!(all.contains("(destructive access)"), "{all}");
        assert!(all.contains("irreversible"), "{all}");
        assert!(all.contains("local files are disabled"), "{all}");
    }

    #[test]
    fn size_limit_mentions_completed_changes() {
        assert!(check_size("abc", 3, None).is_ok());
        let err = check_size("abcd", 3, None).unwrap_err();
        assert!(err.message().contains("larger than 3 bytes"), "{err}");
        let err = check_size("abcd", 3, Some(201)).unwrap_err();
        assert!(err.message().contains("succeeded (HTTP 201)"), "{err}");
    }
}
