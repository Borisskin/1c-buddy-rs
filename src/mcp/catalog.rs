//! Single source of public tool definitions.

use std::fmt;
use std::sync::Arc;

use rmcp::model::Tool;
use serde_json::{Map, Value, json};

use crate::config::{CallMode, Config};

const ASK_DESCRIPTION: &str = "Задать общий вопрос по платформе 1С:Предприятие и получить ответ, \
объяснение или практическую рекомендацию. Используй для общих вопросов по функциональности \
платформы, подходам к разработке и типовым сценариям, когда не нужен отдельный \
специализированный поиск по документации или ИТС.";
const EXPLAIN_DESCRIPTION: &str = "Объяснить конкретный элемент синтаксиса, объект или тип \
платформы 1С с примерами использования. Используй, когда нужно понять, как работает конкретный \
метод, объект, коллекция или конструкция языка.";
const CHECK_DESCRIPTION: &str = "Проверить присланный BSL/1C код. Используй check_type='syntax' \
для быстрой синтаксической проверки конкретного фрагмента и check_type='review' для code review, \
поиска ошибок и замечаний по качеству кода. Проверка syntax выполняется без глобального \
контекста, поэтому возможны ложные срабатывания по необъявленным переменным и методам.";
const MODIFY_DESCRIPTION: &str = "Изменить код 1С по явному заданию пользователя: исправить \
ошибку, сделать рефакторинг или добавить функциональность. В instruction опиши, какие изменения \
нужны и что ожидается на выходе. Если есть исходный код, передай его в параметре code.";
const SEARCH_DOCS_DESCRIPTION: &str = "Поиск по документации платформы 1С:Предприятие. Используй, \
когда вопрос касается функциональности самой платформы: объектов, методов, свойств, синтаксиса \
и параметров, а также перед написанием кода, если нужна точная документация по элементу \
платформы. Не выдумывай синтаксис и поведение, если их можно сначала найти в документации. Для \
общих запросов формируй query так, чтобы он искал обзорную информацию: 'Общая информация о ...', \
'Список всех ...', 'Все ...'. Если пользователь указал версию платформы, обязательно передай её.";
const SEARCH_ITS_DESCRIPTION: &str = "Поиск по базе знаний ИТС. Используй для стандартов и правил \
разработки на 1С, методических материалов, практических примеров, вопросов по конкретным \
конфигурациям и продуктам 1С, а также по EDT и Конфигуратору. Для фактологических вопросов по \
экосистеме 1С предпочитай именно этот инструмент, а не ответ по памяти. Если найденной \
информации недостаточно, переформулируй query или затем используй fetch_its для чтения \
конкретного документа.";
const FETCH_ITS_DESCRIPTION: &str = "Получить содержимое документа, каталога или базы ИТС по id. \
Обычно используется после search_its, когда уже найден нужный документ, либо для исследования \
структуры ИТС с id='root'. Поддерживаются как специальные id вроде root, superior, v8std, так и \
идентификаторы документов и каталогов вида its-...-hdoc или its-...-hdir, возможно с 1-2 якорями \
через '/'. Обычно id документа выглядит как 'its-{database_id}-{doc_or_dir_id}-(hdoc|hdir|...)'.";
const DIFF_DESCRIPTION: &str = "Сравнить документацию платформы 1С между двумя версиями. \
Используй, когда спрашивают об изменениях между версиями платформы. version_a должна быть более \
ранней, version_b — более поздней. Параметр query задаёт предметную область сравнения. Если \
разница пустая, но вернулся список изменённых файлов, значит query нужно переформулировать.";

const SEARCH_DOCUMENTATION_ALIAS: &[&str] = &["Search_1C_Documentation"];
const SEARCH_ITS_ALIAS: &[&str] = &["Search_ITS"];
const FETCH_ITS_ALIAS: &[&str] = &["Fetch_ITS"];
const DIFF_ALIAS: &[&str] = &["Diff_1C_Documentation_Versions"];
const NO_ALIASES: &[&str] = &[];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogErrorKind {
    UnknownTool,
    InvalidArguments,
    LimitExceeded,
}

#[derive(Debug)]
pub struct CatalogError {
    kind: CatalogErrorKind,
    message: String,
}

impl CatalogError {
    fn new(kind: CatalogErrorKind) -> Self {
        let message = match kind {
            CatalogErrorKind::UnknownTool => "unknown tool",
            CatalogErrorKind::InvalidArguments => "tool arguments are invalid",
            CatalogErrorKind::LimitExceeded => "tool arguments exceed a configured limit",
        };
        Self {
            kind,
            message: message.to_owned(),
        }
    }

    fn with_message(kind: CatalogErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn kind(&self) -> CatalogErrorKind {
        self.kind
    }
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CatalogError {}

#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionRoute {
    Message,
    Exact {
        name: &'static str,
        arguments: Map<String, Value>,
    },
}

impl ExecutionRoute {
    #[must_use]
    pub fn exact_name(&self) -> Option<&'static str> {
        match self {
            Self::Message => None,
            Self::Exact { name, .. } => Some(name),
        }
    }

    #[must_use]
    pub fn exact_arguments(&self) -> Option<&Map<String, Value>> {
        match self {
            Self::Message => None,
            Self::Exact { arguments, .. } => Some(arguments),
        }
    }
}

#[derive(Debug)]
pub struct PreparedToolCall {
    canonical_name: &'static str,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "normalized arguments are retained for contract inspection"
        )
    )]
    arguments: Map<String, Value>,
    programming_language: String,
    instruction: String,
    route: ExecutionRoute,
}

impl PreparedToolCall {
    #[must_use]
    pub fn canonical_name(&self) -> &'static str {
        self.canonical_name
    }

    #[must_use]
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "normalized arguments are inspected by contract tests"
        )
    )]
    pub fn arguments(&self) -> &Map<String, Value> {
        &self.arguments
    }

    #[must_use]
    pub fn programming_language(&self) -> &str {
        &self.programming_language
    }

    #[must_use]
    pub fn instruction(&self) -> &str {
        &self.instruction
    }

    #[must_use]
    pub fn route(&self) -> &ExecutionRoute {
        &self.route
    }
}

#[derive(Clone, Copy)]
enum ToolKind {
    Ask,
    Explain,
    Check,
    Modify,
    SearchDocumentation,
    SearchIts,
    FetchIts,
    DiffDocumentation,
}

impl ToolKind {
    const fn name(self) -> &'static str {
        match self {
            Self::Ask => "ask_1c_ai",
            Self::Explain => "explain_1c_syntax",
            Self::Check => "check_1c_code",
            Self::Modify => "modify_1c_code",
            Self::SearchDocumentation => "search_1c_documentation",
            Self::SearchIts => "search_its",
            Self::FetchIts => "fetch_its",
            Self::DiffDocumentation => "diff_1c_documentation_versions",
        }
    }

    const fn field_order(self) -> &'static [&'static str] {
        match self {
            Self::Ask => &[
                "question",
                "programming_language",
                "ssl_version",
                "configuration",
            ],
            Self::Explain => &["syntax_element", "context", "ssl_version", "configuration"],
            Self::Check => &["code", "check_type", "extended"],
            Self::Modify => &["instruction", "code"],
            Self::SearchDocumentation => &["query", "version"],
            Self::SearchIts => &["query", "ssl_version", "configuration"],
            Self::FetchIts => &["id"],
            Self::DiffDocumentation => &["version_a", "version_b", "query"],
        }
    }
}

struct ToolDefinition {
    kind: ToolKind,
    tool: Tool,
    aliases: &'static [&'static str],
}

pub struct ToolCatalog {
    definitions: Box<[ToolDefinition]>,
    tools: Box<[Tool]>,
    call_mode: CallMode,
    programming_language: String,
}

impl ToolCatalog {
    #[must_use]
    #[cfg_attr(
        test,
        expect(
            dead_code,
            reason = "APP-06 wires the production catalog after APP-07 verifies it"
        )
    )]
    pub fn from_config(config: &Config) -> Self {
        Self::build(
            config.tool_input_min_length(),
            config.tool_input_max_length(),
            config.call_mode(),
            config.programming_language(),
            config.default_ssl_version(),
            config.default_1c_configuration(),
        )
    }

    #[cfg(test)]
    pub(super) fn for_test(
        min_length: usize,
        max_length: usize,
        call_mode: CallMode,
        programming_language: &str,
        default_ssl_version: &str,
        default_configuration: &str,
    ) -> Self {
        Self::build(
            min_length,
            max_length,
            call_mode,
            programming_language,
            default_ssl_version,
            default_configuration,
        )
    }

    fn build(
        min_length: usize,
        max_length: usize,
        call_mode: CallMode,
        programming_language: &str,
        default_ssl_version: &str,
        default_configuration: &str,
    ) -> Self {
        let default_ssl_version = default_ssl_version.trim().to_owned();
        let default_configuration = default_configuration.trim().to_owned();
        let definitions = build_definitions(
            min_length,
            max_length,
            &default_ssl_version,
            &default_configuration,
        )
        .into_boxed_slice();
        let tools = definitions
            .iter()
            .map(|definition| definition.tool.clone())
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            definitions,
            tools,
            call_mode,
            programming_language: programming_language.to_owned(),
        }
    }

    #[must_use]
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    pub fn prepare(
        &self,
        requested_name: &str,
        arguments: &Map<String, Value>,
    ) -> Result<PreparedToolCall, CatalogError> {
        let definition = self
            .definitions
            .iter()
            .find(|definition| {
                requested_name == definition.kind.name()
                    || definition.aliases.contains(&requested_name)
            })
            .ok_or_else(|| CatalogError::new(CatalogErrorKind::UnknownTool))?;
        let normalized = Self::validate(definition, arguments)?;
        let programming_language = normalized
            .get("programming_language")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(&self.programming_language)
            .to_owned();
        let route = self.route(definition.kind, &normalized);
        let instruction = match &route {
            ExecutionRoute::Message => build_message_instruction(definition.kind, &normalized),
            ExecutionRoute::Exact { name, arguments } => build_exact_instruction(name, arguments),
        };

        Ok(PreparedToolCall {
            canonical_name: definition.kind.name(),
            arguments: normalized,
            programming_language,
            instruction,
            route,
        })
    }

    fn validate(
        definition: &ToolDefinition,
        arguments: &Map<String, Value>,
    ) -> Result<Map<String, Value>, CatalogError> {
        normalize_arguments(
            definition.kind,
            definition.tool.input_schema.as_ref(),
            arguments,
        )
    }

    fn route(&self, kind: ToolKind, arguments: &Map<String, Value>) -> ExecutionRoute {
        if self.call_mode == CallMode::Standard {
            return ExecutionRoute::Message;
        }
        match kind {
            ToolKind::Check if arguments["check_type"] == "syntax" => ExecutionRoute::Exact {
                name: "mcp__syntax-checker__validate",
                arguments: object_from_pairs([
                    ("code", arguments["code"].clone()),
                    ("extended", arguments["extended"].clone()),
                ]),
            },
            ToolKind::SearchDocumentation => ExecutionRoute::Exact {
                name: "mcp__knowledge-hub__Search_Documentation",
                arguments: object_from_pairs([
                    ("query", arguments["query"].clone()),
                    ("version", arguments["version"].clone()),
                ]),
            },
            ToolKind::SearchIts => {
                let query = arguments["query"].as_str().unwrap_or_default();
                let prefix = context_prefix(
                    arguments["configuration"].as_str().unwrap_or_default(),
                    arguments["ssl_version"].as_str().unwrap_or_default(),
                );
                let query = if prefix.is_empty() {
                    query.to_owned()
                } else {
                    format!("{prefix} {query}")
                };
                ExecutionRoute::Exact {
                    name: "mcp__knowledge-hub__Search_ITS",
                    arguments: object_from_pairs([("query", Value::String(query))]),
                }
            }
            ToolKind::FetchIts => ExecutionRoute::Exact {
                name: "mcp__knowledge-hub__Fetch_ITS",
                arguments: object_from_pairs([("id", arguments["id"].clone())]),
            },
            ToolKind::DiffDocumentation => {
                let mut exact = object_from_pairs([
                    ("version_a", arguments["version_a"].clone()),
                    ("version_b", arguments["version_b"].clone()),
                ]);
                if arguments["query"]
                    .as_str()
                    .is_some_and(|query| !query.is_empty())
                {
                    exact.insert("query".to_owned(), arguments["query"].clone());
                }
                ExecutionRoute::Exact {
                    name: "mcp__knowledge-hub__Diff_Documentation_Versions",
                    arguments: exact,
                }
            }
            _ => ExecutionRoute::Message,
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "the pinned eight-tool schema snapshot is clearer as one ordered definition table"
)]
fn build_definitions(
    min_length: usize,
    max_length: usize,
    default_ssl: &str,
    default_configuration: &str,
) -> Vec<ToolDefinition> {
    let ssl_description = context_description(
        "Версия Библиотеки Стандартных Подсистем (БСП).",
        default_ssl,
    );
    let configuration_description = context_description(
        "Конфигурация 1С (например: Бухгалтерия предприятия, ERP, Управление торговлей).",
        default_configuration,
    );
    let project_context = || {
        [
            (
                "ssl_version",
                json!({
                    "type":"string",
                    "description":ssl_description,
                    "default":default_ssl
                }),
            ),
            (
                "configuration",
                json!({
                    "type":"string",
                    "description":configuration_description,
                    "default":default_configuration
                }),
            ),
        ]
    };

    vec![
        definition(
            ToolKind::Ask,
            ASK_DESCRIPTION,
            object_schema(
                "Ask 1C expert",
                object_from_pairs(
                    [
                        (
                            "question",
                            json!({
                                "type":"string",
                                "description":"Вопрос или задача на русском языке. Старайся формулировать конкретно.",
                                "minLength":min_length,
                                "maxLength":max_length
                            }),
                        ),
                        (
                            "programming_language",
                            json!({
                                "type":"string",
                                "description":"Язык, если вопрос связан с кодом или синтаксисом.",
                                "enum":["","BSL","SQL","JSON","HTTP"],
                                "default":"",
                                "maxLength":max_length
                            }),
                        ),
                    ]
                    .into_iter()
                    .chain(project_context()),
                ),
                &["question"],
            ),
            NO_ALIASES,
        ),
        definition(
            ToolKind::Explain,
            EXPLAIN_DESCRIPTION,
            object_schema(
                "Explain 1C syntax",
                object_from_pairs(
                    [
                        (
                            "syntax_element",
                            json!({
                                "type":"string",
                                "description":"Название элемента, который нужно объяснить, например HTTPЗапрос, ТаблицаЗначений или Запрос.",
                                "minLength":min_length,
                                "maxLength":max_length
                            }),
                        ),
                        (
                            "context",
                            json!({
                                "type":"string",
                                "description":"Дополнительный контекст использования, если он важен для ответа.",
                                "default":"",
                                "minLength":0,
                                "maxLength":max_length
                            }),
                        ),
                    ]
                    .into_iter()
                    .chain(project_context()),
                ),
                &["syntax_element"],
            ),
            NO_ALIASES,
        ),
        definition(
            ToolKind::Check,
            CHECK_DESCRIPTION,
            object_schema(
                "Check 1C code",
                object_from_pairs([
                    (
                        "code",
                        json!({
                            "type":"string",
                            "description":"Проверяемый фрагмент кода 1С.",
                            "minLength":min_length,
                            "maxLength":max_length
                        }),
                    ),
                    (
                        "check_type",
                        json!({
                            "type":"string",
                            "description":"syntax — синтаксическая проверка; review — code review. Значения logic/performance сохранены для обратной совместимости и обрабатываются как review.",
                            "enum":["syntax","review","logic","performance"],
                            "default":"syntax"
                        }),
                    ),
                    (
                        "extended",
                        json!({
                            "type":"boolean",
                            "description":"Только для syntax: включить обогащение стандартами 1С.",
                            "default":false
                        }),
                    ),
                ]),
                &["code"],
            ),
            NO_ALIASES,
        ),
        definition(
            ToolKind::Modify,
            MODIFY_DESCRIPTION,
            object_schema(
                "Modify 1C code",
                object_from_pairs([
                    (
                        "instruction",
                        json!({
                            "type":"string",
                            "description":"Четкое описание задачи на русском языке: что нужно изменить и какой результат ожидается.",
                            "minLength":min_length,
                            "maxLength":max_length
                        }),
                    ),
                    (
                        "code",
                        json!({
                            "type":"string",
                            "description":"Исходный код 1С, который нужно изменить.",
                            "default":"",
                            "minLength":0,
                            "maxLength":max_length
                        }),
                    ),
                ]),
                &["instruction"],
            ),
            NO_ALIASES,
        ),
        definition(
            ToolKind::SearchDocumentation,
            SEARCH_DOCS_DESCRIPTION,
            object_schema(
                "Search 1C documentation",
                object_from_pairs([
                    (
                        "query",
                        json!({
                            "type":"string",
                            "description":"Поисковый запрос для embedding-поиска. Для общих тем лучше писать 'Общая информация о ...' или 'Список всех ...'.",
                            "minLength":min_length,
                            "maxLength":max_length
                        }),
                    ),
                    (
                        "version",
                        json!({
                            "type":"string",
                            "description":"Версия документации платформы в формате v8.x.x или v8.x.x.x.",
                            "default":"v8.5.1",
                            "maxLength":max_length
                        }),
                    ),
                ]),
                &["query"],
            ),
            SEARCH_DOCUMENTATION_ALIAS,
        ),
        definition(
            ToolKind::SearchIts,
            SEARCH_ITS_DESCRIPTION,
            object_schema(
                "Search ITS",
                object_from_pairs(
                    [(
                        "query",
                        json!({
                            "type":"string",
                            "description":"Поисковый запрос для embedding-поиска по ИТС.",
                            "minLength":min_length,
                            "maxLength":max_length
                        }),
                    )]
                    .into_iter()
                    .chain(project_context()),
                ),
                &["query"],
            ),
            SEARCH_ITS_ALIAS,
        ),
        definition(
            ToolKind::FetchIts,
            FETCH_ITS_DESCRIPTION,
            object_schema(
                "Fetch ITS",
                object_from_pairs([(
                    "id",
                    json!({
                        "type":"string",
                        "description":"Идентификатор документа, каталога или базы ИТС: root, superior, v8std или строка вида its-...-hdoc/hdir.",
                        "default":"root",
                        "minLength":1,
                        "maxLength":max_length
                    }),
                )]),
                &[],
            ),
            FETCH_ITS_ALIAS,
        ),
        definition(
            ToolKind::DiffDocumentation,
            DIFF_DESCRIPTION,
            object_schema(
                "Diff 1C documentation versions",
                object_from_pairs([
                    (
                        "version_a",
                        json!({
                            "type":"string",
                            "description":"Более ранняя версия в формате v8.3.27 или v8.3.27.189.",
                            "minLength":2,
                            "maxLength":max_length
                        }),
                    ),
                    (
                        "version_b",
                        json!({
                            "type":"string",
                            "description":"Более поздняя версия в формате v8.3.27 или v8.3.27.189.",
                            "minLength":2,
                            "maxLength":max_length
                        }),
                    ),
                    (
                        "query",
                        json!({
                            "type":"string",
                            "description":"Необязательная предметная область сравнения, например 'HTTP соединение'.",
                            "default":"",
                            "maxLength":max_length
                        }),
                    ),
                ]),
                &["version_a", "version_b"],
            ),
            DIFF_ALIAS,
        ),
    ]
}

fn definition(
    kind: ToolKind,
    description: &'static str,
    input_schema: Arc<Map<String, Value>>,
    aliases: &'static [&'static str],
) -> ToolDefinition {
    ToolDefinition {
        kind,
        tool: Tool::new(kind.name(), description, input_schema),
        aliases,
    }
}

fn object_schema(
    title: &'static str,
    properties: Map<String, Value>,
    required: &[&str],
) -> Arc<Map<String, Value>> {
    Arc::new(object_from_pairs([
        ("type", Value::String("object".to_owned())),
        ("title", Value::String(title.to_owned())),
        ("properties", Value::Object(properties)),
        (
            "required",
            Value::Array(
                required
                    .iter()
                    .map(|name| Value::String((*name).to_owned()))
                    .collect(),
            ),
        ),
    ]))
}

fn object_from_pairs<I, K>(pairs: I) -> Map<String, Value>
where
    I: IntoIterator<Item = (K, Value)>,
    K: Into<String>,
{
    pairs
        .into_iter()
        .map(|(key, value)| (key.into(), value))
        .collect()
}

fn context_description(base: &str, default: &str) -> String {
    if default.is_empty() {
        base.to_owned()
    } else {
        format!("{base} По умолчанию: {default}.")
    }
}

fn normalize_arguments(
    kind: ToolKind,
    input_schema: &Map<String, Value>,
    arguments: &Map<String, Value>,
) -> Result<Map<String, Value>, CatalogError> {
    let properties = input_schema
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(invalid_arguments)?;
    let required = input_schema
        .get("required")
        .and_then(Value::as_array)
        .ok_or_else(invalid_arguments)?;
    let mut normalized = Map::new();

    for field in kind.field_order() {
        let spec = properties
            .get(*field)
            .and_then(Value::as_object)
            .ok_or_else(invalid_arguments)?;
        let is_required = required.iter().any(|name| name.as_str() == Some(field));
        let raw = arguments.get(*field);

        let Some(raw) = raw else {
            if is_required {
                return Err(empty_required_error(kind, field));
            }
            normalized.insert((*field).to_owned(), default_for(spec));
            continue;
        };

        if raw.is_null() {
            if is_required {
                return Err(empty_required_error(kind, field));
            }
            return Err(type_error(field, spec));
        }

        if spec.get("type").and_then(Value::as_str) == Some("boolean") {
            let Some(value) = raw.as_bool() else {
                return Err(type_error(field, spec));
            };
            normalized.insert((*field).to_owned(), Value::Bool(value));
            continue;
        }

        let Some(raw_string) = raw.as_str() else {
            return Err(type_error(field, spec));
        };
        let stripped = raw_string.trim();
        let value = if stripped.is_empty() {
            if is_required {
                return Err(empty_required_error(kind, field));
            }
            default_for(spec).as_str().unwrap_or_default().to_owned()
        } else {
            stripped.to_owned()
        };

        if let Some(allowed) = spec.get("enum").and_then(Value::as_array)
            && !allowed
                .iter()
                .any(|candidate| candidate.as_str() == Some(&value))
        {
            let allowed = allowed
                .iter()
                .filter_map(Value::as_str)
                .map(|item| format!("'{item}'"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(invalid_arguments_with(format!(
                "Ошибка: {field} должно быть одним из: {allowed}"
            )));
        }

        if !value.is_empty() {
            let length = value.chars().count();
            if let Some(min_length) = spec.get("minLength").and_then(Value::as_u64)
                && length < usize::try_from(min_length).unwrap_or(usize::MAX)
            {
                return Err(invalid_arguments_with(format!(
                    "Ошибка: {field} короче минимальной длины {min_length}"
                )));
            }
            if let Some(max_length) = spec.get("maxLength").and_then(Value::as_u64)
                && length > usize::try_from(max_length).unwrap_or(usize::MAX)
            {
                return Err(CatalogError::with_message(
                    CatalogErrorKind::LimitExceeded,
                    format!("Ошибка: {field} длиннее максимальной длины {max_length}"),
                ));
            }
        }

        normalized.insert((*field).to_owned(), Value::String(value));
    }

    Ok(normalized)
}

fn default_for(spec: &Map<String, Value>) -> Value {
    spec.get("default").cloned().unwrap_or_else(|| {
        if spec.get("type").and_then(Value::as_str) == Some("string") {
            Value::String(String::new())
        } else {
            Value::Bool(false)
        }
    })
}

fn empty_required_error(kind: ToolKind, field: &str) -> CatalogError {
    let message = match (kind, field) {
        (ToolKind::Ask, "question") => "Ошибка: Вопрос не может быть пустым".to_owned(),
        (ToolKind::Explain, "syntax_element") => {
            "Ошибка: Элемент синтаксиса не может быть пустым".to_owned()
        }
        (ToolKind::Check, "code") => "Ошибка: Код для проверки не может быть пустым".to_owned(),
        (ToolKind::Modify, "instruction") => "Ошибка: instruction не может быть пустым".to_owned(),
        (ToolKind::SearchDocumentation | ToolKind::SearchIts, "query") => {
            "Ошибка: query не может быть пустым".to_owned()
        }
        (ToolKind::DiffDocumentation, "version_a" | "version_b") => {
            "Ошибка: version_a и version_b обязательны".to_owned()
        }
        _ => format!("Ошибка: {field} не может быть пустым"),
    };
    invalid_arguments_with(message)
}

fn type_error(field: &str, spec: &Map<String, Value>) -> CatalogError {
    if spec.get("type").and_then(Value::as_str) == Some("boolean") {
        invalid_arguments_with(format!(
            "Ошибка: {field} должно быть логическим значением (true или false)"
        ))
    } else {
        invalid_arguments_with(format!("Ошибка: {field} должно быть строкой"))
    }
}

fn invalid_arguments() -> CatalogError {
    CatalogError::new(CatalogErrorKind::InvalidArguments)
}

fn invalid_arguments_with(message: impl Into<String>) -> CatalogError {
    CatalogError::with_message(CatalogErrorKind::InvalidArguments, message)
}

fn context_prefix(configuration: &str, ssl_version: &str) -> String {
    match (configuration.is_empty(), ssl_version.is_empty()) {
        (true, true) => String::new(),
        (false, true) => configuration.to_owned(),
        (true, false) => format!("БСП {ssl_version}"),
        (false, false) => format!("{configuration} БСП {ssl_version}"),
    }
}

fn context_hint(arguments: &Map<String, Value>) -> String {
    let configuration = arguments["configuration"].as_str().unwrap_or_default();
    let ssl_version = arguments["ssl_version"].as_str().unwrap_or_default();
    let mut parts = Vec::with_capacity(2);
    if !configuration.is_empty() {
        parts.push(format!("конфигурация {configuration}"));
    }
    if !ssl_version.is_empty() {
        parts.push(format!("версия БСП {ssl_version}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("Контекст проекта: {}.", parts.join(", "))
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "the eight pinned message templates form one exhaustive execution table"
)]
fn build_message_instruction(kind: ToolKind, arguments: &Map<String, Value>) -> String {
    match kind {
        ToolKind::Ask => {
            let question = arguments["question"].as_str().unwrap_or_default();
            let hint = context_hint(arguments);
            if hint.is_empty() {
                question.to_owned()
            } else {
                format!("{question}\n\n{hint}")
            }
        }
        ToolKind::Explain => {
            let syntax = arguments["syntax_element"].as_str().unwrap_or_default();
            let context = arguments["context"].as_str().unwrap_or_default();
            let mut question = format!("Объясни синтаксис и использование: {syntax}");
            if !context.is_empty() {
                question.push_str(" в контексте: ");
                question.push_str(context);
            }
            let hint = context_hint(arguments);
            if !hint.is_empty() {
                question.push_str("\n\n");
                question.push_str(&hint);
            }
            question
        }
        ToolKind::Check => {
            let code = arguments["code"].as_str().unwrap_or_default();
            if arguments["check_type"] == "syntax" {
                let suffix = if arguments["extended"].as_bool().unwrap_or(false) {
                    " Используй расширенную проверку со стандартами 1С."
                } else {
                    ""
                };
                format!(
                    "Проверь этот код 1С на синтаксические ошибки перед отправкой \
пользователю.{suffix}\n\nКод:\n```bsl\n{code}\n```"
                )
            } else {
                format!(
                    "Проведи code review этого кода 1С. Найди ошибки, нарушения стандартов, \
риски и предложи исправленный вариант.\n\nКод:\n```bsl\n{code}\n```"
                )
            }
        }
        ToolKind::Modify => {
            let instruction = arguments["instruction"].as_str().unwrap_or_default();
            let code = arguments["code"].as_str().unwrap_or_default();
            let mut prompt = format!(
                "Измени этот код 1С по заданию пользователя. Верни итоговый измененный код и \
кратко перечисли, что именно было изменено.\n\nЗадание:\n{instruction}"
            );
            if !code.trim().is_empty() {
                prompt.push_str("\n\nКод:\n```bsl\n");
                prompt.push_str(code);
                prompt.push_str("\n```");
            }
            prompt.push_str(
                "\n\nОБЯЗАТЕЛЬНО выполни синтаксическую проверку измененного кода с помощью \
инструмента mcp__syntax-checker__validate перед отправкой результата.",
            );
            prompt
        }
        ToolKind::SearchDocumentation => format!(
            "Найди информацию в документации платформы 1С:Предприятие. Используй документацию \
версии {}. Верни краткий, но информативный ответ по найденным данным.\n\nЗапрос: {}",
            arguments["version"].as_str().unwrap_or_default(),
            arguments["query"].as_str().unwrap_or_default()
        ),
        ToolKind::SearchIts => {
            let hint = context_hint(arguments);
            let hint_line = if hint.is_empty() {
                String::new()
            } else {
                format!("{hint}\n")
            };
            format!(
                "Выполни поиск в базе знаний ИТС по этому запросу. Верни фактический результат и \
обязательно сохрани ссылки на источники.\n{hint_line}\nЗапрос: {}",
                arguments["query"].as_str().unwrap_or_default()
            )
        }
        ToolKind::FetchIts => format!(
            "Получить содержимое документа, каталога или базы ИТС по \
идентификатору.\n\nid: {}",
            arguments["id"].as_str().unwrap_or_default()
        ),
        ToolKind::DiffDocumentation => {
            let query = arguments["query"].as_str().unwrap_or_default();
            let scope = if query.is_empty() {
                String::new()
            } else {
                format!("\nПредметная область: {query}")
            };
            format!(
                "Сравни документацию платформы 1С между двумя версиями и верни \
различия.\n\nБолее ранняя версия: {}\nБолее поздняя версия: {}{scope}",
                arguments["version_a"].as_str().unwrap_or_default(),
                arguments["version_b"].as_str().unwrap_or_default()
            )
        }
    }
}

fn build_exact_instruction(name: &str, arguments: &Map<String, Value>) -> String {
    let encoded = serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_owned());
    format!(
        "Внутренняя инструкция.\nНужно вернуть ровно один tool call для {name}.\n\
Не используй другие инструменты.\nСохрани все символы в аргументах без изменений.\n\
Используй ровно эти JSON-аргументы: {encoded}\nНе отвечай обычным текстом до tool call."
    )
}

#[must_use]
#[cfg(test)]
pub fn rename_1c_markdown_fences(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut inside_fence = false;
    let mut position = 0;

    while position < input.len() {
        let (content_end, line_end) = next_line(input, position);
        let line = &input[position..content_end];
        let indent_length = line.bytes().take_while(|byte| *byte == b' ').count().min(4);
        let markdown = if indent_length <= 3 {
            &line[indent_length..]
        } else {
            ""
        };

        if !inside_fence && matches!(markdown, "```1С" | "```1C" | "```1C (BSL)") {
            output.push_str(&line[..indent_length]);
            output.push_str("```bsl");
            inside_fence = true;
        } else {
            output.push_str(line);
            if let Some(after_fence) = markdown.strip_prefix("```") {
                if inside_fence && after_fence.trim().is_empty() {
                    inside_fence = false;
                } else if !inside_fence {
                    inside_fence = true;
                }
            }
        }
        output.push_str(&input[content_end..line_end]);
        position = line_end;
    }
    output
}

#[cfg(test)]
fn next_line(input: &str, start: usize) -> (usize, usize) {
    let bytes = input.as_bytes();
    let mut index = start;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => return (index, index + 1),
            b'\r' => {
                let end = if bytes.get(index + 1) == Some(&b'\n') {
                    index + 2
                } else {
                    index + 1
                };
                return (index, end);
            }
            _ => index += 1,
        }
    }
    (bytes.len(), bytes.len())
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};

    use super::{CatalogErrorKind, ExecutionRoute, ToolCatalog, rename_1c_markdown_fences};
    use crate::config::CallMode;

    const NAMES: [&str; 8] = [
        "ask_1c_ai",
        "explain_1c_syntax",
        "check_1c_code",
        "modify_1c_code",
        "search_1c_documentation",
        "search_its",
        "fetch_its",
        "diff_1c_documentation_versions",
    ];

    fn catalog(mode: CallMode) -> ToolCatalog {
        ToolCatalog::for_test(4, 100_000, mode, "", "", "")
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "the test helper consumes temporary json! values at each call site"
    )]
    fn object(value: Value) -> Map<String, Value> {
        value.as_object().expect("test value is an object").clone()
    }

    #[test]
    fn public_catalog_is_the_pinned_eight_tool_snapshot() {
        let catalog = catalog(CallMode::Direct);
        let names = catalog
            .tools()
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(names, NAMES);

        let rendered = serde_json::to_value(catalog.tools()).unwrap();
        let expected: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/tool_catalog.json")).unwrap();
        assert_eq!(rendered, expected);
    }

    #[test]
    fn aliases_are_call_only_case_sensitive_and_canonicalized() {
        let catalog = catalog(CallMode::Direct);
        for (alias, canonical) in [
            ("Search_1C_Documentation", "search_1c_documentation"),
            ("Search_ITS", "search_its"),
            ("Fetch_ITS", "fetch_its"),
            (
                "Diff_1C_Documentation_Versions",
                "diff_1c_documentation_versions",
            ),
        ] {
            let call = catalog.prepare(alias, &Map::new()).unwrap_or_else(|_| {
                let required = match canonical {
                    "search_1c_documentation" | "search_its" => object(json!({"query":"test"})),
                    "diff_1c_documentation_versions" => {
                        object(json!({"version_a":"v1","version_b":"v2"}))
                    }
                    _ => Map::new(),
                };
                catalog.prepare(alias, &required).unwrap()
            });
            assert_eq!(call.canonical_name(), canonical);

            let wrong_case = format!("{}{}", alias[..1].to_ascii_lowercase(), &alias[1..]);
            let error = catalog.prepare(&wrong_case, &Map::new()).unwrap_err();
            assert_eq!(error.kind(), CatalogErrorKind::UnknownTool);
        }
        assert!(catalog.tools().iter().all(|tool| {
            !tool
                .name
                .starts_with(|character: char| character.is_uppercase())
        }));
    }

    #[test]
    fn every_default_and_compatibility_enumeration_is_applied() {
        let catalog = catalog(CallMode::Direct);
        let ask = catalog
            .prepare("ask_1c_ai", &object(json!({"question":"test"})))
            .unwrap();
        assert_eq!(ask.arguments()["programming_language"], "");
        assert_eq!(ask.arguments()["ssl_version"], "");
        assert_eq!(ask.arguments()["configuration"], "");

        let explain = catalog
            .prepare(
                "explain_1c_syntax",
                &object(json!({"syntax_element":"HTTPЗапрос"})),
            )
            .unwrap();
        assert_eq!(explain.arguments()["context"], "");

        let check = catalog
            .prepare("check_1c_code", &object(json!({"code":"abcd"})))
            .unwrap();
        assert_eq!(check.arguments()["check_type"], "syntax");
        assert_eq!(check.arguments()["extended"], false);

        for legacy in ["logic", "performance"] {
            let check = catalog
                .prepare(
                    "check_1c_code",
                    &object(json!({"code":"abcd","check_type":legacy})),
                )
                .unwrap();
            assert_eq!(check.arguments()["check_type"], legacy);
        }

        let modify = catalog
            .prepare(
                "modify_1c_code",
                &object(json!({"instruction":"change this"})),
            )
            .unwrap();
        assert_eq!(modify.arguments()["code"], "");

        let docs = catalog
            .prepare("search_1c_documentation", &object(json!({"query":"test"})))
            .unwrap();
        assert_eq!(docs.arguments()["version"], "v8.5.1");

        let fetch = catalog.prepare("fetch_its", &Map::new()).unwrap();
        assert_eq!(fetch.arguments()["id"], "root");

        let diff = catalog
            .prepare(
                "diff_1c_documentation_versions",
                &object(json!({"version_a":"v1","version_b":"v2"})),
            )
            .unwrap();
        assert_eq!(diff.arguments()["query"], "");
    }

    #[test]
    fn validation_matches_the_python_buddy_1_4_0_fixture() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/python_buddy_1_4_0_observed_parity.json"
        ))
        .expect("the Python parity fixture must be valid JSON");
        let settings = &fixture["catalog"];
        let catalog = ToolCatalog::for_test(
            usize::try_from(settings["min_length"].as_u64().unwrap()).unwrap(),
            usize::try_from(settings["max_length"].as_u64().unwrap()).unwrap(),
            CallMode::Direct,
            settings["programming_language"].as_str().unwrap(),
            settings["default_ssl_version"].as_str().unwrap(),
            settings["default_configuration"].as_str().unwrap(),
        );

        for case in fixture["input_cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let tool = case["tool"].as_str().unwrap();
            let arguments = case["arguments"].as_object().unwrap();
            if let Some(expected) = case.get("normalized") {
                let prepared = catalog
                    .prepare(tool, arguments)
                    .unwrap_or_else(|error| panic!("{name}: {error}"));
                assert_eq!(
                    prepared.arguments(),
                    expected.as_object().unwrap(),
                    "{name}"
                );
            } else {
                let error = catalog
                    .prepare(tool, arguments)
                    .expect_err("fixture case must be rejected");
                assert_eq!(error.to_string(), case["error"].as_str().unwrap(), "{name}");
            }
        }
    }

    #[test]
    fn validation_counts_unicode_scalars_after_stripping_and_ignores_unknown_fields() {
        let catalog = ToolCatalog::for_test(4, 100, CallMode::Standard, "BSL", "", "");
        let original = "  🦀🦀🦀🦀\r\n";
        let call = catalog
            .prepare("ask_1c_ai", &object(json!({"question":original})))
            .unwrap();
        assert_eq!(call.arguments()["question"], "🦀🦀🦀🦀");
        assert_eq!(call.instruction(), "🦀🦀🦀🦀");
        assert_eq!(call.programming_language(), "BSL");

        let scalar_catalog = ToolCatalog::for_test(4, 4, CallMode::Standard, "", "", "");
        scalar_catalog
            .prepare("ask_1c_ai", &object(json!({"question":"🦀🦀🦀🦀"})))
            .expect("four Unicode scalar values are accepted");
        let too_long = scalar_catalog
            .prepare("ask_1c_ai", &object(json!({"question":"🦀🦀🦀🦀🦀"})))
            .unwrap_err();
        assert_eq!(too_long.kind(), CatalogErrorKind::LimitExceeded);

        let production_limit = ToolCatalog::for_test(4, 100_000, CallMode::Standard, "", "", "");
        production_limit
            .prepare(
                "ask_1c_ai",
                &object(json!({"question":"x".repeat(100_000)})),
            )
            .expect("one hundred thousand Unicode scalar values are accepted");
        let over_production_limit = production_limit
            .prepare(
                "ask_1c_ai",
                &object(json!({"question":"x".repeat(100_001)})),
            )
            .unwrap_err();
        assert_eq!(
            over_production_limit.kind(),
            CatalogErrorKind::LimitExceeded
        );

        let with_unknown = catalog
            .prepare(
                "ask_1c_ai",
                &object(json!({"question":"test","unknown":true})),
            )
            .expect("undeclared fields follow the Python behavior and are ignored");
        assert!(!with_unknown.arguments().contains_key("unknown"));

        for invalid in [
            object(json!({"question":"    "})),
            object(json!({"question":"test","programming_language":"Rust"})),
        ] {
            let error = catalog.prepare("ask_1c_ai", &invalid).unwrap_err();
            assert_eq!(error.kind(), CatalogErrorKind::InvalidArguments);
        }
    }

    #[test]
    fn explicit_project_context_wins_and_blank_context_uses_global_defaults() {
        let catalog = ToolCatalog::for_test(4, 100_000, CallMode::Direct, "", "3.1.10", "ERP");

        let explicit = catalog
            .prepare(
                "search_its",
                &object(json!({
                    "query":"test",
                    "ssl_version":" 3.1.11 ",
                    "configuration":" УТ "
                })),
            )
            .unwrap();
        assert_eq!(explicit.arguments()["ssl_version"], "3.1.11");
        assert_eq!(explicit.arguments()["configuration"], "УТ");

        let fallback = catalog
            .prepare(
                "search_its",
                &object(json!({
                    "query":"test",
                    "ssl_version":" ",
                    "configuration":""
                })),
            )
            .unwrap();
        assert_eq!(fallback.arguments()["ssl_version"], "3.1.10");
        assert_eq!(fallback.arguments()["configuration"], "ERP");

        let tools = catalog.tools();
        let search_schema = tools
            .iter()
            .find(|tool| tool.name == "search_its")
            .expect("search_its is public");
        assert_eq!(
            search_schema.input_schema["properties"]["ssl_version"]["default"],
            "3.1.10"
        );
        assert_eq!(
            search_schema.input_schema["properties"]["configuration"]["default"],
            "ERP"
        );
    }

    #[test]
    fn question_code_and_instruction_reach_prepared_requests_after_stripping() {
        let standard = catalog(CallMode::Standard);
        let question = "\n  вопрос  \r\n";
        let ask = standard
            .prepare(
                "ask_1c_ai",
                &object(json!({
                    "question":question,
                    "programming_language":"SQL"
                })),
            )
            .unwrap();
        assert_eq!(ask.arguments()["question"], "вопрос");
        assert_eq!(ask.instruction(), "вопрос");
        assert_eq!(ask.programming_language(), "SQL");

        let code = "\n  Сообщить(\"x\");  \r\n";
        let check = standard
            .prepare("check_1c_code", &object(json!({"code":code})))
            .unwrap();
        assert_eq!(check.arguments()["code"], "Сообщить(\"x\");");
        assert!(check.instruction().contains("Сообщить(\"x\");"));
        let direct_check = catalog(CallMode::Direct)
            .prepare("check_1c_code", &object(json!({"code":code})))
            .unwrap();
        assert_eq!(
            direct_check
                .route()
                .exact_arguments()
                .expect("syntax uses the exact route")["code"],
            "Сообщить(\"x\");"
        );

        let instruction = "\n  изменить  \r\n";
        let modify = standard
            .prepare(
                "modify_1c_code",
                &object(json!({"instruction":instruction,"code":code})),
            )
            .unwrap();
        assert_eq!(modify.arguments()["instruction"], "изменить");
        assert_eq!(modify.arguments()["code"], "Сообщить(\"x\");");
        assert!(modify.instruction().contains("изменить"));
        assert!(modify.instruction().contains("Сообщить(\"x\");"));
    }

    #[test]
    fn direct_and_standard_routes_follow_the_normative_table_without_fallback() {
        let direct = catalog(CallMode::Direct);
        let standard = catalog(CallMode::Standard);
        let cases = [
            ("ask_1c_ai", object(json!({"question":"test"})), None),
            (
                "explain_1c_syntax",
                object(json!({"syntax_element":"test"})),
                None,
            ),
            (
                "check_1c_code",
                object(json!({"code":"test","check_type":"syntax"})),
                Some("mcp__syntax-checker__validate"),
            ),
            (
                "check_1c_code",
                object(json!({"code":"test","check_type":"review"})),
                None,
            ),
            (
                "modify_1c_code",
                object(json!({"instruction":"test"})),
                None,
            ),
            (
                "search_1c_documentation",
                object(json!({"query":"test"})),
                Some("mcp__knowledge-hub__Search_Documentation"),
            ),
            (
                "search_its",
                object(json!({"query":"test"})),
                Some("mcp__knowledge-hub__Search_ITS"),
            ),
            (
                "fetch_its",
                Map::new(),
                Some("mcp__knowledge-hub__Fetch_ITS"),
            ),
            (
                "diff_1c_documentation_versions",
                object(json!({"version_a":"v1","version_b":"v2"})),
                Some("mcp__knowledge-hub__Diff_Documentation_Versions"),
            ),
        ];

        for (name, arguments, expected_exact) in cases {
            let direct_call = direct.prepare(name, &arguments).unwrap();
            assert_eq!(direct_call.route().exact_name(), expected_exact);
            if expected_exact.is_some() {
                assert!(direct_call.route().exact_arguments().is_some());
            }
            let standard_call = standard.prepare(name, &arguments).unwrap();
            assert!(matches!(standard_call.route(), ExecutionRoute::Message));
        }
    }

    #[test]
    fn markdown_cleanup_only_renames_the_opening_fence_designation() {
        let source = concat!(
            "До Ａ\u{301}\r\n",
            "```1C (BSL)\r\n",
            "Строка = \"```1C\";\r\n",
            "\u{0001}\r\n",
            "```\r\n",
            "После"
        );
        let cleaned = rename_1c_markdown_fences(source);

        assert_eq!(
            cleaned,
            concat!(
                "До Ａ\u{301}\r\n",
                "```bsl\r\n",
                "Строка = \"```1C\";\r\n",
                "\u{0001}\r\n",
                "```\r\n",
                "После"
            )
        );
    }
}
