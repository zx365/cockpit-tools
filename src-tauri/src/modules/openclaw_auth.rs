use crate::models::codex::CodexAccount;
use crate::modules::{atomic_write, codex_account, codex_oauth, logger};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
#[cfg(target_os = "macos")]
use sha2::{Digest, Sha256};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use std::time::SystemTime;

const OPENCLAW_AUTH_PROFILES_FILENAME: &str = "auth-profiles.json";
const OPENCLAW_DEFAULT_AGENT_ID: &str = "main";
const OPENCLAW_CODEX_PROVIDER: &str = "openai";
const OPENCLAW_CODEX_PROFILE_ID: &str = "openai:default";
const OPENCLAW_LEGACY_CODEX_PROVIDER: &str = "openai-codex";
const OPENCLAW_LEGACY_CODEX_PROFILE_PREFIX: &str = "openai-codex:";
const OPENCLAW_AUTH_STORE_VERSION: i32 = 1;
pub const OPENCLAW_DEFAULT_MODEL: &str = "openai/gpt-5.6-luna";
pub const OPENCLAW_DEFAULT_THINKING: &str = "low";
#[cfg(target_os = "macos")]
const CODEX_KEYCHAIN_SERVICE: &str = "Codex Auth";

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|item| item == &path) {
        paths.push(path);
    }
}

fn resolve_user_path(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }

    if let Some(stripped) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }

    PathBuf::from(trimmed)
}

fn get_openclaw_state_dir_candidates() -> Result<Vec<PathBuf>, String> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    for env_key in ["OPENCLAW_STATE_DIR", "CLAWDBOT_STATE_DIR"] {
        if let Ok(value) = std::env::var(env_key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                push_unique_path(&mut candidates, resolve_user_path(trimmed));
            }
        }
    }

    if let Some(home) = dirs::home_dir() {
        push_unique_path(&mut candidates, home.join(".openclaw"));
        push_unique_path(&mut candidates, home.join(".clawdbot"));
        push_unique_path(&mut candidates, home.join(".moldbot"));
        push_unique_path(&mut candidates, home.join(".moltbot"));
    }

    if candidates.is_empty() {
        return Err("无法推断 OpenClaw state 目录".to_string());
    }

    Ok(candidates)
}

/// 从 Gateway 启动命令中提取 OpenClaw 的 Node 运行时和入口文件。
fn parse_openclaw_gateway_runtime(content: &str) -> Option<(PathBuf, PathBuf)> {
    for line in content.lines() {
        let args = crate::modules::process::parse_extra_args(line);
        let node = args.iter().find(|arg| {
            let normalized = arg.replace('\\', "/").to_ascii_lowercase();
            normalized.ends_with("/node.exe") || normalized.ends_with("/node")
        });
        let script = args.iter().find(|arg| {
            arg.replace('\\', "/")
                .to_ascii_lowercase()
                .ends_with("/node_modules/openclaw/dist/index.js")
        });
        if let (Some(node), Some(script)) = (node, script) {
            return Some((PathBuf::from(node), PathBuf::from(script)));
        }
    }
    None
}

/// 从已安装的 Gateway 启动脚本定位桌面应用可直接调用的 OpenClaw CLI。
fn find_openclaw_gateway_runtime() -> Option<(String, String)> {
    get_openclaw_state_dir_candidates()
        .ok()?
        .into_iter()
        .find_map(|state_dir| {
            let content = fs::read_to_string(state_dir.join("gateway.cmd")).ok()?;
            let (node, script) = parse_openclaw_gateway_runtime(&content)?;
            (node.exists() && script.exists()).then(|| {
                (
                    node.to_string_lossy().to_string(),
                    script.to_string_lossy().to_string(),
                )
            })
        })
}

/// 执行 OpenClaw CLI，并兼容桌面应用 PATH 中没有 openclaw 的情况。
fn run_openclaw_cli(args: &[&str]) -> Result<(), String> {
    let mut commands: Vec<(String, Vec<String>)> = Vec::new();
    if let Ok(path) = std::env::var("OPENCLAW_CLI_PATH") {
        if !path.trim().is_empty() {
            commands.push((path.trim().to_string(), Vec::new()));
        }
    }
    if let Some((node, script)) = find_openclaw_gateway_runtime() {
        commands.push((node, vec![script]));
    }
    commands.push(("openclaw".to_string(), Vec::new()));

    for (program, prefix_args) in commands {
        let mut command = Command::new(&program);
        command.args(prefix_args).args(args);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        match command.output() {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => {
                let detail = String::from_utf8_lossy(if output.stderr.is_empty() {
                    &output.stdout
                } else {
                    &output.stderr
                })
                .trim()
                .chars()
                .take(500)
                .collect::<String>();
                return Err(format!(
                    "OpenClaw 命令失败（{}）: {}",
                    output.status,
                    if detail.is_empty() {
                        "无错误详情"
                    } else {
                        &detail
                    }
                ));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("调用 OpenClaw 失败: {error}")),
        }
    }
    Err("未找到 OpenClaw 命令，请先安装 OpenClaw 或设置 OPENCLAW_CLI_PATH".to_string())
}

/// 读取 OpenClaw 当前默认模型与思考强度。
pub fn get_openclaw_model_settings() -> Result<(String, String), String> {
    let config_path = get_openclaw_state_dir_candidates()?
        .into_iter()
        .map(|state_dir| state_dir.join("openclaw.json"))
        .find(|path| path.exists());
    let Some(config_path) = config_path else {
        return Ok((
            OPENCLAW_DEFAULT_MODEL.to_string(),
            OPENCLAW_DEFAULT_THINKING.to_string(),
        ));
    };
    let content = fs::read_to_string(&config_path)
        .map_err(|error| format!("读取 OpenClaw 配置失败: {error}"))?;
    let config: Value = serde_json::from_str(&content)
        .map_err(|error| format!("解析 OpenClaw 配置失败: {error}"))?;
    let defaults = config.pointer("/agents/defaults");
    let model = defaults
        .and_then(|value| {
            value
                .pointer("/model/primary")
                .or_else(|| value.get("model"))
        })
        .and_then(Value::as_str)
        .unwrap_or(OPENCLAW_DEFAULT_MODEL)
        .to_string();
    let thinking = defaults
        .and_then(|value| value.get("thinkingDefault"))
        .and_then(Value::as_str)
        .unwrap_or(OPENCLAW_DEFAULT_THINKING)
        .to_string();
    Ok((model, thinking))
}

/// 校验 OpenClaw 默认模型与思考强度。
pub fn validate_openclaw_model_settings(model: &str, thinking: &str) -> Result<(), String> {
    if model.len() > 128
        || !model.contains('/')
        || !model
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-/".contains(character))
    {
        return Err("不支持的 OpenClaw 模型".to_string());
    }
    if !matches!(
        thinking,
        "off" | "minimal" | "low" | "medium" | "high" | "xhigh"
    ) {
        return Err("不支持的 OpenClaw 思考强度".to_string());
    }
    Ok(())
}

/// 保存 OpenClaw 默认模型与思考强度。
pub fn save_openclaw_model_settings(model: &str, thinking: &str) -> Result<(), String> {
    validate_openclaw_model_settings(model, thinking)?;
    run_openclaw_cli(&["config", "set", "agents.defaults.model.primary", model])?;
    run_openclaw_cli(&["config", "set", "agents.defaults.thinkingDefault", thinking])
}

/// 读取 Cockpit 当前进程实际使用的 HTTP 代理。
fn current_http_proxy_url() -> Option<String> {
    [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .into_iter()
    .filter_map(|key| std::env::var(key).ok())
    .map(|value| value.trim().to_string())
    .find(|value| {
        !value.contains(['\r', '\n'])
            && (value.starts_with("http://") || value.starts_with("https://"))
    })
}

/// 合并 OpenClaw 守护进程环境文件中的代理配置。
fn merge_openclaw_gateway_env(existing: &str, proxy_url: &str, no_proxy: &str) -> String {
    let mut lines = existing
        .lines()
        .filter(|line| {
            let key = line
                .trim_start()
                .split_once('=')
                .map(|(key, _)| key.trim())
                .unwrap_or_default();
            !matches!(
                key.to_ascii_uppercase().as_str(),
                "HTTP_PROXY" | "HTTPS_PROXY" | "NO_PROXY"
            )
        })
        .map(str::to_string)
        .collect::<Vec<_>>();
    lines.push(format!("HTTP_PROXY={proxy_url}"));
    lines.push(format!("HTTPS_PROXY={proxy_url}"));
    lines.push(format!("NO_PROXY={no_proxy}"));
    format!("{}\n", lines.join("\n"))
}

/// 将 Cockpit 当前代理同步给 OpenClaw Gateway，并启用环境代理传输。
pub fn sync_openclaw_gateway_proxy_env() -> Result<bool, String> {
    let Some(proxy_url) = current_http_proxy_url() else {
        logger::log_info("未检测到可供 OpenClaw 使用的 HTTP(S) 代理，保留其当前网络配置");
        return Ok(false);
    };
    let state_dir = get_openclaw_state_dir_candidates()?
        .into_iter()
        .find(|path| path.exists())
        .or_else(|| dirs::home_dir().map(|home| home.join(".openclaw")))
        .ok_or_else(|| "无法推断 OpenClaw state 目录".to_string())?;
    let env_path = state_dir.join(".env");
    let existing = match fs::read_to_string(&env_path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("读取 OpenClaw Gateway 环境失败: {error}")),
    };
    let no_proxy_seed = [
        std::env::var("NO_PROXY").unwrap_or_default(),
        std::env::var("no_proxy").unwrap_or_default(),
    ]
    .into_iter()
    .filter(|value| !value.trim().is_empty())
    .collect::<Vec<_>>()
    .join(",");
    let no_proxy = crate::modules::codex_protocol::merge_local_no_proxy(&no_proxy_seed);
    atomic_write::write_string_atomic(
        &env_path,
        &merge_openclaw_gateway_env(&existing, &proxy_url, &no_proxy),
    )?;
    run_openclaw_cli(&[
        "config",
        "set",
        "models.providers.openai.request.proxy",
        r#"{"mode":"env-proxy"}"#,
        "--strict-json",
    ])?;
    logger::log_info("已将 Cockpit 当前代理同步给 OpenClaw Gateway");
    Ok(true)
}

#[derive(Debug)]
struct OpenClawWechatProfile {
    account_id: String,
    user_id: String,
    modified_at: SystemTime,
}

/// 查找最近绑定且已建立主动推送会话的 OpenClaw 微信账号。
fn find_openclaw_wechat_profile() -> Result<OpenClawWechatProfile, String> {
    let mut profiles = Vec::new();
    for state_dir in get_openclaw_state_dir_candidates()? {
        let accounts_dir = state_dir.join("openclaw-weixin").join("accounts");
        let Ok(entries) = fs::read_dir(&accounts_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if path.extension().and_then(|value| value.to_str()) != Some("json")
                || file_name.ends_with(".context-tokens.json")
            {
                continue;
            }
            let Some(account) = fs::read_to_string(&path)
                .ok()
                .and_then(|content| serde_json::from_str::<Value>(&content).ok())
            else {
                continue;
            };
            let Some(user_id) = account
                .get("userId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            if account
                .get("token")
                .and_then(Value::as_str)
                .map(str::trim)
                .is_none_or(str::is_empty)
            {
                continue;
            }
            let Some(account_id) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            let context_path = accounts_dir.join(format!("{account_id}.context-tokens.json"));
            let has_context = fs::read_to_string(context_path)
                .ok()
                .and_then(|content| serde_json::from_str::<Value>(&content).ok())
                .and_then(|value| value.as_object().cloned())
                .is_some_and(|tokens| {
                    tokens.iter().any(|(key, value)| {
                        key.eq_ignore_ascii_case(user_id)
                            && value.as_str().is_some_and(|token| !token.trim().is_empty())
                    })
                });
            if !has_context {
                continue;
            }
            profiles.push(OpenClawWechatProfile {
                account_id: account_id.to_string(),
                user_id: user_id.to_string(),
                modified_at: entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH),
            });
        }
    }

    // ponytail: 仅使用最近绑定的一个微信账号；需要多账号通知时再增加账号选择器。
    profiles
        .into_iter()
        .max_by_key(|profile| profile.modified_at)
        .ok_or_else(|| {
            "未找到已建立会话的 OpenClaw 微信账号，请先绑定并向机器人发送任意消息".to_string()
        })
}

/// 通过 OpenClaw 微信通道发送一条消息。
pub fn send_openclaw_wechat_message(message: &str) -> Result<(), String> {
    if message.trim().is_empty() {
        return Err("微信通知内容不能为空".to_string());
    }
    let profile = find_openclaw_wechat_profile()?;
    run_openclaw_cli(&[
        "message",
        "send",
        "--channel",
        "openclaw-weixin",
        "--account",
        &profile.account_id,
        "--target",
        &profile.user_id,
        "--message",
        message,
        "--json",
    ])?;
    logger::log_info("OpenClaw 微信通知发送成功");
    Ok(())
}

/// 在后台发送 OpenClaw 微信通知，不阻塞额度刷新。
pub fn notify_openclaw_wechat(message: String) {
    std::thread::spawn(move || {
        if let Err(error) = send_openclaw_wechat_message(&message) {
            logger::log_warn(&format!("OpenClaw 微信通知失败: {error}"));
        }
    });
}

fn get_openclaw_auth_profiles_path_candidates() -> Result<Vec<PathBuf>, String> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    for env_key in ["OPENCLAW_AGENT_DIR", "PI_CODING_AGENT_DIR"] {
        if let Ok(value) = std::env::var(env_key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                push_unique_path(
                    &mut candidates,
                    resolve_user_path(trimmed).join(OPENCLAW_AUTH_PROFILES_FILENAME),
                );
            }
        }
    }

    let state_dir_candidates = get_openclaw_state_dir_candidates()?;
    let explicit_state_dir = ["OPENCLAW_STATE_DIR", "CLAWDBOT_STATE_DIR"]
        .iter()
        .find_map(|env_key| std::env::var(env_key).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| resolve_user_path(&value));

    let preferred_state_dir = explicit_state_dir.unwrap_or_else(|| {
        state_dir_candidates
            .iter()
            .find(|path| path.exists())
            .cloned()
            .unwrap_or_else(|| state_dir_candidates[0].clone())
    });

    push_unique_path(
        &mut candidates,
        preferred_state_dir
            .join("agents")
            .join(OPENCLAW_DEFAULT_AGENT_ID)
            .join("agent")
            .join(OPENCLAW_AUTH_PROFILES_FILENAME),
    );

    for state_dir in state_dir_candidates {
        push_unique_path(
            &mut candidates,
            state_dir
                .join("agents")
                .join(OPENCLAW_DEFAULT_AGENT_ID)
                .join("agent")
                .join(OPENCLAW_AUTH_PROFILES_FILENAME),
        );
    }

    if candidates.is_empty() {
        return Err("无法推断 OpenClaw auth-profiles.json 路径".to_string());
    }

    Ok(candidates)
}

fn get_openclaw_auth_profiles_path() -> Result<PathBuf, String> {
    let candidates = get_openclaw_auth_profiles_path_candidates()?;
    Ok(candidates
        .first()
        .cloned()
        .ok_or_else(|| "无法推断 OpenClaw auth-profiles.json 路径".to_string())?)
}

fn atomic_write(path: &Path, content: &str) -> Result<(), String> {
    crate::modules::atomic_write::write_string_atomic(path, content)
        .map_err(|e| format!("写入 auth-profiles.json 失败: {}", e))
}

fn decode_token_exp_ms(access_token: &str) -> Option<i64> {
    let payload = codex_account::decode_jwt_payload(access_token).ok()?;
    payload.exp.map(|exp| exp * 1000)
}

fn build_openclaw_codex_payload(account: &CodexAccount) -> Result<Value, String> {
    let refresh = account
        .tokens
        .refresh_token
        .clone()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "Codex refresh_token 缺失，无法同步到 OpenClaw".to_string())?;
    let expires = decode_token_exp_ms(&account.tokens.access_token)
        .ok_or_else(|| "Codex access_token 缺少 exp，无法同步到 OpenClaw".to_string())?;

    let mut payload = json!({
        "type": "oauth",
        "provider": OPENCLAW_CODEX_PROVIDER,
        "access": account.tokens.access_token,
        "refresh": refresh,
        "expires": expires,
    });

    if !account.tokens.id_token.trim().is_empty() {
        payload["idToken"] = json!(account.tokens.id_token.clone());
    }

    if let Some(account_id) = account
        .account_id
        .clone()
        .filter(|value| !value.trim().is_empty())
    {
        payload["accountId"] = json!(account_id);
    }
    if !account.email.trim().is_empty() {
        payload["email"] = json!(account.email.clone());
    }

    Ok(payload)
}

fn upsert_openclaw_codex_profile(auth_json: &mut Value, payload: Value) {
    if !auth_json.is_object() {
        *auth_json = json!({});
    }

    let Some(root) = auth_json.as_object_mut() else {
        return;
    };

    let profiles_value = root.entry("profiles").or_insert_with(|| json!({}));
    if !profiles_value.is_object() {
        *profiles_value = json!({});
    }

    if let Some(profiles) = profiles_value.as_object_mut() {
        let stale_profile_ids: Vec<String> = profiles
            .keys()
            .filter(|profile_id| profile_id.starts_with(OPENCLAW_LEGACY_CODEX_PROFILE_PREFIX))
            .cloned()
            .collect();
        for profile_id in stale_profile_ids {
            profiles.remove(&profile_id);
        }
        profiles.insert(OPENCLAW_CODEX_PROFILE_ID.to_string(), payload);
    }
}

/// 更新 OpenClaw 的 OpenAI 凭据顺序并清理旧命名空间失败态。
fn upsert_openclaw_codex_state(auth_json: &mut Value) {
    if !auth_json.is_object() {
        *auth_json = json!({});
    }

    let Some(root) = auth_json.as_object_mut() else {
        return;
    };

    let order_value = root.entry("order").or_insert_with(|| json!({}));
    if !order_value.is_object() {
        *order_value = json!({});
    }

    if let Some(order_map) = order_value.as_object_mut() {
        order_map.remove(OPENCLAW_LEGACY_CODEX_PROVIDER);
        let mut profile_ids = vec![Value::String(OPENCLAW_CODEX_PROFILE_ID.to_string())];
        if let Some(existing) = order_map
            .get(OPENCLAW_CODEX_PROVIDER)
            .and_then(Value::as_array)
        {
            profile_ids.extend(
                existing
                    .iter()
                    .filter(|profile_id| {
                        profile_id.as_str().is_some_and(|profile_id| {
                            profile_id != OPENCLAW_CODEX_PROFILE_ID
                                && !profile_id.starts_with(OPENCLAW_LEGACY_CODEX_PROFILE_PREFIX)
                        })
                    })
                    .cloned(),
            );
        }
        order_map.insert(
            OPENCLAW_CODEX_PROVIDER.to_string(),
            Value::Array(profile_ids),
        );
    }

    if let Some(last_good_value) = root.get_mut("lastGood") {
        if !last_good_value.is_object() {
            *last_good_value = json!({});
        }
    } else {
        root.insert("lastGood".to_string(), json!({}));
    }
    if let Some(last_good_map) = root.get_mut("lastGood").and_then(Value::as_object_mut) {
        last_good_map.remove(OPENCLAW_LEGACY_CODEX_PROVIDER);
        last_good_map.insert(
            OPENCLAW_CODEX_PROVIDER.to_string(),
            Value::String(OPENCLAW_CODEX_PROFILE_ID.to_string()),
        );
    }

    if let Some(usage_stats_value) = root.get_mut("usageStats") {
        if !usage_stats_value.is_object() {
            *usage_stats_value = json!({});
        }
        if let Some(usage_stats_map) = usage_stats_value.as_object_mut() {
            let stale_profile_ids: Vec<String> = usage_stats_map
                .keys()
                .filter(|profile_id| {
                    profile_id.as_str() == OPENCLAW_CODEX_PROFILE_ID
                        || profile_id.starts_with(OPENCLAW_LEGACY_CODEX_PROFILE_PREFIX)
                })
                .cloned()
                .collect();
            for profile_id in stale_profile_ids {
                usage_stats_map.remove(&profile_id);
            }
        }
    }

    if root.get("version").is_none() {
        root.insert(
            "version".to_string(),
            Value::Number(serde_json::Number::from(OPENCLAW_AUTH_STORE_VERSION)),
        );
    }
}

/// 返回 OpenClaw 当前版本使用的账号凭据数据库候选路径。
fn get_openclaw_auth_database_path_candidates() -> Result<Vec<PathBuf>, String> {
    Ok(get_openclaw_auth_profiles_path_candidates()?
        .into_iter()
        .filter_map(|path| {
            path.parent()
                .map(|parent| parent.join("openclaw-agent.sqlite"))
        })
        .collect())
}

/// 将当前 Codex 凭据写入 OpenClaw SQLite 主存储。
fn sync_openclaw_auth_database(path: &Path, payload: &Value) -> Result<bool, String> {
    let mut connection = Connection::open(path)
        .map_err(|error| format!("打开 OpenClaw 凭据数据库失败 ({}): {error}", path.display()))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| format!("设置 OpenClaw 数据库超时失败: {error}"))?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| format!("锁定 OpenClaw 凭据数据库失败: {error}"))?;

    let store_content: Option<String> = transaction
        .query_row(
            "SELECT store_json FROM auth_profile_store WHERE store_key = 'primary'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("读取 OpenClaw 凭据失败: {error}"))?;
    let mut store = store_content
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| format!("解析 OpenClaw 凭据失败: {error}"))?
        .unwrap_or_else(|| json!({ "version": OPENCLAW_AUTH_STORE_VERSION, "profiles": {} }));
    upsert_openclaw_codex_profile(&mut store, payload.clone());

    let state_content: Option<String> = transaction
        .query_row(
            "SELECT state_json FROM auth_profile_state WHERE state_key = 'primary'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("读取 OpenClaw 凭据状态失败: {error}"))?;
    let mut state = state_content
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| format!("解析 OpenClaw 凭据状态失败: {error}"))?
        .unwrap_or_else(|| json!({}));
    let had_active_lockout = has_active_openclaw_codex_lockout(&state);
    upsert_openclaw_codex_state(&mut state);

    let now = chrono::Utc::now().timestamp_millis();
    transaction
        .execute(
            "INSERT INTO auth_profile_store (store_key, store_json, updated_at) VALUES ('primary', ?1, ?2) ON CONFLICT(store_key) DO UPDATE SET store_json = excluded.store_json, updated_at = excluded.updated_at",
            params![serde_json::to_string(&store).map_err(|error| format!("序列化 OpenClaw 凭据失败: {error}"))?, now],
        )
        .map_err(|error| format!("写入 OpenClaw 凭据失败: {error}"))?;
    transaction
        .execute(
            "INSERT INTO auth_profile_state (state_key, state_json, updated_at) VALUES ('primary', ?1, ?2) ON CONFLICT(state_key) DO UPDATE SET state_json = excluded.state_json, updated_at = excluded.updated_at",
            params![serde_json::to_string(&state).map_err(|error| format!("序列化 OpenClaw 凭据状态失败: {error}"))?, now],
        )
        .map_err(|error| format!("写入 OpenClaw 凭据状态失败: {error}"))?;
    transaction
        .commit()
        .map_err(|error| format!("提交 OpenClaw 凭据失败: {error}"))?;
    Ok(had_active_lockout)
}

#[derive(Debug, Clone)]
struct CodexCredentialSnapshot {
    account_id: Option<String>,
    email: Option<String>,
    expires_ms: Option<i64>,
}

fn normalize_non_empty(value: Option<&str>) -> Option<String> {
    let raw = value?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn normalize_email(value: Option<&str>) -> Option<String> {
    normalize_non_empty(value).map(|text| text.to_ascii_lowercase())
}

fn value_to_i64(value: Option<&Value>) -> Option<i64> {
    if let Some(as_i64) = value.and_then(Value::as_i64) {
        return Some(as_i64);
    }
    value
        .and_then(Value::as_u64)
        .and_then(|as_u64| i64::try_from(as_u64).ok())
}

fn has_active_openclaw_codex_lockout(auth_json: &Value) -> bool {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let Some(stats) = auth_json
        .get("usageStats")
        .and_then(Value::as_object)
        .and_then(|usage| usage.get(OPENCLAW_CODEX_PROFILE_ID))
        .and_then(Value::as_object)
    else {
        return false;
    };

    let cooldown_until = value_to_i64(stats.get("cooldownUntil"));
    let disabled_until = value_to_i64(stats.get("disabledUntil"));
    cooldown_until.is_some_and(|ts| ts > now_ms) || disabled_until.is_some_and(|ts| ts > now_ms)
}

fn expected_snapshot_from_account(account: &CodexAccount) -> CodexCredentialSnapshot {
    CodexCredentialSnapshot {
        account_id: normalize_non_empty(account.account_id.as_deref()).or_else(|| {
            codex_account::extract_chatgpt_account_id_from_access_token(
                &account.tokens.access_token,
            )
        }),
        email: normalize_email(Some(account.email.as_str())),
        expires_ms: decode_token_exp_ms(&account.tokens.access_token),
    }
}

fn snapshot_from_access_token(
    access_token: &str,
    account_id_hint: Option<String>,
    email_hint: Option<String>,
    expires_hint: Option<i64>,
) -> CodexCredentialSnapshot {
    let payload = codex_account::decode_jwt_payload(access_token).ok();
    let account_id = account_id_hint
        .or_else(|| codex_account::extract_chatgpt_account_id_from_access_token(access_token));
    let email = email_hint
        .or_else(|| normalize_email(payload.as_ref().and_then(|item| item.email.as_deref())));
    let expires_ms = expires_hint.or_else(|| decode_token_exp_ms(access_token));
    CodexCredentialSnapshot {
        account_id,
        email,
        expires_ms,
    }
}

fn read_codex_auth_snapshot() -> Result<CodexCredentialSnapshot, String> {
    let auth_path = codex_account::get_auth_json_path();
    let content = fs::read_to_string(&auth_path)
        .map_err(|e| format!("读取 Codex auth.json 失败 ({}): {}", auth_path.display(), e))?;
    let parsed: Value = serde_json::from_str(&content)
        .map_err(|e| format!("解析 Codex auth.json 失败 ({}): {}", auth_path.display(), e))?;
    let tokens = parsed
        .get("tokens")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("Codex auth.json 缺少 tokens 字段 ({})", auth_path.display()))?;
    let access_token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!(
                "Codex auth.json 缺少 tokens.access_token ({})",
                auth_path.display()
            )
        })?;
    Ok(snapshot_from_access_token(
        access_token,
        normalize_non_empty(tokens.get("account_id").and_then(Value::as_str)),
        normalize_email(tokens.get("email").and_then(Value::as_str)),
        None,
    ))
}

#[cfg(target_os = "macos")]
fn build_codex_keychain_account(base_dir: &Path) -> String {
    let resolved_home = fs::canonicalize(base_dir).unwrap_or_else(|_| base_dir.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(resolved_home.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let digest_hex = format!("{:x}", digest);
    format!("cli|{}", &digest_hex[..16])
}

#[cfg(target_os = "macos")]
fn read_codex_keychain_snapshot() -> Result<Option<CodexCredentialSnapshot>, String> {
    let codex_home = codex_account::get_codex_home();
    let keychain_account = build_codex_keychain_account(&codex_home);
    let output = Command::new("security")
        .arg("find-generic-password")
        .arg("-s")
        .arg(CODEX_KEYCHAIN_SERVICE)
        .arg("-a")
        .arg(&keychain_account)
        .arg("-w")
        .output()
        .map_err(|e| format!("调用 security 读取 Codex keychain 失败: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "读取 Codex keychain 失败: status={}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let secret = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if secret.is_empty() {
        return Err("读取 Codex keychain 失败: 返回值为空".to_string());
    }

    let parsed: Value = serde_json::from_str(&secret)
        .map_err(|e| format!("解析 Codex keychain JSON 失败: {}", e))?;
    let tokens = parsed
        .get("tokens")
        .and_then(Value::as_object)
        .ok_or_else(|| "Codex keychain 缺少 tokens 字段".to_string())?;
    let access_token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| "Codex keychain 缺少 tokens.access_token".to_string())?;

    Ok(Some(snapshot_from_access_token(
        access_token,
        normalize_non_empty(tokens.get("account_id").and_then(Value::as_str)),
        normalize_email(tokens.get("email").and_then(Value::as_str)),
        None,
    )))
}

#[cfg(not(target_os = "macos"))]
fn read_codex_keychain_snapshot() -> Result<Option<CodexCredentialSnapshot>, String> {
    Ok(None)
}

fn read_openclaw_default_snapshot(path: &Path) -> Result<CodexCredentialSnapshot, String> {
    let database_path = path
        .parent()
        .map(|parent| parent.join("openclaw-agent.sqlite"));
    let parsed: Value = if let Some(database_path) = database_path.filter(|path| path.exists()) {
        let connection = Connection::open(&database_path).map_err(|error| {
            format!(
                "打开 OpenClaw 凭据数据库失败 ({}): {error}",
                database_path.display()
            )
        })?;
        let content: String = connection
            .query_row(
                "SELECT store_json FROM auth_profile_store WHERE store_key = 'primary'",
                [],
                |row| row.get(0),
            )
            .map_err(|error| format!("读取 OpenClaw SQLite 凭据失败: {error}"))?;
        serde_json::from_str(&content)
            .map_err(|error| format!("解析 OpenClaw SQLite 凭据失败: {error}"))?
    } else {
        let content = fs::read_to_string(path).map_err(|e| {
            format!(
                "读取 OpenClaw auth-profiles.json 失败 ({}): {}",
                path.display(),
                e
            )
        })?;
        serde_json::from_str(&content).map_err(|e| {
            format!(
                "解析 OpenClaw auth-profiles.json 失败 ({}): {}",
                path.display(),
                e
            )
        })?
    };
    let profile = parsed
        .get("profiles")
        .and_then(Value::as_object)
        .and_then(|profiles| profiles.get(OPENCLAW_CODEX_PROFILE_ID))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            format!(
                "OpenClaw auth-profiles.json 缺少 {} ({})",
                OPENCLAW_CODEX_PROFILE_ID,
                path.display()
            )
        })?;

    let access_token = profile
        .get("access")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!(
                "OpenClaw {} 缺少 access ({})",
                OPENCLAW_CODEX_PROFILE_ID,
                path.display()
            )
        })?;
    Ok(snapshot_from_access_token(
        access_token,
        normalize_non_empty(profile.get("accountId").and_then(Value::as_str)),
        normalize_email(profile.get("email").and_then(Value::as_str)),
        value_to_i64(profile.get("expires")),
    ))
}

fn snapshot_matches(
    expected: &CodexCredentialSnapshot,
    actual: &CodexCredentialSnapshot,
    require_email: bool,
) -> bool {
    let account_id_matches = match expected.account_id.as_ref() {
        Some(expected_account_id) => actual.account_id.as_ref() == Some(expected_account_id),
        None => true,
    };
    let email_matches = if require_email {
        match expected.email.as_ref() {
            Some(expected_email) => actual.email.as_ref() == Some(expected_email),
            None => true,
        }
    } else {
        true
    };
    let expires_matches = match expected.expires_ms {
        Some(expected_expires) => actual.expires_ms == Some(expected_expires),
        None => true,
    };
    account_id_matches && email_matches && expires_matches
}

fn snapshot_to_log(snapshot: &CodexCredentialSnapshot) -> String {
    format!(
        "account_id={:?}, email={:?}, expires_ms={:?}",
        snapshot.account_id, snapshot.email, snapshot.expires_ms
    )
}

fn verify_openclaw_codex_sync(
    account: &CodexAccount,
    openclaw_auth_path: &Path,
) -> Result<(), String> {
    let expected = expected_snapshot_from_account(account);
    let codex_auth_snapshot = read_codex_auth_snapshot()?;
    let openclaw_snapshot = read_openclaw_default_snapshot(openclaw_auth_path)?;
    let keychain_snapshot = read_codex_keychain_snapshot()?;

    let mut issues: Vec<String> = Vec::new();
    if !snapshot_matches(&expected, &codex_auth_snapshot, false) {
        issues.push(format!(
            "Codex auth.json 不一致: expected[{}], actual[{}]",
            snapshot_to_log(&expected),
            snapshot_to_log(&codex_auth_snapshot)
        ));
    }

    if let Some(keychain) = keychain_snapshot.as_ref() {
        if !snapshot_matches(&expected, keychain, false) {
            issues.push(format!(
                "Codex keychain 不一致: expected[{}], actual[{}]",
                snapshot_to_log(&expected),
                snapshot_to_log(keychain)
            ));
        }
    }

    if !snapshot_matches(&expected, &openclaw_snapshot, true) {
        issues.push(format!(
            "OpenClaw auth-profiles(default) 不一致: expected[{}], actual[{}]",
            snapshot_to_log(&expected),
            snapshot_to_log(&openclaw_snapshot)
        ));
    }

    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues.join(" | "))
    }
}

fn try_reload_openclaw_secrets() -> bool {
    #[derive(Clone)]
    struct ReloadCommand {
        program: String,
        args: Vec<String>,
        label: String,
    }

    fn push_reload_command(commands: &mut Vec<ReloadCommand>, command: ReloadCommand) {
        if commands
            .iter()
            .any(|item| item.program == command.program && item.args == command.args)
        {
            return;
        }
        commands.push(command);
    }

    fn get_reload_commands() -> Vec<ReloadCommand> {
        let mut commands: Vec<ReloadCommand> = Vec::new();

        if let Ok(cli_path) = std::env::var("OPENCLAW_CLI_PATH") {
            let trimmed = cli_path.trim();
            if !trimmed.is_empty() {
                push_reload_command(
                    &mut commands,
                    ReloadCommand {
                        program: trimmed.to_string(),
                        args: vec!["secrets".to_string(), "reload".to_string()],
                        label: format!("OPENCLAW_CLI_PATH ({})", trimmed),
                    },
                );
            }
        }

        if let Some((node, script)) = find_openclaw_gateway_runtime() {
            push_reload_command(
                &mut commands,
                ReloadCommand {
                    program: node,
                    args: vec![script, "secrets".to_string(), "reload".to_string()],
                    label: "Gateway runtime secrets reload".to_string(),
                },
            );
        }

        push_reload_command(
            &mut commands,
            ReloadCommand {
                program: "openclaw".to_string(),
                args: vec!["secrets".to_string(), "reload".to_string()],
                label: "openclaw secrets reload".to_string(),
            },
        );

        let mut mjs_candidates: Vec<PathBuf> = Vec::new();
        if let Ok(repo_dir) = std::env::var("OPENCLAW_REPO_DIR") {
            let trimmed = repo_dir.trim();
            if !trimmed.is_empty() {
                mjs_candidates.push(resolve_user_path(trimmed).join("openclaw.mjs"));
            }
        }
        mjs_candidates.push(PathBuf::from("/private/var/www/openclaw/openclaw.mjs"));

        if let Ok(current_dir) = std::env::current_dir() {
            mjs_candidates.push(current_dir.join("openclaw.mjs"));
            if let Some(parent) = current_dir.parent() {
                mjs_candidates.push(parent.join("openclaw").join("openclaw.mjs"));
            }
        }

        if let Some(home) = dirs::home_dir() {
            mjs_candidates.push(home.join("openclaw").join("openclaw.mjs"));
        }

        for script_path in mjs_candidates {
            if !script_path.exists() {
                continue;
            }
            let script = script_path.to_string_lossy().to_string();
            push_reload_command(
                &mut commands,
                ReloadCommand {
                    program: "node".to_string(),
                    args: vec![script.clone(), "secrets".to_string(), "reload".to_string()],
                    label: format!("node {} secrets reload", script),
                },
            );
            push_reload_command(
                &mut commands,
                ReloadCommand {
                    program: script.clone(),
                    args: vec!["secrets".to_string(), "reload".to_string()],
                    label: format!("{} secrets reload", script),
                },
            );
        }

        commands
    }

    let commands = get_reload_commands();
    let mut not_found_labels: Vec<String> = Vec::new();

    for reload_command in commands {
        let mut command = Command::new(&reload_command.program);
        command.args(&reload_command.args);
        command.env("CODEX_HOME", codex_account::get_codex_home());

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }

        match command.output() {
            Ok(output) => {
                if output.status.success() {
                    logger::log_info(&format!(
                        "OpenClaw secrets.reload 已触发（{}）",
                        reload_command.label
                    ));
                    return true;
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    logger::log_warn(&format!(
                        "OpenClaw secrets.reload 执行失败（{}，status={}）: stderr={}, stdout={}",
                        reload_command.label,
                        output.status,
                        if stderr.is_empty() {
                            "<empty>"
                        } else {
                            &stderr
                        },
                        if stdout.is_empty() {
                            "<empty>"
                        } else {
                            &stdout
                        }
                    ));
                }
                continue;
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                not_found_labels.push(reload_command.label.clone());
                continue;
            }
            Err(err) => {
                logger::log_info(&format!(
                    "未执行 OpenClaw secrets.reload（{}，调用失败）: {}",
                    reload_command.label, err
                ));
                continue;
            }
        }
    }

    logger::log_info(&format!(
        "未执行 OpenClaw secrets.reload（未找到可执行命令）: {}",
        if not_found_labels.is_empty() {
            "<none>".to_string()
        } else {
            not_found_labels.join(" | ")
        }
    ));
    false
}

fn try_restart_openclaw_gateway() -> bool {
    #[derive(Clone)]
    struct RestartCommand {
        program: String,
        args: Vec<String>,
        label: String,
    }

    fn push_restart_command(commands: &mut Vec<RestartCommand>, command: RestartCommand) {
        if commands
            .iter()
            .any(|item| item.program == command.program && item.args == command.args)
        {
            return;
        }
        commands.push(command);
    }

    fn get_restart_commands() -> Vec<RestartCommand> {
        let mut commands: Vec<RestartCommand> = Vec::new();

        if let Ok(cli_path) = std::env::var("OPENCLAW_CLI_PATH") {
            let trimmed = cli_path.trim();
            if !trimmed.is_empty() {
                push_restart_command(
                    &mut commands,
                    RestartCommand {
                        program: trimmed.to_string(),
                        args: vec!["gateway".to_string(), "restart".to_string()],
                        label: format!("OPENCLAW_CLI_PATH ({}) gateway restart", trimmed),
                    },
                );
            }
        }

        if let Some((node, script)) = find_openclaw_gateway_runtime() {
            push_restart_command(
                &mut commands,
                RestartCommand {
                    program: node,
                    args: vec![script, "gateway".to_string(), "restart".to_string()],
                    label: "Gateway runtime gateway restart".to_string(),
                },
            );
        }

        push_restart_command(
            &mut commands,
            RestartCommand {
                program: "openclaw".to_string(),
                args: vec!["gateway".to_string(), "restart".to_string()],
                label: "openclaw gateway restart".to_string(),
            },
        );

        let mut mjs_candidates: Vec<PathBuf> = Vec::new();
        if let Ok(repo_dir) = std::env::var("OPENCLAW_REPO_DIR") {
            let trimmed = repo_dir.trim();
            if !trimmed.is_empty() {
                mjs_candidates.push(resolve_user_path(trimmed).join("openclaw.mjs"));
            }
        }
        mjs_candidates.push(PathBuf::from("/private/var/www/openclaw/openclaw.mjs"));

        if let Ok(current_dir) = std::env::current_dir() {
            mjs_candidates.push(current_dir.join("openclaw.mjs"));
            if let Some(parent) = current_dir.parent() {
                mjs_candidates.push(parent.join("openclaw").join("openclaw.mjs"));
            }
        }

        if let Some(home) = dirs::home_dir() {
            mjs_candidates.push(home.join("openclaw").join("openclaw.mjs"));
        }

        for script_path in mjs_candidates {
            if !script_path.exists() {
                continue;
            }
            let script = script_path.to_string_lossy().to_string();
            push_restart_command(
                &mut commands,
                RestartCommand {
                    program: "node".to_string(),
                    args: vec![script.clone(), "gateway".to_string(), "restart".to_string()],
                    label: format!("node {} gateway restart", script),
                },
            );
            push_restart_command(
                &mut commands,
                RestartCommand {
                    program: script.clone(),
                    args: vec!["gateway".to_string(), "restart".to_string()],
                    label: format!("{} gateway restart", script),
                },
            );
        }

        commands
    }

    let commands = get_restart_commands();
    let mut not_found_labels: Vec<String> = Vec::new();

    for restart_command in commands {
        let mut command = Command::new(&restart_command.program);
        command.args(&restart_command.args);
        command.env("CODEX_HOME", codex_account::get_codex_home());

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }

        match command.output() {
            Ok(output) => {
                if output.status.success() {
                    logger::log_info(&format!(
                        "OpenClaw gateway.restart 已触发（{}）",
                        restart_command.label
                    ));
                    return true;
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    logger::log_warn(&format!(
                        "OpenClaw gateway.restart 执行失败（{}，status={}）: stderr={}, stdout={}",
                        restart_command.label,
                        output.status,
                        if stderr.is_empty() {
                            "<empty>"
                        } else {
                            &stderr
                        },
                        if stdout.is_empty() {
                            "<empty>"
                        } else {
                            &stdout
                        }
                    ));
                }
                continue;
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                not_found_labels.push(restart_command.label.clone());
                continue;
            }
            Err(err) => {
                logger::log_info(&format!(
                    "未执行 OpenClaw gateway.restart（{}，调用失败）: {}",
                    restart_command.label, err
                ));
                continue;
            }
        }
    }

    logger::log_info(&format!(
        "未执行 OpenClaw gateway.restart（未找到可执行命令）: {}",
        if not_found_labels.is_empty() {
            "<none>".to_string()
        } else {
            not_found_labels.join(" | ")
        }
    ));
    false
}

/// 使用 Cockpit 当前 Codex 登录覆盖 OpenClaw 的 openai:default 凭据。
pub fn replace_openai_codex_entry_from_codex(account: &CodexAccount) -> Result<(), String> {
    if codex_oauth::is_token_expired(&account.tokens.access_token) {
        return Err("Codex access_token 已过期，无法同步到 OpenClaw".to_string());
    }

    let auth_paths = get_openclaw_auth_profiles_path_candidates()?;
    let mut database_paths = get_openclaw_auth_database_path_candidates()?;
    if !database_paths.iter().any(|path| path.exists()) {
        let _ = run_openclaw_cli(&["models", "status", "--json"]);
        database_paths = get_openclaw_auth_database_path_candidates()?;
    }
    let target_auth_path = get_openclaw_auth_profiles_path()?;
    let source_auth_path = auth_paths.iter().find(|path| path.exists()).cloned();

    let mut auth_json = if let Some(source_path) = source_auth_path.as_ref() {
        let content = fs::read_to_string(source_path).map_err(|e| {
            format!(
                "读取 OpenClaw auth-profiles.json 失败 ({}): {}",
                source_path.display(),
                e
            )
        })?;
        serde_json::from_str::<Value>(&content).map_err(|e| {
            format!(
                "解析 OpenClaw auth-profiles.json 失败 ({}): {}",
                source_path.display(),
                e
            )
        })?
    } else {
        json!({
            "version": OPENCLAW_AUTH_STORE_VERSION,
            "profiles": {}
        })
    };

    let mut had_active_codex_lockout = has_active_openclaw_codex_lockout(&auth_json);
    let payload = build_openclaw_codex_payload(account)?;
    upsert_openclaw_codex_profile(&mut auth_json, payload.clone());
    upsert_openclaw_codex_state(&mut auth_json);

    let mut synced_database = false;
    for database_path in database_paths.iter().filter(|path| path.exists()) {
        had_active_codex_lockout |= sync_openclaw_auth_database(database_path, &payload)?;
        synced_database = true;
        logger::log_info(&format!(
            "已更新 OpenClaw SQLite 凭据中的 {}: {}",
            OPENCLAW_CODEX_PROFILE_ID,
            database_path.display()
        ));
    }

    let content = serde_json::to_string_pretty(&auth_json)
        .map_err(|e| format!("序列化 OpenClaw auth-profiles.json 失败: {}", e))?;
    atomic_write(&target_auth_path, &content)?;

    for extra_path in &auth_paths {
        if extra_path == &target_auth_path || !extra_path.exists() {
            continue;
        }
        if let Err(err) = atomic_write(extra_path, &content) {
            logger::log_warn(&format!(
                "同步 OpenClaw 备用 auth-profiles.json 失败 ({}): {}",
                extra_path.display(),
                err
            ));
        }
    }

    if let Some(source_path) = source_auth_path {
        if source_path != target_auth_path {
            logger::log_info(&format!(
                "OpenClaw auth-profiles.json 已迁移: {} -> {}",
                source_path.display(),
                target_auth_path.display()
            ));
        }
    }

    logger::log_info(&format!(
        "已更新 OpenClaw auth-profiles.json 中的 {}: {}",
        OPENCLAW_CODEX_PROFILE_ID,
        target_auth_path.display()
    ));

    let first_reload_ok = try_reload_openclaw_secrets();
    let first_restart_ok = try_restart_openclaw_gateway();
    if !first_restart_ok {
        logger::log_warn(
            "OpenClaw secrets.reload 后未能触发 gateway.restart；如切号后仍未即时生效，请手动执行 openclaw gateway restart",
        );
    }

    match verify_openclaw_codex_sync(account, &target_auth_path) {
        Ok(()) => {
            logger::log_info("OpenClaw/Codex 凭据一致性校验通过");
        }
        Err(first_err) => {
            logger::log_warn(&format!(
                "OpenClaw/Codex 凭据一致性校验首次失败，准备二次 reload + gateway restart: {}",
                first_err
            ));
            let second_reload_ok = try_reload_openclaw_secrets();
            let second_restart_ok = try_restart_openclaw_gateway();
            if !second_restart_ok {
                logger::log_warn(
                    "OpenClaw 二次 secrets.reload 后未能触发 gateway.restart；如切号后仍未即时生效，请手动执行 openclaw gateway restart",
                );
            }
            match verify_openclaw_codex_sync(account, &target_auth_path) {
                Ok(()) => {
                    logger::log_info("OpenClaw/Codex 凭据一致性校验二次重试后通过");
                }
                Err(second_err) => {
                    let reload_status = if first_reload_ok || second_reload_ok {
                        "已尝试 reload"
                    } else {
                        "reload 未成功触发"
                    };
                    let restart_status = if first_restart_ok || second_restart_ok {
                        "已尝试 gateway restart"
                    } else {
                        "gateway restart 未成功触发"
                    };
                    return Err(format!(
                        "OpenClaw/Codex 凭据一致性校验失败（{}，{}）: {}",
                        reload_status, restart_status, second_err
                    ));
                }
            }
        }
    }

    if had_active_codex_lockout && !first_restart_ok {
        logger::log_info(
            "检测到 OpenClaw openai:default 存在活动失败态，尝试 gateway restart 清理运行态缓存",
        );
        if try_restart_openclaw_gateway() {
            let _ = try_reload_openclaw_secrets();
            match verify_openclaw_codex_sync(account, &target_auth_path) {
                Ok(()) => {
                    logger::log_info("OpenClaw/Codex 凭据一致性校验在活动失败态修复后通过");
                }
                Err(err) => {
                    logger::log_warn(&format!(
                        "OpenClaw/Codex 活动失败态修复后校验仍未通过: {}",
                        err
                    ));
                }
            }
        } else {
            logger::log_warn("未能触发 OpenClaw gateway.restart；若模型下拉仍不可选，请手动执行 openclaw gateway restart");
        }
    }

    if !synced_database {
        logger::log_info("OpenClaw 尚未创建 SQLite 凭据库，已写入旧版 JSON 兼容存储");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        merge_openclaw_gateway_env, parse_openclaw_gateway_runtime, sync_openclaw_auth_database,
        validate_openclaw_model_settings, OPENCLAW_CODEX_PROFILE_ID,
    };
    use rusqlite::Connection;
    use serde_json::{json, Value};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Gateway 路径和模型设置都应通过最小安全校验。
    #[test]
    fn validates_openclaw_runtime_and_settings() {
        let command = r#""C:\Program Files\nodejs\node.exe" "C:\Users\Demo\node_modules\openclaw\dist\index.js" gateway --port 18789"#;
        let (node, script) = parse_openclaw_gateway_runtime(command).expect("解析 Gateway 路径");
        assert_eq!(node.to_string_lossy(), r"C:\Program Files\nodejs\node.exe");
        assert!(script
            .to_string_lossy()
            .replace('\\', "/")
            .ends_with("node_modules/openclaw/dist/index.js"));
        assert!(validate_openclaw_model_settings("openai/gpt-5.6-luna", "low").is_ok());
        assert!(validate_openclaw_model_settings("openai/gpt;bad", "low").is_err());
    }

    /// Gateway 环境同步应保留其他变量并替换旧代理值。
    #[test]
    fn merges_openclaw_gateway_proxy_env() {
        let merged = merge_openclaw_gateway_env(
            "TOKEN=keep\nhttp_proxy=http://old:1\nNO_PROXY=old\n",
            "http://127.0.0.1:10808",
            "localhost,127.0.0.1",
        );
        assert_eq!(
            merged,
            "TOKEN=keep\nHTTP_PROXY=http://127.0.0.1:10808\nHTTPS_PROXY=http://127.0.0.1:10808\nNO_PROXY=localhost,127.0.0.1\n"
        );
    }

    /// SQLite 凭据同步应迁移旧命名空间并清理失败态。
    #[test]
    fn syncs_current_openclaw_sqlite_auth_store() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时间有效")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "cockpit-openclaw-auth-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("创建测试目录");
        let database_path = directory.join("openclaw-agent.sqlite");
        let connection = Connection::open(&database_path).expect("创建测试数据库");
        connection
            .execute_batch(
                "CREATE TABLE auth_profile_store (store_key TEXT PRIMARY KEY, store_json TEXT NOT NULL, updated_at INTEGER NOT NULL);\
                 CREATE TABLE auth_profile_state (state_key TEXT PRIMARY KEY, state_json TEXT NOT NULL, updated_at INTEGER NOT NULL);\
                 INSERT INTO auth_profile_store VALUES ('primary', '{\"version\":1,\"profiles\":{\"openai-codex:old\":{\"type\":\"oauth\",\"provider\":\"openai-codex\"}}}', 0);\
                 INSERT INTO auth_profile_state VALUES ('primary', '{\"usageStats\":{\"openai-codex:old\":{\"disabledUntil\":9999999999999}}}', 0);",
            )
            .expect("初始化测试数据库");
        drop(connection);

        let payload = json!({
            "type": "oauth",
            "provider": "openai",
            "access": "access",
            "refresh": "refresh",
            "expires": 9_999_999_999_999_i64,
        });
        assert!(sync_openclaw_auth_database(&database_path, &payload).expect("同步凭据"));

        let connection = Connection::open(&database_path).expect("重开测试数据库");
        let store_content: String = connection
            .query_row(
                "SELECT store_json FROM auth_profile_store WHERE store_key = 'primary'",
                [],
                |row| row.get(0),
            )
            .expect("读取凭据");
        let store: Value = serde_json::from_str(&store_content).expect("解析凭据");
        assert_eq!(
            store.pointer("/profiles/openai:default/provider"),
            Some(&Value::String("openai".to_string()))
        );
        assert!(store.pointer("/profiles/openai-codex:old").is_none());
        let state_content: String = connection
            .query_row(
                "SELECT state_json FROM auth_profile_state WHERE state_key = 'primary'",
                [],
                |row| row.get(0),
            )
            .expect("读取凭据状态");
        let state: Value = serde_json::from_str(&state_content).expect("解析凭据状态");
        assert_eq!(
            state.pointer("/lastGood/openai"),
            Some(&Value::String(OPENCLAW_CODEX_PROFILE_ID.to_string()))
        );
        assert!(state.pointer("/usageStats/openai-codex:old").is_none());
        drop(connection);
        fs::remove_dir_all(directory).expect("清理测试目录");
    }
}
