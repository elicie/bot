//! Vault secrets ("보관함" / "시크릿") and custom MCP servers per bot ("사용자 지정 MCP").
//!
//! Secrets: named credentials for every bot or one bot. Values are write-only from the browser:
//! they are stored in `state.json` (mode 0600, like the provider API key; there is no OS keychain
//! integration), never placed in a public snapshot, and redacted from every snapshot string
//! wherever they appear ([`State::redaction_keys`]). They reach only the custom MCP server
//! environment or headers that reference them as `${secret:NAME}`, resolved when a session starts
//! ([`State::resolve_mcp_servers`]). They are never put in a model prompt.
//!
//! Custom MCP servers: the bot's "MCP 서버 JSON" document in the common
//! `{"mcpServers": {"name": {"command", "args", "env"} | {"url", "headers"}}}` shape, validated
//! strictly and stored canonically. A stdio server is arbitrary command execution on the server
//! computer, so creating, editing, and enabling servers (and every secret change) takes a
//! loopback client (`local`); the server has no login. New and changed servers start disabled.
//! Disabling is a tightening and is allowed from any client.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::sync::LazyLock;

use grok_bot_model::{
    DiskState, HttpError, HttpResult, McpPair, McpServerConfig, McpServerRecord, McpTransportKind, PublicMcpServer, PublicSecret, SecretInput, VaultSecret,
    iso, new_id,
};
use regex::Regex;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::State;
use crate::validate::{is_timestamp, is_uuid, js_len, js_trim};

pub const MAX_SECRETS: usize = 200;
pub const MIN_SECRET_CHARS: usize = 8;
pub const MAX_SECRET_CHARS: usize = 8192;
pub const MAX_SERVERS_PER_BOT: usize = 10;
pub const MAX_MCP_JSON_BYTES: usize = 64 * 1024;
const MAX_COMMAND_CHARS: usize = 1024;
const MAX_ARGS: usize = 64;
const MAX_ARG_CHARS: usize = 4096;
const MAX_PAIRS: usize = 64;
const MAX_PAIR_VALUE_CHARS: usize = 8192;
const MAX_URL_CHARS: usize = 2048;

pub const VAULT_REQUIRES_LOOPBACK: &str =
    "보관함 시크릿은 서버 컴퓨터의 localhost 연결에서만 추가·교체·삭제할 수 있어요. 이 네트워크 서버에는 사용자 인증이 없습니다.";
pub const MCP_REQUIRES_LOOPBACK: &str =
    "사용자 지정 MCP 서버는 서버 컴퓨터의 localhost 연결에서만 만들고, 편집하고, 켤 수 있어요. stdio 서버는 서버 컴퓨터에서 명령을 실행합니다.";

static SECRET_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Z][A-Z0-9_]{0,63}$").unwrap());
static SERVER_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9_-]{0,31}$").unwrap());
static ENV_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]{0,127}$").unwrap());
static HEADER_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9!#$%&'*+.^_`|~-]{1,128}$").unwrap());
/// `${secret:NAME}` references inside an env or header value.
static SECRET_REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{secret:([^}]*)\}").unwrap());

fn bad(message: impl Into<String>) -> HttpError {
    HttpError::bad_request(message)
}

fn require_local(local: bool, message: &str) -> HttpResult<()> {
    if local { Ok(()) } else { Err(HttpError::forbidden(message)) }
}

/// Whether `name` may name a custom MCP server: `^[a-z0-9_-]{1,32}$`, starting with a letter or
/// digit, without `__` (the engine joins server and tool names with `__`, so a name such as
/// `workspace__x` would produce `workspace__x__tool`, which the engine's `MCPTool(workspace__*)`
/// allow rule would run without asking), and not `workspace…` (the app's own broker).
pub fn server_name_valid(name: &str) -> bool {
    SERVER_NAME.is_match(name) && !name.contains("__") && !name.starts_with("workspace")
}

pub fn secret_name_valid(name: &str) -> bool {
    SECRET_NAME.is_match(name)
}

fn control(value: &str) -> bool {
    value.chars().any(char::is_control)
}

/// Secret names referenced by `value`, in order. `Err` names a malformed reference.
fn references(value: &str) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for capture in SECRET_REF.captures_iter(value) {
        let name = &capture[1];
        if !secret_name_valid(name) {
            return Err(format!("${{secret:{name}}}: 시크릿 이름은 문자로 시작하는 대문자 환경 변수 이름(A-Z, 0-9, _)이어야 해요."));
        }
        names.push(name.to_owned());
    }
    Ok(names)
}

fn config_references(config: &McpServerConfig) -> Vec<String> {
    let pairs = match config {
        McpServerConfig::Stdio { env, .. } => env,
        McpServerConfig::Http { headers, .. } => headers,
    };
    let mut names: Vec<String> = Vec::new();
    for pair in pairs {
        for name in references(&pair.value).unwrap_or_default() {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

// ---------------------------------------------------------------------------------------------
// Strict JSON: like `serde_json::Value`, but a repeated object key is an error instead of
// silently keeping the last one (two servers with the same name must not collapse into one).

struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StrictVisitor;
        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = StrictValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON")
            }
            fn visit_bool<E>(self, v: bool) -> Result<StrictValue, E> {
                Ok(StrictValue(Value::Bool(v)))
            }
            fn visit_i64<E>(self, v: i64) -> Result<StrictValue, E> {
                Ok(StrictValue(Value::from(v)))
            }
            fn visit_u64<E>(self, v: u64) -> Result<StrictValue, E> {
                Ok(StrictValue(Value::from(v)))
            }
            fn visit_f64<E>(self, v: f64) -> Result<StrictValue, E> {
                Ok(StrictValue(serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number)))
            }
            fn visit_str<E>(self, v: &str) -> Result<StrictValue, E> {
                Ok(StrictValue(Value::String(v.to_owned())))
            }
            fn visit_string<E>(self, v: String) -> Result<StrictValue, E> {
                Ok(StrictValue(Value::String(v)))
            }
            fn visit_unit<E>(self) -> Result<StrictValue, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_none<E>(self) -> Result<StrictValue, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<StrictValue, A::Error> {
                let mut items = Vec::new();
                while let Some(StrictValue(item)) = seq.next_element()? {
                    items.push(item);
                }
                Ok(StrictValue(Value::Array(items)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<StrictValue, A::Error> {
                let mut object = Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    let StrictValue(value) = map.next_value()?;
                    if object.contains_key(&key) {
                        return Err(de::Error::custom(format!("duplicate key \"{key}\"")));
                    }
                    object.insert(key, value);
                }
                Ok(StrictValue(Value::Object(object)))
            }
        }
        deserializer.deserialize_any(StrictVisitor)
    }
}

// ---------------------------------------------------------------------------------------------
// "MCP 서버 JSON"

fn pairs(server: &str, field: &str, value: Option<&Value>, header: bool) -> HttpResult<Vec<McpPair>> {
    let Some(value) = value else { return Ok(Vec::new()) };
    let Value::Object(object) = value else {
        return Err(bad(format!("\"{server}\".{field}: 이름과 문자열 값으로 된 객체여야 해요.")));
    };
    if object.len() > MAX_PAIRS {
        return Err(bad(format!("\"{server}\".{field}: 항목은 {MAX_PAIRS}개까지 넣을 수 있어요.")));
    }
    let mut seen = HashSet::new();
    let mut result: Vec<McpPair> = Vec::new();
    for (name, value) in object {
        let Value::String(text) = value else {
            return Err(bad(format!("\"{server}\".{field}.{name}: 값은 문자열이어야 해요.")));
        };
        let name_ok = if header { HEADER_NAME.is_match(name) } else { ENV_NAME.is_match(name) };
        if !name_ok {
            return Err(bad(if header {
                format!("\"{server}\".headers: \"{name}\"은(는) 올바른 HTTP 헤더 이름이 아니에요.")
            } else {
                format!("\"{server}\".env: \"{name}\"은(는) 올바른 환경 변수 이름이 아니에요(A-Z, a-z, 0-9, _).")
            }));
        }
        if !seen.insert(name.to_ascii_lowercase()) && header {
            return Err(bad(format!("\"{server}\".headers: \"{name}\" 헤더가 중복되었어요.")));
        }
        if js_len(text) > MAX_PAIR_VALUE_CHARS || control(text) {
            return Err(bad(format!("\"{server}\".{field}.{name}: 값은 제어 문자 없이 {MAX_PAIR_VALUE_CHARS}자 이하여야 해요.")));
        }
        references(text).map_err(|message| bad(format!("\"{server}\".{field}.{name}: {message}")))?;
        result.push(McpPair { name: name.clone(), value: text.clone() });
    }
    result.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(result)
}

fn no_secret_reference(server: &str, field: &str, value: &str) -> HttpResult<()> {
    if value.contains("${secret:") {
        return Err(bad(format!("\"{server}\".{field}: 시크릿은 env 또는 headers 값에서만 참조할 수 있어요.")));
    }
    Ok(())
}

#[derive(Clone, Copy, Default)]
enum McpField {
    #[default]
    Json,
    Name,
    Transport,
    Command,
    Args,
    Env,
    Url,
    Headers,
}

impl McpField {
    /// Static field labels and recovery hints: never echo peer-supplied keys, URLs or values.
    fn message(self) -> &'static str {
        match self {
            Self::Json => "MCP 구성 JSON(json): 서버 이름을 키로 하는 객체와 지원하는 항목을 확인해 주세요. JSON은 64 KiB 이하여야 해요.",
            Self::Name => "서버 이름(name): 소문자·숫자로 시작하는 32자 이하 이름을 쓰고, __와 workspace로 시작하는 이름은 피해주세요.",
            Self::Transport => "서버 종류(type): 로컬 명령(command·args·env) 또는 원격 URL(url·headers) 중 하나만 입력해 주세요.",
            Self::Command => {
                "명령(command): PATH의 실행 파일 이름이나 절대 경로를 입력해 주세요. 비어 있거나 시크릿 참조·제어 문자를 포함할 수 없고 최대 1,024자예요."
            }
            Self::Args => "인자(args): 빈 칸은 삭제하거나 값을 입력해 주세요. 인자는 최대 64개, 각 4,096자 이하의 문자열이며 시크릿 참조와 NUL은 쓸 수 없어요.",
            Self::Env => "환경 변수(env): 항목 이름·값·시크릿 참조를 확인해 주세요. 최대 64개, 값은 제어 문자 없이 8,192자 이하여야 해요.",
            Self::Url => "서버 URL(url): 인증 정보와 # 없이 http:// 또는 https://로 시작하는 2,048자 이하 주소를 입력해 주세요.",
            Self::Headers => {
                "헤더(headers): 이름·값·시크릿 참조와 대소문자 구분 없는 중복을 확인해 주세요. 최대 64개, 값은 제어 문자 없이 8,192자 이하여야 해요."
            }
        }
    }
}

fn server_config(name: &str, value: &Value, field: &mut McpField) -> HttpResult<McpServerConfig> {
    *field = McpField::Json;
    let Value::Object(entry) = value else {
        return Err(bad(format!("서버 \"{name}\"의 구성이 올바르지 않아요. 객체여야 해요.")));
    };
    for key in entry.keys() {
        if !matches!(key.as_str(), "command" | "args" | "env" | "url" | "headers" | "type") {
            return Err(bad(format!("서버 \"{name}\"의 구성이 올바르지 않아요. 알 수 없는 항목 \"{key}\"이(가) 있어요.")));
        }
    }
    *field = McpField::Transport;
    let kind = match entry.get("type") {
        None => None,
        Some(Value::String(kind)) => match kind.as_str() {
            "stdio" => Some(McpTransportKind::Stdio),
            "http" | "streamable-http" | "streamableHttp" => Some(McpTransportKind::Http),
            other => return Err(bad(format!("서버 \"{name}\"의 type \"{other}\"은(는) 지원하지 않아요. stdio 또는 http를 사용하세요."))),
        },
        Some(_) => return Err(bad(format!("서버 \"{name}\"의 type은 문자열이어야 해요."))),
    };
    let (command, url) = (entry.get("command"), entry.get("url"));
    match (command, url) {
        (Some(_), Some(_)) => Err(bad(format!("서버 \"{name}\"에는 URL과 명령 중 하나만 넣어 주세요."))),
        (None, None) => Err(bad(format!("서버에는 URL 또는 명령이 필요해요. (\"{name}\")"))),
        (Some(command), None) => {
            if kind == Some(McpTransportKind::Http) || entry.contains_key("headers") {
                return Err(bad(format!("서버 \"{name}\": 명령(stdio) 서버에는 type http나 headers를 쓸 수 없어요.")));
            }
            *field = McpField::Command;
            let Value::String(command) = command else {
                return Err(bad(format!("\"{name}\".command: 문자열이어야 해요.")));
            };
            let command = js_trim(command);
            if command.is_empty() || js_len(command) > MAX_COMMAND_CHARS || control(command) {
                return Err(bad(format!("\"{name}\".command: 비어 있지 않고 제어 문자 없이 {MAX_COMMAND_CHARS}자 이하여야 해요.")));
            }
            no_secret_reference(name, "command", command)?;
            if command.contains('/') && !command.starts_with('/') {
                return Err(bad(format!("\"{name}\".command: 명령은 절대 경로이거나 PATH에서 찾을 이름이어야 해요.")));
            }
            *field = McpField::Args;
            let args = match entry.get("args") {
                None => Vec::new(),
                Some(Value::Array(items)) => {
                    if items.len() > MAX_ARGS {
                        return Err(bad(format!("\"{name}\".args: 인수는 {MAX_ARGS}개까지 넣을 수 있어요.")));
                    }
                    let mut args = Vec::new();
                    for item in items {
                        let Value::String(arg) = item else {
                            return Err(bad(format!("\"{name}\".args: 모든 인수는 문자열이어야 해요.")));
                        };
                        if js_len(arg) > MAX_ARG_CHARS || arg.contains('\0') {
                            return Err(bad(format!("\"{name}\".args: 인수는 {MAX_ARG_CHARS}자 이하여야 해요.")));
                        }
                        no_secret_reference(name, "args", arg)?;
                        args.push(arg.clone());
                    }
                    args
                }
                Some(_) => return Err(bad(format!("\"{name}\".args: 문자열 배열이어야 해요."))),
            };
            *field = McpField::Env;
            let env = pairs(name, "env", entry.get("env"), false)?;
            Ok(McpServerConfig::Stdio { command: command.to_owned(), args, env })
        }
        (None, Some(url)) => {
            if kind == Some(McpTransportKind::Stdio) || entry.contains_key("args") || entry.contains_key("env") {
                return Err(bad(format!("서버 \"{name}\": URL(http) 서버에는 type stdio, args, env를 쓸 수 없어요.")));
            }
            *field = McpField::Url;
            let Value::String(url) = url else {
                return Err(bad(format!("\"{name}\".url: 문자열이어야 해요.")));
            };
            let url = js_trim(url);
            no_secret_reference(name, "url", url)?;
            let parsed = url::Url::parse(url).ok().filter(|parsed| {
                matches!(parsed.scheme(), "http" | "https")
                    && parsed.host_str().is_some()
                    && parsed.username().is_empty()
                    && parsed.password().is_none()
                    && parsed.fragment().is_none()
            });
            if parsed.is_none() || js_len(url) > MAX_URL_CHARS {
                return Err(bad(format!("\"{name}\".url: 인증 정보와 # 없이 http:// 또는 https://로 시작하는 {MAX_URL_CHARS}자 이하의 서버 URL이어야 해요.")));
            }
            *field = McpField::Headers;
            let headers = pairs(name, "headers", entry.get("headers"), true)?;
            Ok(McpServerConfig::Http { url: url.to_owned(), headers })
        }
    }
}

/// Parse and validate an "MCP 서버 JSON" document: `{"mcpServers": {...}}` or the bare server
/// object. Returns the servers sorted by name.
pub fn parse_mcp_json(text: &str) -> HttpResult<Vec<(String, McpServerConfig)>> {
    parse_mcp_json_field(text, &mut McpField::Json)
}

fn parse_mcp_json_field(text: &str, field: &mut McpField) -> HttpResult<Vec<(String, McpServerConfig)>> {
    *field = McpField::Json;
    if text.len() > MAX_MCP_JSON_BYTES {
        return Err(HttpError::too_large(format!("MCP 서버 JSON은 {} KiB 이하여야 해요.", MAX_MCP_JSON_BYTES / 1024)));
    }
    let StrictValue(document) = serde_json::from_str::<StrictValue>(text).map_err(|error| {
        if error.to_string().starts_with("duplicate key") {
            bad(format!("서버 이름이 중복되었어요. {error}"))
        } else {
            bad(format!("구성이 올바른 JSON이 아니에요. {error}"))
        }
    })?;
    let Value::Object(mut top) = document else {
        return Err(bad("JSON은 서버 이름을 키로 하는 객체여야 해요"));
    };
    let servers = if top.len() == 1 && top.contains_key("mcpServers") {
        match top.remove("mcpServers") {
            Some(Value::Object(servers)) => servers,
            _ => return Err(bad("mcpServers는 서버 이름을 키로 하는 객체여야 해요")),
        }
    } else if top.contains_key("mcpServers") {
        return Err(bad("mcpServers 옆에는 다른 항목을 둘 수 없어요."));
    } else {
        top
    };
    if servers.len() > MAX_SERVERS_PER_BOT {
        return Err(bad(format!("Bot 하나에 MCP 서버는 {MAX_SERVERS_PER_BOT}개까지 추가할 수 있어요.")));
    }
    let mut parsed = BTreeMap::new();
    for (name, value) in &servers {
        *field = McpField::Name;
        let trimmed = js_trim(name);
        if trimmed.is_empty() {
            return Err(bad("서버 이름은 비워 둘 수 없어요"));
        }
        if !server_name_valid(trimmed) {
            return Err(bad(format!(
                "서버 이름 \"{trimmed}\"을(를) 쓸 수 없어요. 소문자, 숫자, -, _로 된 32자 이하 이름을 쓰고(__ 제외), workspace로 시작하지 않게 해 주세요."
            )));
        }
        let config = server_config(trimmed, value, field)?;
        *field = McpField::Name;
        if parsed.insert(trimmed.to_owned(), config).is_some() {
            return Err(bad(format!("서버 이름 \"{trimmed}\"이(가) 중복되었어요.")));
        }
    }
    Ok(parsed.into_iter().collect())
}

/// The canonical "MCP 서버 JSON" of `servers` (pretty-printed, sorted).
pub fn mcp_json<'a>(servers: impl IntoIterator<Item = &'a McpServerRecord>) -> String {
    let pairs = |pairs: &[McpPair]| Value::Object(pairs.iter().map(|pair| (pair.name.clone(), Value::String(pair.value.clone()))).collect());
    let mut entries: Vec<&McpServerRecord> = servers.into_iter().collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let mut object = Map::new();
    for server in entries {
        let mut entry = Map::new();
        match &server.config {
            McpServerConfig::Stdio { command, args, env } => {
                entry.insert("command".into(), Value::String(command.clone()));
                if !args.is_empty() {
                    entry.insert("args".into(), Value::Array(args.iter().cloned().map(Value::String).collect()));
                }
                if !env.is_empty() {
                    entry.insert("env".into(), pairs(env));
                }
            }
            McpServerConfig::Http { url, headers } => {
                entry.insert("url".into(), Value::String(url.clone()));
                if !headers.is_empty() {
                    entry.insert("headers".into(), pairs(headers));
                }
            }
        }
        object.insert(server.name.clone(), Value::Object(entry));
    }
    let document = serde_json::json!({ "mcpServers": Value::Object(object) });
    serde_json::to_string_pretty(&document).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Session start

/// An enabled custom server with its secret references resolved. Never serialized to a client.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedMcpServer {
    pub name: String,
    pub config: McpServerConfig,
}

impl fmt::Debug for ResolvedMcpServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Resolved values can hold secrets; keep them out of debug output.
        f.debug_struct("ResolvedMcpServer").field("name", &self.name).finish_non_exhaustive()
    }
}

/// Resolve references without changing the configuration or vault. Both runtime startup and the
/// draft probe use this pure resolver, with their own scope-aware lookup.
pub fn resolve_mcp_config<'a>(config: &McpServerConfig, lookup: impl Fn(&str) -> Option<&'a str>) -> Result<McpServerConfig, String> {
    let resolve = |pairs: &[McpPair]| -> Result<Vec<McpPair>, String> {
        pairs
            .iter()
            .map(|pair| {
                let mut missing = None;
                let value = SECRET_REF.replace_all(&pair.value, |capture: &regex::Captures<'_>| match lookup(&capture[1]) {
                    Some(value) => value.to_owned(),
                    None => {
                        missing.get_or_insert_with(|| capture[1].to_owned());
                        String::new()
                    }
                });
                match missing {
                    Some(name) => Err(format!("보관함에 {name} 시크릿이 없어요. 보관함에 추가한 뒤 다시 시도하세요.")),
                    None => Ok(McpPair { name: pair.name.clone(), value: value.into_owned() }),
                }
            })
            .collect()
    };
    match config {
        McpServerConfig::Stdio { command, args, env } => resolve(env).map(|env| McpServerConfig::Stdio { command: command.clone(), args: args.clone(), env }),
        McpServerConfig::Http { url, headers } => resolve(headers).map(|headers| McpServerConfig::Http { url: url.clone(), headers }),
    }
}

/// An enabled server that could not be prepared (a referenced secret is missing).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpStartupFailure {
    pub name: String,
    pub message: String,
}

fn public_server(server: &McpServerRecord) -> PublicMcpServer {
    PublicMcpServer {
        id: server.id.clone(),
        bot_id: server.bot_id.clone(),
        name: server.name.clone(),
        transport: match server.config {
            McpServerConfig::Stdio { .. } => McpTransportKind::Stdio,
            McpServerConfig::Http { .. } => McpTransportKind::Http,
        },
        enabled: server.enabled,
        secret_refs: config_references(&server.config),
        created_at: server.created_at.clone(),
        updated_at: server.updated_at.clone(),
    }
}

/// Public projections for the snapshot.
pub(crate) fn public_vault(disk: &DiskState) -> (Vec<PublicSecret>, Vec<PublicMcpServer>) {
    (disk.secrets.iter().map(PublicSecret::from).collect(), disk.mcp_servers.iter().map(public_server).collect())
}

/// Every credential a public string must not contain: the provider API key and every vault
/// secret value, longest first (so a secret that contains another is replaced whole).
pub(crate) fn redaction_keys(disk: &DiskState) -> Vec<String> {
    let mut keys: Vec<String> =
        std::iter::once(disk.settings.api_key.clone()).chain(disk.secrets.iter().map(|secret| secret.value.clone())).filter(|key| !key.is_empty()).collect();
    keys.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    keys.dedup();
    keys
}

// ---------------------------------------------------------------------------------------------
// state.json loading

pub(crate) fn schema_valid(disk: &DiskState) -> bool {
    if disk.secrets.len() > MAX_SECRETS {
        return false;
    }
    let mut scopes = HashSet::new();
    let secrets = disk.secrets.iter().all(|secret| {
        is_uuid(&secret.id)
            && secret.bot_id.as_deref().is_none_or(is_uuid)
            && secret_name_valid(&secret.name)
            && (MIN_SECRET_CHARS..=MAX_SECRET_CHARS).contains(&js_len(&secret.value))
            && !control(&secret.value)
            && is_timestamp(&secret.created_at)
            && is_timestamp(&secret.updated_at)
            && scopes.insert((secret.bot_id.clone(), secret.name.clone()))
    });
    let mut names = HashSet::new();
    let servers = disk.mcp_servers.iter().all(|server| {
        is_uuid(&server.id)
            && is_uuid(&server.bot_id)
            && server_name_valid(&server.name)
            && names.insert((server.bot_id.clone(), server.name.clone()))
            && is_timestamp(&server.created_at)
            && is_timestamp(&server.updated_at)
            && parse_mcp_json(&mcp_json([server])).is_ok_and(|parsed| parsed.len() == 1 && parsed[0].1 == server.config)
    });
    let per_bot = {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for server in &disk.mcp_servers {
            *counts.entry(server.bot_id.as_str()).or_default() += 1;
        }
        counts.values().all(|count| *count <= MAX_SERVERS_PER_BOT)
    };
    secrets && servers && per_bot
}

pub(crate) fn references_valid(disk: &DiskState) -> bool {
    let bots: HashSet<&str> = disk.bots.iter().map(|bot| bot.id.as_str()).collect();
    disk.secrets.iter().all(|secret| secret.bot_id.as_deref().is_none_or(|id| bots.contains(id)))
        && disk.mcp_servers.iter().all(|server| bots.contains(server.bot_id.as_str()))
}

// ---------------------------------------------------------------------------------------------
// State methods

fn secret_name(name: &str) -> HttpResult<String> {
    let name = js_trim(name);
    if name.is_empty() {
        return Err(bad("환경 변수 이름을 입력해 주세요"));
    }
    if !secret_name_valid(name) {
        return Err(bad("문자로 시작하는 대문자 환경 변수 이름(A-Z, 0-9, _)을 사용해 주세요"));
    }
    Ok(name.to_owned())
}

fn secret_value(value: &str) -> HttpResult<String> {
    let value = js_trim(value);
    let length = js_len(value);
    if length < MIN_SECRET_CHARS {
        return Err(bad(format!("시크릿 값은 {MIN_SECRET_CHARS}자 이상이어야 해요. 짧은 값은 대화와 기록 곳곳에서 가려져 내용을 알아볼 수 없게 됩니다.")));
    }
    if length > MAX_SECRET_CHARS || control(value) {
        return Err(bad(format!("시크릿 값은 줄바꿈이나 제어 문자 없이 {MAX_SECRET_CHARS}자 이하여야 해요.")));
    }
    Ok(value.to_owned())
}

impl State {
    /// Every string the public snapshot and model-facing text must redact.
    pub fn redaction_keys(&self) -> Vec<String> {
        let mut keys = redaction_keys(&self.disk);
        keys.extend(self.external_redaction_keys.iter().cloned());
        keys.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        keys.dedup();
        keys
    }

    pub fn secrets(&self) -> &[VaultSecret] {
        &self.disk.secrets
    }

    pub fn mcp_servers_for(&self, bot_id: &str) -> impl Iterator<Item = &McpServerRecord> {
        self.disk.mcp_servers.iter().filter(move |server| server.bot_id == bot_id)
    }

    /// "새 시크릿 추가". Loopback only.
    pub fn add_secret(&mut self, input: SecretInput, local: bool) -> HttpResult<PublicSecret> {
        let name = secret_name(&input.name)?;
        if let Some(bot_id) = input.bot_id.as_deref()
            && !is_uuid(bot_id)
        {
            return Err(bad("botId: Invalid uuid"));
        }
        let value = secret_value(&input.value)?;
        self.assert_writable()?;
        require_local(local, VAULT_REQUIRES_LOOPBACK)?;
        if let Some(bot_id) = input.bot_id.as_deref() {
            self.bot(bot_id)?;
        }
        if self.disk.secrets.len() >= MAX_SECRETS {
            return Err(HttpError::conflict(format!("보관함이 가득 찼어요. 시크릿은 {MAX_SECRETS}개까지 저장할 수 있어요.")));
        }
        if self.disk.secrets.iter().any(|secret| secret.name == name && secret.bot_id == input.bot_id) {
            return Err(HttpError::conflict(format!("{name} 시크릿이 이미 있어요. 값을 바꾸려면 교체하세요.")));
        }
        let now = iso(self.now());
        let secret = VaultSecret { id: new_id(), name, bot_id: input.bot_id, value, created_at: now.clone(), updated_at: now };
        let public = PublicSecret::from(&secret);
        self.disk_mut().secrets.push(secret);
        Ok(public)
    }

    /// Replace a secret's value. Loopback only.
    pub fn replace_secret(&mut self, id: &str, value: &str, local: bool) -> HttpResult<()> {
        let value = secret_value(value)?;
        self.assert_writable()?;
        require_local(local, VAULT_REQUIRES_LOOPBACK)?;
        let index = self.secret_index(id)?;
        let now = iso(self.now());
        let secret = &mut self.disk_mut().secrets[index];
        secret.value = value;
        secret.updated_at = now;
        Ok(())
    }

    /// Delete a secret. Loopback only.
    pub fn delete_secret(&mut self, id: &str, local: bool) -> HttpResult<()> {
        self.assert_writable()?;
        require_local(local, VAULT_REQUIRES_LOOPBACK)?;
        let index = self.secret_index(id)?;
        self.disk_mut().secrets.remove(index);
        Ok(())
    }

    fn secret_index(&self, id: &str) -> HttpResult<usize> {
        self.disk.secrets.iter().position(|secret| secret.id == id).ok_or_else(|| HttpError::not_found("시크릿을 찾을 수 없어요."))
    }

    /// The bot's servers as an "MCP 서버 JSON" document (literal values included). The HTTP route
    /// serves it to loopback clients only.
    pub fn mcp_json(&self, bot_id: &str) -> HttpResult<String> {
        self.bot(bot_id)?;
        Ok(mcp_json(self.mcp_servers_for(bot_id)))
    }

    /// Replace the bot's server set with an "MCP 서버 JSON" document. Loopback only, idle only.
    /// A server whose name and configuration are unchanged keeps its ID and enabled state; a
    /// changed or new server is disabled until it is enabled again.
    pub fn set_mcp_servers(&mut self, bot_id: &str, json: &str, local: bool) -> HttpResult<()> {
        let parsed = parse_mcp_json(json)?;
        self.assert_idle()?;
        require_local(local, MCP_REQUIRES_LOOPBACK)?;
        self.bot(bot_id)?;
        let now = iso(self.now());
        let current: Vec<McpServerRecord> = self.mcp_servers_for(bot_id).cloned().collect();
        let next: Vec<McpServerRecord> = parsed
            .into_iter()
            .map(|(name, config)| match current.iter().find(|server| server.name == name) {
                Some(existing) if existing.config == config => existing.clone(),
                Some(existing) => McpServerRecord { enabled: false, config, updated_at: now.clone(), ..existing.clone() },
                None => {
                    McpServerRecord { id: new_id(), bot_id: bot_id.to_owned(), name, enabled: false, config, created_at: now.clone(), updated_at: now.clone() }
                }
            })
            .collect();
        if next == current {
            return Ok(());
        }
        let disk = self.disk_mut();
        disk.mcp_servers.retain(|server| server.bot_id != bot_id);
        disk.mcp_servers.extend(next);
        Ok(())
    }

    /// Save a form's new credentials and configuration in one store mutation/state-file write.
    /// Existing `{json}` callers keep the exact JSON editor contract (including literal argv).
    pub fn set_mcp_servers_with_secrets(&mut self, bot_id: &str, json: &str, secrets: &[SecretInput], local: bool) -> HttpResult<()> {
        if secrets.is_empty() {
            return self.set_mcp_servers(bot_id, json, local);
        }
        require_local(local, MCP_REQUIRES_LOOPBACK)?;
        self.assert_idle()?;
        self.bot(bot_id)?;
        let mut field = McpField::Json;
        let parsed = parse_mcp_json_field(json, &mut field).map_err(|error| HttpError::new(error.status, field.message()))?;
        if secrets.len() > MAX_SECRETS {
            return Err(bad("새 시크릿(secrets): 항목이 너무 많아요."));
        }
        let referenced: HashSet<String> = parsed.iter().flat_map(|(_, config)| config_references(config)).collect();
        // Checkpoint only the two collections this module mutates, not unrelated app state.
        let previous = (self.disk.secrets.clone(), self.disk.mcp_servers.clone(), self.dirty);
        let result = (|| {
            for input in secrets {
                if input.bot_id.as_deref().is_some_and(|scope| scope != bot_id) || !referenced.contains(js_trim(&input.name)) {
                    return Err(bad("새 시크릿(secrets): 현재 Bot의 구성에서 참조하는 값만 저장할 수 있어요."));
                }
                secret_name(&input.name).map_err(|error| {
                    HttpError::new(error.status, "새 시크릿 이름(secrets.name): A-Z로 시작하는 A-Z, 0-9, _ 이름을 64자 이내로 입력해 주세요.")
                })?;
                secret_value(&input.value)
                    .map_err(|error| HttpError::new(error.status, "새 시크릿 값(secrets.value): 제어 문자 없이 8~8,192자로 입력해 주세요."))?;
                self.add_secret(SecretInput { bot_id: Some(bot_id.into()), ..input.clone() }, local)
                    .map_err(|error| HttpError::new(error.status, "새 시크릿 이름(secrets.name): 같은 이름의 Bot 시크릿이 있거나 보관함이 가득 찼어요."))?;
            }
            self.set_mcp_servers(bot_id, json, local)
        })();
        if result.is_err() {
            (self.disk.secrets, self.disk.mcp_servers, self.dirty) = previous;
        }
        result
    }

    /// Turn one server on or off. Enabling needs a loopback client; disabling is allowed from any
    /// client (it only removes capability).
    pub fn set_mcp_server_enabled(&mut self, bot_id: &str, name: &str, enabled: bool, local: bool) -> HttpResult<()> {
        self.assert_writable()?;
        if enabled {
            require_local(local, MCP_REQUIRES_LOOPBACK)?;
        }
        self.bot(bot_id)?;
        let index = self
            .disk
            .mcp_servers
            .iter()
            .position(|server| server.bot_id == bot_id && server.name == name)
            .ok_or_else(|| HttpError::not_found("MCP 서버를 찾을 수 없어요."))?;
        if self.disk.mcp_servers[index].enabled == enabled {
            return Ok(());
        }
        let now = iso(self.now());
        let server = &mut self.disk_mut().mcp_servers[index];
        server.enabled = enabled;
        server.updated_at = now;
        Ok(())
    }

    /// Fingerprint of the bot's enabled server configurations (references unresolved, so secret
    /// values never enter it); `None` without enabled servers so existing sessions stay valid.
    pub fn mcp_fingerprint(&self, bot_id: &str) -> Option<String> {
        let enabled: Vec<(&str, &McpServerConfig)> =
            self.mcp_servers_for(bot_id).filter(|server| server.enabled).map(|server| (server.name.as_str(), &server.config)).collect();
        if enabled.is_empty() {
            return None;
        }
        let input = serde_json::to_string(&enabled).unwrap_or_default();
        Some(hex::encode(Sha256::digest(input.as_bytes())))
    }

    /// Prepare exactly one unsaved server for a loopback-only connection test. The supplied
    /// secrets are ephemeral, target-bot scoped even when `botId` is omitted, and cannot replace
    /// an existing target-scoped secret. Nothing is written, enabled, or broadcast.
    pub fn resolve_mcp_draft(&self, bot_id: &str, json: &str, secrets: &[SecretInput], local: bool) -> HttpResult<ResolvedMcpServer> {
        require_local(local, MCP_REQUIRES_LOOPBACK)?;
        self.assert_idle()?;
        self.bot(bot_id)?;
        // Parser diagnostics may contain literal credentials from the draft (e.g. unknown keys).
        let mut field = McpField::Json;
        let mut parsed = parse_mcp_json_field(json, &mut field).map_err(|error| HttpError::new(error.status, field.message()))?;
        if parsed.len() != 1 {
            return Err(bad("MCP 구성 오류: 연결 테스트에는 서버를 정확히 하나만 넣어 주세요."));
        }
        if secrets.len() > MAX_SECRETS {
            return Err(bad("MCP 시크릿 오류: 테스트 시크릿이 너무 많아요."));
        }
        let mut ephemeral = BTreeMap::new();
        for input in secrets {
            if input.bot_id.as_deref().is_some_and(|scope| scope != bot_id) {
                return Err(bad("MCP 시크릿 오류: 현재 Bot의 시크릿만 테스트할 수 있어요."));
            }
            let name = secret_name(&input.name)
                .map_err(|error| HttpError::new(error.status, "새 시크릿 이름(secrets.name): A-Z로 시작하는 A-Z, 0-9, _ 이름을 64자 이내로 입력해 주세요."))?;
            let value = secret_value(&input.value)
                .map_err(|error| HttpError::new(error.status, "새 시크릿 값(secrets.value): 제어 문자 없이 8~8,192자로 입력해 주세요."))?;
            if self.disk.secrets.iter().any(|secret| secret.name == name && secret.bot_id.as_deref() == Some(bot_id)) {
                return Err(HttpError::conflict("MCP 시크릿 오류: 현재 Bot에 같은 이름의 시크릿이 이미 있어요."));
            }
            if ephemeral.insert(name, value).is_some() {
                return Err(bad("MCP 시크릿 오류: 테스트 시크릿 이름이 중복되었어요."));
            }
        }
        let lookup = |name: &str| {
            self.disk
                .secrets
                .iter()
                .find(|secret| secret.name == name && secret.bot_id.as_deref() == Some(bot_id))
                .map(|secret| secret.value.as_str())
                .or_else(|| ephemeral.get(name).map(String::as_str))
                .or_else(|| self.disk.secrets.iter().find(|secret| secret.name == name && secret.bot_id.is_none()).map(|secret| secret.value.as_str()))
        };
        let (name, config) = parsed.pop().expect("exactly one validated server");
        if let McpServerConfig::Stdio { args, .. } = &config
            && args.iter().any(|arg| js_trim(arg).is_empty())
        {
            return Err(bad(McpField::Args.message()));
        }
        let field = match config {
            McpServerConfig::Stdio { .. } => "환경 변수(env)",
            McpServerConfig::Http { .. } => "헤더(headers)",
        };
        let config =
            resolve_mcp_config(&config, lookup).map_err(|_| bad(format!("{field}: 참조한 시크릿이 없어요. 보관함 또는 테스트 시크릿을 확인해 주세요.")))?;
        Ok(ResolvedMcpServer { name, config })
    }

    /// The bot's enabled servers for a session start, with `${secret:NAME}` resolved (the bot's own
    /// secret first, then an all-bots secret). A server with a missing secret is skipped and
    /// reported instead.
    pub fn resolve_mcp_servers(&self, bot_id: &str) -> (Vec<ResolvedMcpServer>, Vec<McpStartupFailure>) {
        let lookup = |name: &str| {
            let scoped = self.disk.secrets.iter().find(|secret| secret.name == name && secret.bot_id.as_deref() == Some(bot_id));
            scoped.or_else(|| self.disk.secrets.iter().find(|secret| secret.name == name && secret.bot_id.is_none())).map(|secret| secret.value.as_str())
        };
        let mut servers = Vec::new();
        let mut failures = Vec::new();
        for server in self.mcp_servers_for(bot_id).filter(|server| server.enabled) {
            let config = resolve_mcp_config(&server.config, lookup);
            match config {
                Ok(config) => servers.push(ResolvedMcpServer { name: server.name.clone(), config }),
                Err(message) => failures.push(McpStartupFailure { name: server.name.clone(), message }),
            }
        }
        (servers, failures)
    }
}

/// Bot deletion: drop the bot's secrets and servers from the candidate state.
pub(crate) fn remove_bot(disk: &mut DiskState, bot_id: &str) {
    disk.secrets.retain(|secret| secret.bot_id.as_deref() != Some(bot_id));
    disk.mcp_servers.retain(|server| server.bot_id != bot_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_reject_engine_prefix_collisions() {
        assert!(server_name_valid("github"));
        assert!(server_name_valid("my-server_2"));
        for name in ["workspace", "workspace-files", "a__b", "_x", "-x", "Upper", "", "x".repeat(33).as_str(), "한글"] {
            assert!(!server_name_valid(name), "{name}");
        }
        assert!(secret_name_valid("API_TOKEN"));
        assert!(!secret_name_valid("api_token") && !secret_name_valid("1TOKEN") && !secret_name_valid("A-B"));
    }

    #[test]
    fn mcp_json_is_strict_and_canonical() {
        let text = r#"{"mcpServers": {
            "web": {"url": "https://example.com/mcp", "headers": {"Authorization": "Bearer ${secret:API_TOKEN}"}},
            "local": {"command": " node ", "args": ["server.js"], "env": {"TOKEN": "${secret:API_TOKEN}", "MODE": "x"}}
        }}"#;
        let parsed = parse_mcp_json(text).unwrap();
        assert_eq!(parsed.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(), ["local", "web"]);
        assert_eq!(
            parsed[0].1,
            McpServerConfig::Stdio {
                command: "node".into(),
                args: vec!["server.js".into()],
                env: vec![McpPair { name: "MODE".into(), value: "x".into() }, McpPair { name: "TOKEN".into(), value: "${secret:API_TOKEN}".into() }],
            }
        );
        // The bare server object is accepted too.
        assert_eq!(parse_mcp_json(r#"{"one": {"url": "http://127.0.0.1:1/mcp"}}"#).unwrap().len(), 1);
        for (bad_json, needle) in [
            ("[]", "객체"),
            ("{", "JSON"),
            (r#"{"a": {"url": "http://x/a"}, "a": {"url": "http://x/b"}}"#, "중복"),
            (r#"{"workspace": {"url": "http://x"}}"#, "workspace"),
            (r#"{"a__b": {"url": "http://x"}}"#, "__"),
            (r#"{"a": {}}"#, "URL 또는 명령"),
            (r#"{"a": {"url": "http://x", "command": "y"}}"#, "하나만"),
            (r#"{"a": {"url": "ftp://x"}}"#, "url"),
            (r#"{"a": {"url": "https://user:pw@x"}}"#, "url"),
            (r#"{"a": {"command": "x", "cwd": "/"}}"#, "알 수 없는"),
            (r#"{"a": {"command": "x", "env": {"A": 1}}}"#, "문자열"),
            (r#"{"a": {"command": "x", "env": {"A-B": "1"}}}"#, "환경 변수"),
            (r#"{"a": {"url": "http://x", "headers": {"X": "a\nb"}}}"#, "제어"),
            (r#"{"a": {"url": "http://x", "headers": {"X": "${secret:lower}"}}}"#, "대문자"),
            (r#"{"a": {"command": "${secret:TOKEN}"}}"#, "env 또는 headers"),
            (r#"{"a": {"command": "./server.sh"}}"#, "절대 경로"),
            (r#"{"a": {"command": "x", "type": "sse"}}"#, "지원"),
            (r#"{"mcpServers": {}, "other": 1}"#, "다른 항목"),
        ] {
            let error = parse_mcp_json(bad_json).unwrap_err();
            assert_eq!(error.status, 400, "{bad_json}");
            assert!(error.message.contains(needle), "{bad_json}: {}", error.message);
        }
        let many: String = format!("{{{}}}", (0..11).map(|i| format!("\"s{i}\": {{\"url\": \"http://x/{i}\"}}")).collect::<Vec<_>>().join(","));
        assert!(parse_mcp_json(&many).unwrap_err().message.contains("10개"));
        assert_eq!(parse_mcp_json(&" ".repeat(MAX_MCP_JSON_BYTES + 1)).unwrap_err().status, 413);
    }
}
