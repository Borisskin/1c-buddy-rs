//! Startup configuration and validation.

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::path::{Path, PathBuf};

use secrecy::{ExposeSecret, SecretString};
use tracing_subscriber::EnvFilter;

use crate::error::ConfigError;
use crate::limits::{
    TOKEN_MAX_BYTES, TOOL_INPUT_MAX_DEFAULT, TOOL_INPUT_MAX_HARD, TOOL_INPUT_MIN_DEFAULT,
};

const TOKEN_ENV: &str = "ONEC_AI_TOKEN";
const TOKEN_FILE_ENV: &str = "ONEC_AI_TOKEN_FILE";
const BASE_URL_ENV: &str = "ONEC_AI_BASE_URL";
const TOOL_INPUT_MIN_ENV: &str = "MCP_TOOL_INPUT_MIN_LENGTH";
const TOOL_INPUT_MAX_ENV: &str = "MCP_TOOL_INPUT_MAX_LENGTH";
const TOOL_CALL_MODE_ENV: &str = "MCP_TOOL_CALL_MODE";
const UI_LANGUAGE_ENV: &str = "ONEC_AI_UI_LANGUAGE";
const PROGRAMMING_LANGUAGE_ENV: &str = "ONEC_AI_PROGRAMMING_LANGUAGE";
const DEFAULT_SSL_VERSION_ENV: &str = "DEFAULT_SSL_VERSION";
const DEFAULT_CONFIGURATION_ENV: &str = "DEFAULT_1C_CONFIGURATION";
const MAX_CONCURRENT_CALLS_ENV: &str = "MCP_MAX_CONCURRENT_CALLS";
const RUST_LOG_ENV: &str = "RUST_LOG";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallMode {
    Direct,
    Standard,
}

pub struct Config {
    token: SecretString,
    tool_input_min_length: usize,
    tool_input_max_length: usize,
    call_mode: CallMode,
    ui_language: String,
    programming_language: String,
    default_ssl_version: String,
    default_1c_configuration: String,
    max_concurrent_calls: usize,
    rust_log: String,
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("token", &"[REDACTED]")
            .field("tool_input_min_length", &self.tool_input_min_length)
            .field("tool_input_max_length", &self.tool_input_max_length)
            .field("call_mode", &self.call_mode)
            .field("ui_language", &self.ui_language)
            .field("programming_language", &self.programming_language)
            .field("default_ssl_version", &self.default_ssl_version)
            .field("default_1c_configuration", &self.default_1c_configuration)
            .field("max_concurrent_calls", &self.max_concurrent_calls)
            .field("rust_log", &self.rust_log)
            .finish()
    }
}

impl Config {
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_with(&ProcessEnvironment, &RealTokenFileSystem)
    }

    fn load_with<E, F>(environment: &E, file_system: &F) -> Result<Self, ConfigError>
    where
        E: Environment,
        F: TokenFileSystem,
    {
        let token_value = environment.get(TOKEN_ENV);
        let token_file_value = environment.get(TOKEN_FILE_ENV);

        let token_source = match (token_value, token_file_value) {
            (Some(token), None) => TokenSource::Environment(token),
            (None, Some(path)) => TokenSource::File(path),
            (None, None) | (Some(_), Some(_)) => {
                return Err(ConfigError::TokenSourceCount);
            }
        };

        if environment.get(BASE_URL_ENV).is_some() {
            return Err(ConfigError::UnsupportedBaseUrl);
        }

        let token = match token_source {
            TokenSource::Environment(value) => {
                let token = os_string_to_string(value, TOKEN_ENV)?;
                validate_token(token)?
            }
            TokenSource::File(value) => load_token_file(value, file_system)?,
        };

        let tool_input_min_length = read_usize(
            environment,
            TOOL_INPUT_MIN_ENV,
            TOOL_INPUT_MIN_DEFAULT,
            1,
            TOOL_INPUT_MAX_HARD,
        )?;
        let tool_input_max_length = read_usize(
            environment,
            TOOL_INPUT_MAX_ENV,
            TOOL_INPUT_MAX_DEFAULT,
            1,
            TOOL_INPUT_MAX_HARD,
        )?;
        if tool_input_min_length > tool_input_max_length {
            return Err(ConfigError::InvertedToolInputLimits);
        }

        let call_mode = match read_string(environment, TOOL_CALL_MODE_ENV, "direct")?.as_str() {
            "direct" => CallMode::Direct,
            "standard" => CallMode::Standard,
            _ => {
                return Err(ConfigError::InvalidSetting {
                    name: TOOL_CALL_MODE_ENV,
                });
            }
        };
        let ui_language = read_bounded_text(environment, UI_LANGUAGE_ENV, "russian", 1, 64)?;
        let programming_language =
            read_bounded_text(environment, PROGRAMMING_LANGUAGE_ENV, "", 0, 64)?;
        let default_ssl_version =
            read_bounded_text(environment, DEFAULT_SSL_VERSION_ENV, "", 0, 256)?;
        let default_1c_configuration =
            read_bounded_text(environment, DEFAULT_CONFIGURATION_ENV, "", 0, 256)?;
        let max_concurrent_calls = read_usize(environment, MAX_CONCURRENT_CALLS_ENV, 2, 1, 2)?;

        let rust_log = read_string(environment, RUST_LOG_ENV, "warn")?;
        if rust_log.is_empty() || rust_log.len() > 1024 || contains_forbidden_control(&rust_log) {
            return Err(ConfigError::InvalidSetting { name: RUST_LOG_ENV });
        }
        EnvFilter::try_new(&rust_log).map_err(|_| ConfigError::InvalidLogFilter)?;

        Ok(Self {
            token,
            tool_input_min_length,
            tool_input_max_length,
            call_mode,
            ui_language,
            programming_language,
            default_ssl_version,
            default_1c_configuration,
            max_concurrent_calls,
            rust_log,
        })
    }

    pub fn init_tracing(&self) -> Result<(), ConfigError> {
        let filter =
            EnvFilter::try_new(&self.rust_log).map_err(|_| ConfigError::InvalidLogFilter)?;
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .with_target(false)
            .try_init()
            .map_err(|_| ConfigError::TracingInitialization)
    }

    #[must_use]
    pub fn token(&self) -> &SecretString {
        &self.token
    }

    #[must_use]
    pub fn tool_input_min_length(&self) -> usize {
        self.tool_input_min_length
    }

    #[must_use]
    pub fn tool_input_max_length(&self) -> usize {
        self.tool_input_max_length
    }

    #[must_use]
    pub fn call_mode(&self) -> CallMode {
        self.call_mode
    }

    #[must_use]
    pub fn ui_language(&self) -> &str {
        &self.ui_language
    }

    #[must_use]
    pub fn programming_language(&self) -> &str {
        &self.programming_language
    }

    #[must_use]
    pub fn default_ssl_version(&self) -> &str {
        &self.default_ssl_version
    }

    #[must_use]
    pub fn default_1c_configuration(&self) -> &str {
        &self.default_1c_configuration
    }

    #[must_use]
    pub fn max_concurrent_calls(&self) -> usize {
        self.max_concurrent_calls
    }

    #[must_use]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the validated filter is consumed by init_tracing")
    )]
    pub fn rust_log(&self) -> &str {
        &self.rust_log
    }
}

enum TokenSource {
    Environment(OsString),
    File(OsString),
}

trait Environment {
    fn get(&self, name: &'static str) -> Option<OsString>;
}

struct ProcessEnvironment;

impl Environment for ProcessEnvironment {
    fn get(&self, name: &'static str) -> Option<OsString> {
        env::var_os(name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileKind {
    Regular,
    Directory,
    Link,
    Other,
}

#[derive(Clone, Copy, Debug)]
struct TokenFileInfo {
    kind: FileKind,
    len: u64,
}

trait TokenFileSystem {
    fn inspect(&self, path: &Path) -> Result<TokenFileInfo, ()>;
    fn read_limited(&self, path: &Path, limit: usize) -> Result<Vec<u8>, ()>;
}

struct RealTokenFileSystem;

impl TokenFileSystem for RealTokenFileSystem {
    fn inspect(&self, path: &Path) -> Result<TokenFileInfo, ()> {
        let metadata = fs::symlink_metadata(path).map_err(|_| ())?;
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() || is_reparse_point(&metadata) {
            FileKind::Link
        } else if file_type.is_file() {
            FileKind::Regular
        } else if file_type.is_dir() {
            FileKind::Directory
        } else {
            FileKind::Other
        };

        Ok(TokenFileInfo {
            kind,
            len: metadata.len(),
        })
    }

    fn read_limited(&self, path: &Path, limit: usize) -> Result<Vec<u8>, ()> {
        let file = File::open(path).map_err(|_| ())?;
        let mut contents = Vec::new();
        file.take((limit + 1) as u64)
            .read_to_end(&mut contents)
            .map_err(|_| ())?;
        Ok(contents)
    }
}

#[cfg(windows)]
fn is_reparse_point(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &Metadata) -> bool {
    false
}

fn load_token_file<F>(value: OsString, file_system: &F) -> Result<SecretString, ConfigError>
where
    F: TokenFileSystem,
{
    let raw_path = os_string_to_string(value, TOKEN_FILE_ENV)?;
    let path = validate_token_file_path(&raw_path)?;
    let info = file_system
        .inspect(&path)
        .map_err(|()| ConfigError::TokenFileUnreadable)?;

    if info.kind != FileKind::Regular {
        return Err(ConfigError::InvalidTokenFileKind);
    }
    if info.len > TOKEN_MAX_BYTES as u64 {
        return Err(ConfigError::TokenFileTooLarge);
    }

    let contents = file_system
        .read_limited(&path, TOKEN_MAX_BYTES)
        .map_err(|()| ConfigError::TokenFileUnreadable)?;
    if contents.len() > TOKEN_MAX_BYTES {
        return Err(ConfigError::TokenFileTooLarge);
    }

    let mut token = String::from_utf8(contents).map_err(|_| ConfigError::InvalidToken)?;
    if token.ends_with("\r\n") {
        token.truncate(token.len() - 2);
    } else if token.ends_with('\n') {
        token.truncate(token.len() - 1);
    }
    validate_token(token)
}

fn validate_token_file_path(raw_path: &str) -> Result<PathBuf, ConfigError> {
    if raw_path.is_empty()
        || contains_forbidden_control(raw_path)
        || raw_path.starts_with(r"\\")
        || raw_path.starts_with("//")
    {
        return Err(ConfigError::InvalidTokenFilePath);
    }

    let path = PathBuf::from(raw_path);
    if !path.is_absolute() || has_reserved_windows_component(&path) {
        return Err(ConfigError::InvalidTokenFilePath);
    }

    Ok(path)
}

#[cfg(windows)]
fn has_reserved_windows_component(path: &Path) -> bool {
    path.components().any(|component| {
        let std::path::Component::Normal(name) = component else {
            return false;
        };
        let name = name.to_string_lossy();
        let base = name
            .trim_end_matches([' ', '.'])
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || base.strip_prefix("COM").is_some_and(|suffix| {
                matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
            || base.strip_prefix("LPT").is_some_and(|suffix| {
                matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
    })
}

#[cfg(not(windows))]
fn has_reserved_windows_component(_path: &Path) -> bool {
    false
}

fn validate_token(token: String) -> Result<SecretString, ConfigError> {
    let token = SecretString::from(token);
    let exposed = token.expose_secret();
    if exposed.is_empty()
        || exposed.len() > TOKEN_MAX_BYTES
        || contains_forbidden_control(exposed)
        || exposed.trim() != exposed
    {
        return Err(ConfigError::InvalidToken);
    }

    Ok(token)
}

fn read_usize<E>(
    environment: &E,
    name: &'static str,
    default: usize,
    minimum: usize,
    maximum: usize,
) -> Result<usize, ConfigError>
where
    E: Environment,
{
    let Some(value) = environment.get(name) else {
        return Ok(default);
    };
    let value = os_string_to_string(value, name)?;
    let parsed = value
        .parse::<usize>()
        .map_err(|_| ConfigError::InvalidInteger { name })?;
    if !(minimum..=maximum).contains(&parsed) {
        return Err(ConfigError::InvalidSetting { name });
    }
    Ok(parsed)
}

fn read_bounded_text<E>(
    environment: &E,
    name: &'static str,
    default: &str,
    minimum_chars: usize,
    maximum_chars: usize,
) -> Result<String, ConfigError>
where
    E: Environment,
{
    let value = read_string(environment, name, default)?;
    let length = value.chars().count();
    if !(minimum_chars..=maximum_chars).contains(&length) || contains_forbidden_control(&value) {
        return Err(ConfigError::InvalidSetting { name });
    }
    Ok(value)
}

fn read_string<E>(environment: &E, name: &'static str, default: &str) -> Result<String, ConfigError>
where
    E: Environment,
{
    environment.get(name).map_or_else(
        || Ok(default.to_owned()),
        |value| os_string_to_string(value, name),
    )
}

fn os_string_to_string(value: OsString, name: &'static str) -> Result<String, ConfigError> {
    value
        .into_string()
        .map_err(|_| ConfigError::NonUnicodeValue { name })
}

fn contains_forbidden_control(value: &str) -> bool {
    value.contains(['\0', '\r', '\n'])
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use secrecy::ExposeSecret;

    use super::{CallMode, Config, Environment, FileKind, TokenFileInfo, TokenFileSystem};
    use crate::error::ConfigError;
    use crate::limits::{TOKEN_MAX_BYTES, TOOL_INPUT_MAX_HARD};

    #[derive(Default)]
    struct FakeEnvironment {
        values: BTreeMap<&'static str, OsString>,
    }

    impl FakeEnvironment {
        fn with(mut self, name: &'static str, value: impl Into<OsString>) -> Self {
            self.values.insert(name, value.into());
            self
        }
    }

    impl Environment for FakeEnvironment {
        fn get(&self, name: &'static str) -> Option<OsString> {
            self.values.get(name).cloned()
        }
    }

    struct FakeFileSystem {
        info: Result<TokenFileInfo, ()>,
        contents: Result<Vec<u8>, ()>,
        inspections: Cell<usize>,
        reads: Cell<usize>,
    }

    impl FakeFileSystem {
        fn regular(contents: impl Into<Vec<u8>>) -> Self {
            let contents = contents.into();
            Self {
                info: Ok(TokenFileInfo {
                    kind: FileKind::Regular,
                    len: contents.len() as u64,
                }),
                contents: Ok(contents),
                inspections: Cell::new(0),
                reads: Cell::new(0),
            }
        }

        fn kind(kind: FileKind) -> Self {
            Self {
                info: Ok(TokenFileInfo { kind, len: 0 }),
                contents: Ok(Vec::new()),
                inspections: Cell::new(0),
                reads: Cell::new(0),
            }
        }
    }

    impl TokenFileSystem for FakeFileSystem {
        fn inspect(&self, _path: &Path) -> Result<TokenFileInfo, ()> {
            self.inspections.set(self.inspections.get() + 1);
            self.info
        }

        fn read_limited(&self, _path: &Path, _limit: usize) -> Result<Vec<u8>, ()> {
            self.reads.set(self.reads.get() + 1);
            self.contents.clone()
        }
    }

    fn valid_token_environment() -> FakeEnvironment {
        FakeEnvironment::default().with("ONEC_AI_TOKEN", "test-token")
    }

    #[cfg(windows)]
    fn valid_token_path() -> PathBuf {
        PathBuf::from(r"C:\tokens\onec-ai.txt")
    }

    #[cfg(not(windows))]
    fn valid_token_path() -> PathBuf {
        PathBuf::from("/tokens/onec-ai.txt")
    }

    fn file_environment() -> FakeEnvironment {
        FakeEnvironment::default().with("ONEC_AI_TOKEN_FILE", valid_token_path().into_os_string())
    }

    #[test]
    fn environment_token_and_defaults_are_loaded() {
        let file_system = FakeFileSystem::regular(Vec::new());
        let config =
            Config::load_with(&valid_token_environment(), &file_system).expect("valid config");

        assert_eq!(config.token().expose_secret(), "test-token");
        assert_eq!(config.tool_input_min_length(), 4);
        assert_eq!(config.tool_input_max_length(), 100_000);
        assert_eq!(config.call_mode(), CallMode::Direct);
        assert_eq!(config.ui_language(), "russian");
        assert_eq!(config.programming_language(), "");
        assert_eq!(config.default_ssl_version(), "");
        assert_eq!(config.default_1c_configuration(), "");
        assert_eq!(config.max_concurrent_calls(), 2);
        assert_eq!(config.rust_log(), "warn");
        assert_eq!(file_system.inspections.get(), 0);
        assert_eq!(file_system.reads.get(), 0);
    }

    #[test]
    fn config_debug_output_redacts_the_token() {
        let secret = "DO_NOT_LEAK_TOKEN";
        let environment = FakeEnvironment::default().with("ONEC_AI_TOKEN", secret);
        let config = Config::load_with(&environment, &FakeFileSystem::regular(Vec::new()))
            .expect("valid config");

        let rendered = format!("{config:?}");
        assert!(rendered.contains("[REDACTED]"));
        assert!(!rendered.contains(secret));
    }

    #[test]
    fn exactly_one_token_source_is_required() {
        let file_system = FakeFileSystem::regular(b"file-token".to_vec());

        let missing = Config::load_with(&FakeEnvironment::default(), &file_system);
        assert_eq!(missing.unwrap_err(), ConfigError::TokenSourceCount);

        let both = valid_token_environment()
            .with("ONEC_AI_TOKEN_FILE", valid_token_path().into_os_string());
        assert_eq!(
            Config::load_with(&both, &file_system).unwrap_err(),
            ConfigError::TokenSourceCount
        );
        assert_eq!(file_system.reads.get(), 0);
    }

    #[test]
    fn environment_token_rejects_invalid_content_without_exposing_it() {
        let invalid_tokens = [
            String::new(),
            " leading".to_owned(),
            "trailing ".to_owned(),
            "line\nbreak".to_owned(),
            "line\rbreak".to_owned(),
            "nul\0byte".to_owned(),
            "x".repeat(TOKEN_MAX_BYTES + 1),
        ];

        for token in invalid_tokens {
            let environment = FakeEnvironment::default().with("ONEC_AI_TOKEN", token.clone());
            let error =
                Config::load_with(&environment, &FakeFileSystem::regular(Vec::new())).unwrap_err();
            let rendered = format!("{error:?} {error}");

            assert_eq!(error, ConfigError::InvalidToken);
            if !token.is_empty() {
                assert!(!rendered.contains(&token));
            }
        }
    }

    #[test]
    fn token_file_allows_one_trailing_crlf_and_is_read_once() {
        let file_system = FakeFileSystem::regular(b"file-token\r\n".to_vec());
        let config =
            Config::load_with(&file_environment(), &file_system).expect("valid file config");

        assert_eq!(config.token().expose_secret(), "file-token");
        assert_eq!(file_system.inspections.get(), 1);
        assert_eq!(file_system.reads.get(), 1);
    }

    #[test]
    fn token_sources_accept_the_exact_eight_kib_boundary() {
        let exact_token = "x".repeat(TOKEN_MAX_BYTES);
        let environment = FakeEnvironment::default().with("ONEC_AI_TOKEN", exact_token.clone());
        let environment_config =
            Config::load_with(&environment, &FakeFileSystem::regular(Vec::new()))
                .expect("an environment token at the exact limit is valid");
        assert_eq!(environment_config.token().expose_secret(), exact_token);

        let file_system = FakeFileSystem::regular(exact_token.as_bytes().to_vec());
        let file_config = Config::load_with(&file_environment(), &file_system)
            .expect("a token file at the exact limit is valid");
        assert_eq!(file_config.token().expose_secret(), exact_token);
        assert_eq!(file_system.reads.get(), 1);
    }

    #[test]
    fn token_file_rejects_non_local_or_non_absolute_paths_before_inspection() {
        for path in [
            "token.txt",
            r"\\server\share\token.txt",
            "//server/share/token.txt",
            r"\\?\C:\tokens\token.txt",
            r"\\.\NUL",
        ] {
            let environment = FakeEnvironment::default().with("ONEC_AI_TOKEN_FILE", path);
            let file_system = FakeFileSystem::regular(b"token".to_vec());

            assert_eq!(
                Config::load_with(&environment, &file_system).unwrap_err(),
                ConfigError::InvalidTokenFilePath
            );
            assert_eq!(file_system.inspections.get(), 0);
            assert_eq!(file_system.reads.get(), 0);
        }
    }

    #[test]
    fn token_file_rejects_directories_links_and_other_objects() {
        for kind in [FileKind::Directory, FileKind::Link, FileKind::Other] {
            let file_system = FakeFileSystem::kind(kind);

            assert_eq!(
                Config::load_with(&file_environment(), &file_system).unwrap_err(),
                ConfigError::InvalidTokenFileKind
            );
            assert_eq!(file_system.reads.get(), 0);
        }
    }

    #[test]
    fn token_file_rejects_oversize_before_reading() {
        let file_system = FakeFileSystem {
            info: Ok(TokenFileInfo {
                kind: FileKind::Regular,
                len: (TOKEN_MAX_BYTES + 1) as u64,
            }),
            contents: Ok(vec![b'x'; TOKEN_MAX_BYTES + 1]),
            inspections: Cell::new(0),
            reads: Cell::new(0),
        };

        assert_eq!(
            Config::load_with(&file_environment(), &file_system).unwrap_err(),
            ConfigError::TokenFileTooLarge
        );
        assert_eq!(file_system.reads.get(), 0);
    }

    #[test]
    fn token_file_rejects_invalid_or_changed_content() {
        let invalid_contents = [
            (vec![0xff], ConfigError::InvalidToken),
            (b"line\nbreak".to_vec(), ConfigError::InvalidToken),
            (b"line\rbreak".to_vec(), ConfigError::InvalidToken),
            (b"nul\0byte".to_vec(), ConfigError::InvalidToken),
            (b" leading".to_vec(), ConfigError::InvalidToken),
            (b"trailing ".to_vec(), ConfigError::InvalidToken),
            (b"token\n\n".to_vec(), ConfigError::InvalidToken),
            (
                vec![b'x'; TOKEN_MAX_BYTES + 1],
                ConfigError::TokenFileTooLarge,
            ),
        ];

        for (contents, expected_error) in invalid_contents {
            let file_system = FakeFileSystem {
                info: Ok(TokenFileInfo {
                    kind: FileKind::Regular,
                    len: contents.len().min(TOKEN_MAX_BYTES) as u64,
                }),
                contents: Ok(contents),
                inspections: Cell::new(0),
                reads: Cell::new(0),
            };

            assert_eq!(
                Config::load_with(&file_environment(), &file_system).unwrap_err(),
                expected_error
            );
            assert_eq!(file_system.reads.get(), 1);
        }
    }

    #[test]
    fn valid_optional_settings_are_preserved() {
        let environment = valid_token_environment()
            .with("MCP_TOOL_INPUT_MIN_LENGTH", "5")
            .with("MCP_TOOL_INPUT_MAX_LENGTH", "999")
            .with("MCP_TOOL_CALL_MODE", "standard")
            .with("ONEC_AI_UI_LANGUAGE", "english")
            .with("ONEC_AI_PROGRAMMING_LANGUAGE", "bsl")
            .with("DEFAULT_SSL_VERSION", "TLS 1.3")
            .with("DEFAULT_1C_CONFIGURATION", "ERP")
            .with("MCP_MAX_CONCURRENT_CALLS", "1")
            .with("RUST_LOG", "onec_buddy_mcp=debug,warn");
        let config = Config::load_with(&environment, &FakeFileSystem::regular(Vec::new()))
            .expect("valid optional settings");

        assert_eq!(config.tool_input_min_length(), 5);
        assert_eq!(config.tool_input_max_length(), 999);
        assert_eq!(config.call_mode(), CallMode::Standard);
        assert_eq!(config.ui_language(), "english");
        assert_eq!(config.programming_language(), "bsl");
        assert_eq!(config.default_ssl_version(), "TLS 1.3");
        assert_eq!(config.default_1c_configuration(), "ERP");
        assert_eq!(config.max_concurrent_calls(), 1);
        assert_eq!(config.rust_log(), "onec_buddy_mcp=debug,warn");
    }

    #[test]
    fn invalid_optional_settings_are_rejected() {
        let cases = [
            ("MCP_TOOL_INPUT_MIN_LENGTH", "0"),
            ("MCP_TOOL_INPUT_MIN_LENGTH", "not-a-number"),
            (
                "MCP_TOOL_INPUT_MAX_LENGTH",
                &(TOOL_INPUT_MAX_HARD + 1).to_string(),
            ),
            ("MCP_TOOL_CALL_MODE", "automatic"),
            ("ONEC_AI_UI_LANGUAGE", ""),
            ("ONEC_AI_UI_LANGUAGE", &"я".repeat(65)),
            ("ONEC_AI_PROGRAMMING_LANGUAGE", &"x".repeat(65)),
            ("DEFAULT_SSL_VERSION", &"x".repeat(257)),
            ("DEFAULT_1C_CONFIGURATION", "line\nbreak"),
            ("MCP_MAX_CONCURRENT_CALLS", "3"),
            ("RUST_LOG", ""),
            ("RUST_LOG", "not a filter ==="),
        ];

        for (name, value) in cases {
            let environment = valid_token_environment().with(name, value);
            assert!(
                Config::load_with(&environment, &FakeFileSystem::regular(Vec::new())).is_err(),
                "{name}={value:?} must be rejected"
            );
        }

        let inverted = valid_token_environment()
            .with("MCP_TOOL_INPUT_MIN_LENGTH", "10")
            .with("MCP_TOOL_INPUT_MAX_LENGTH", "9");
        assert_eq!(
            Config::load_with(&inverted, &FakeFileSystem::regular(Vec::new())).unwrap_err(),
            ConfigError::InvertedToolInputLimits
        );
    }

    #[test]
    fn production_base_url_override_is_rejected_without_exposing_values() {
        let secret = "do-not-leak-token";
        let address = "https://attacker.invalid/private";
        let environment = FakeEnvironment::default()
            .with("ONEC_AI_TOKEN", secret)
            .with("ONEC_AI_BASE_URL", address);
        let error =
            Config::load_with(&environment, &FakeFileSystem::regular(Vec::new())).unwrap_err();
        let rendered = format!("{error:?} {error}");

        assert_eq!(error, ConfigError::UnsupportedBaseUrl);
        assert!(!rendered.contains(secret));
        assert!(!rendered.contains(address));
    }
}
