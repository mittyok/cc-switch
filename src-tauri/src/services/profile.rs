//! 项目 Profile 编排服务
//!
//! Profile 是**全应用共享的项目实体**（用户拥有的项目就那几个），payload
//! 按 app 分槽存配置快照（供应商 / MCP / Skills / Prompt）。快照与应用
//! 均**按分组（scope）操作**：Claude Code 与 Codex 的工作目录往往不同
//! （各在各的项目里），因此各组独立指向自己的当前项目、只拍/只应用组内
//! 槽位，互不牵连；重命名/删除作用于共享实体本身。
//! 应用（apply）时复用现有切换原语批量落地：
//! - 供应商：`ProviderService::switch`（内建代理接管热切换与接管下禁切官方）
//! - MCP：`McpService::toggle_app`（改标志 + 单 server 物化）
//! - Skills：`SkillService::toggle_app`（改标志 + 单 skill 物化）
//! - Prompt：`PromptService::enable_prompt`（互斥激活 + 原子写 live）
//!
//! apply 为 best-effort：单项失败收集为 warning 继续，不整体回滚。

use std::collections::HashSet;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::app_config::AppType;
use crate::database::Database;
use crate::database::Profile;
use crate::error::AppError;
use crate::services::{McpService, PromptService, ProviderService, SkillService};
use crate::store::AppState;

/// Profile 操作的应用分组：项目实体全应用共享，但快照/应用/当前指针按组进行。
///
/// Claude Code 与 Claude Desktop 的供应商在 cc-switch 中是独立切换的，
/// 因此各自拥有独立的项目分组。两者 live 文件零交集
///（`~/.claude` / `Application Support/Claude-3p`），分组切换互不干扰。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileScope {
    Claude,
    #[serde(rename = "claude-desktop")]
    ClaudeDesktop,
    Codex,
}

impl ProfileScope {
    /// 全部分组（扩展新分组时同步扩展 apps/for_app 与前端 scope.ts 镜像）
    pub const ALL: [ProfileScope; 3] = [
        ProfileScope::Claude,
        ProfileScope::ClaudeDesktop,
        ProfileScope::Codex,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            ProfileScope::Claude => "claude",
            ProfileScope::ClaudeDesktop => "claude-desktop",
            ProfileScope::Codex => "codex",
        }
    }

    pub fn parse(value: &str) -> Result<Self, AppError> {
        match value {
            "claude" => Ok(ProfileScope::Claude),
            "claude-desktop" => Ok(ProfileScope::ClaudeDesktop),
            "codex" => Ok(ProfileScope::Codex),
            other => Err(AppError::InvalidInput(format!(
                "Unknown profile scope: {other}"
            ))),
        }
    }

    /// 组内受管应用（快照与 apply 只作用于这些 app 的槽位）
    pub fn apps(&self) -> &'static [AppType] {
        match self {
            ProfileScope::Claude => &[AppType::Claude],
            ProfileScope::ClaudeDesktop => &[AppType::ClaudeDesktop],
            ProfileScope::Codex => &[AppType::Codex],
        }
    }

    /// 应用页 → 所属分组（Profile 不支持的应用返回 None）
    pub fn for_app(app: &AppType) -> Option<Self> {
        match app {
            AppType::Claude => Some(ProfileScope::Claude),
            AppType::ClaudeDesktop => Some(ProfileScope::ClaudeDesktop),
            AppType::Codex => Some(ProfileScope::Codex),
            _ => None,
        }
    }
}

/// 按 app 分槽的载荷容器；字段名与 AppType 的 serde 形式一致
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PerApp<T> {
    pub claude: T,
    #[serde(rename = "claude-desktop")]
    pub claude_desktop: T,
    pub codex: T,
}

/// Profile 快照里的 provider/prompt 槽位历史上应保存字符串 id；线上曾出现
/// 旧实现把完整对象写入槽位，导致应用项目时报 `invalid type: map, expected a string`。
/// 这里仅在快照边界做兼容：能从常见 id 字段恢复则恢复，否则视为未拍过该槽位。
fn deserialize_per_app_optional_string<'de, D>(
    deserializer: D,
) -> Result<PerApp<Option<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?.unwrap_or(Value::Null);
    Ok(per_app_optional_string_from_value(value))
}

fn per_app_optional_string_from_value(value: Value) -> PerApp<Option<String>> {
    let mut per_app = PerApp::default();
    let Some(map) = value.as_object() else {
        return per_app;
    };

    per_app.claude = optional_id_from_value(map.get("claude"));
    per_app.claude_desktop = optional_id_from_value(map.get("claude-desktop"));
    per_app.codex = optional_id_from_value(map.get("codex"));
    per_app
}

fn optional_id_from_value(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(id)) if !id.is_empty() => Some(id.clone()),
        Some(Value::Object(map)) => ["id", "providerId", "promptId"]
            .iter()
            .find_map(|key| map.get(*key).and_then(Value::as_str))
            .filter(|id| !id.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

/// 故障转移队列新结构只保存 provider id 数组；兼容旧/错误快照中保存完整
/// queue item 对象数组的形态，避免单个历史项目无法被应用。
fn deserialize_per_app_optional_string_vec<'de, D>(
    deserializer: D,
) -> Result<PerApp<Option<Vec<String>>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?.unwrap_or(Value::Null);
    Ok(per_app_optional_string_vec_from_value(value))
}

fn per_app_optional_string_vec_from_value(value: Value) -> PerApp<Option<Vec<String>>> {
    let mut per_app = PerApp::default();
    let Some(map) = value.as_object() else {
        return per_app;
    };

    per_app.claude = optional_id_vec_from_value(map.get("claude"));
    per_app.claude_desktop = optional_id_vec_from_value(map.get("claude-desktop"));
    per_app.codex = optional_id_vec_from_value(map.get("codex"));
    per_app
}

fn optional_id_vec_from_value(value: Option<&Value>) -> Option<Vec<String>> {
    match value {
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .filter_map(|item| optional_id_from_value(Some(item)))
                .collect(),
        ),
        Some(Value::Object(map)) => ["providers", "items", "queue"]
            .iter()
            .find_map(|key| map.get(*key))
            .and_then(|items| optional_id_vec_from_value(Some(items)))
            .or_else(|| optional_id_from_value(value).map(|id| vec![id])),
        Some(Value::Null) | None => None,
        _ => Some(vec![]),
    }
}

impl<T> PerApp<T> {
    pub fn get(&self, app: &AppType) -> Option<&T> {
        match app {
            AppType::Claude => Some(&self.claude),
            AppType::ClaudeDesktop => Some(&self.claude_desktop),
            AppType::Codex => Some(&self.codex),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, app: &AppType) -> Option<&mut T> {
        match app {
            AppType::Claude => Some(&mut self.claude),
            AppType::ClaudeDesktop => Some(&mut self.claude_desktop),
            AppType::Codex => Some(&mut self.codex),
            _ => None,
        }
    }
}

/// Profile 的 JSON 快照结构（与前端 TS 类型严格对应）
///
/// 所有槽位都是 Option：None = 该侧从未拍过快照（应用时不动），
/// 与"拍到的就是空集/无激活项"（Some(空)，应用时清空启用）严格区分——
/// 在 Codex 页选中一个只在 Claude 页建过的项目不能误清 Codex 的启用状态。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfilePayload {
    /// 每 app 的当前供应商 id
    #[serde(deserialize_with = "deserialize_per_app_optional_string")]
    pub providers: PerApp<Option<String>>,
    /// 每 app 的故障转移队列 provider id，顺序即故障转移优先级
    #[serde(deserialize_with = "deserialize_per_app_optional_string_vec")]
    pub failover: PerApp<Option<Vec<String>>>,
    /// 每 app 启用的 MCP server id 集合
    pub mcp: PerApp<Option<Vec<String>>>,
    /// 每 app 启用的 Skill id 集合
    pub skills: PerApp<Option<Vec<String>>>,
    /// 每 app 激活的 prompt id
    #[serde(deserialize_with = "deserialize_per_app_optional_string")]
    pub prompts: PerApp<Option<String>>,
}

impl ProfilePayload {
    /// 用另一份快照覆盖本载荷中某分组的槽位，其余分组原样保留
    /// （"以当前状态更新"只更新发起页所属分组，避免把别的应用
    /// 正处于其他项目的状态串进来）
    pub fn merge_scope_from(&mut self, other: &ProfilePayload, scope: ProfileScope) {
        for app in scope.apps() {
            if let (Some(dst), Some(src)) = (self.providers.get_mut(app), other.providers.get(app))
            {
                *dst = src.clone();
            }
            if let (Some(dst), Some(src)) = (self.failover.get_mut(app), other.failover.get(app)) {
                *dst = src.clone();
            }
            if let (Some(dst), Some(src)) = (self.mcp.get_mut(app), other.mcp.get(app)) {
                *dst = src.clone();
            }
            if let (Some(dst), Some(src)) = (self.skills.get_mut(app), other.skills.get(app)) {
                *dst = src.clone();
            }
            if let (Some(dst), Some(src)) = (self.prompts.get_mut(app), other.prompts.get(app)) {
                *dst = src.clone();
            }
        }
    }

    /// 某分组是否拍过快照（任一槽位非 None 即视为拍过）
    pub fn scope_captured(&self, scope: ProfileScope) -> bool {
        scope.apps().iter().any(|app| {
            self.providers.get(app).is_some_and(|s| s.is_some())
                || self.failover.get(app).is_some_and(|s| s.is_some())
                || self.mcp.get(app).is_some_and(|s| s.is_some())
                || self.skills.get(app).is_some_and(|s| s.is_some())
                || self.prompts.get(app).is_some_and(|s| s.is_some())
        })
    }
}

/// 计算从当前启用状态到目标集合的最小 toggle 集
///
/// 返回 (需要执行的 (id, enabled) 列表, payload 中已不存在于 DB 的悬空 id 列表)
fn plan_toggles(
    current: &[(String, bool)],
    target_ids: &[String],
) -> (Vec<(String, bool)>, Vec<String>) {
    let existing: HashSet<&str> = current.iter().map(|(id, _)| id.as_str()).collect();
    let target: HashSet<&str> = target_ids.iter().map(|s| s.as_str()).collect();

    let toggles = current
        .iter()
        .filter(|(id, enabled)| target.contains(id.as_str()) != *enabled)
        .map(|(id, enabled)| (id.clone(), !enabled))
        .collect();

    let dangling = target_ids
        .iter()
        .filter(|id| !existing.contains(id.as_str()))
        .cloned()
        .collect();

    (toggles, dangling)
}

/// 计算从当前故障转移队列到目标顺序的最小可恢复计划。
///
/// 旧项目快照切换时必须恢复完整优先级顺序；快照中的悬空 provider id
/// 按既有 MCP/Skill 容错策略跳过，避免单个已删除节点中断项目切换。
fn plan_failover_restore(
    existing_provider_ids: &HashSet<&str>,
    current_queue_ids: &[String],
    target_queue_ids: &[String],
) -> (Vec<String>, Vec<String>, bool) {
    let mut restored = Vec::new();
    let mut dangling = Vec::new();
    let mut seen = HashSet::new();

    for id in target_queue_ids {
        if !existing_provider_ids.contains(id.as_str()) {
            dangling.push(id.clone());
            continue;
        }
        if seen.insert(id.as_str()) {
            restored.push(id.clone());
        }
    }

    let changed = restored != current_queue_ids;
    (restored, dangling, changed)
}

fn none_profile_payload_key(scope: ProfileScope) -> String {
    format!("profile_none_payload_{}", scope.as_str())
}

fn apply_payload(
    state: &AppState,
    payload: &ProfilePayload,
    scope: ProfileScope,
    source_id: &str,
) -> Result<(Vec<String>, bool), AppError> {
    let mut warnings = Vec::new();

    if !payload.scope_captured(scope) {
        warnings.push(format!(
            "no {} configuration captured in this project yet; marked as current without changes (it will be saved automatically when you switch away)",
            scope.as_str()
        ));
    }

    for app in scope.apps().iter() {
        let app_str = app.as_str();

        // 1. 供应商。项目切换必须保持路由开关的最后状态；接管开启时 ProviderService::switch
        // 会走热切换路径，避免用户反馈的“切换项目后路由开关总是关闭”。
        if let Some(Some(target_pid)) = payload.providers.get(app) {
            let providers = state.db.get_all_providers(app_str)?;
            if !providers.contains_key(target_pid) {
                warnings.push(format!(
                    "[{app_str}] provider '{target_pid}' no longer exists, skipped"
                ));
            } else {
                let current = crate::settings::get_effective_current_provider(&state.db, app)?;
                if current.as_deref() != Some(target_pid.as_str()) {
                    match ProviderService::switch(state, app.clone(), target_pid) {
                        Ok(result) => warnings.extend(result.warnings),
                        Err(e) => warnings.push(format!(
                            "[{app_str}] switch provider '{target_pid}' failed: {e}"
                        )),
                    }
                }
            }
        }

        // 2. 故障转移队列（None = 旧快照未包含该槽位，不触碰当前运行时队列）
        if let Some(Some(target_ids)) = payload.failover.get(app) {
            let providers = state.db.get_all_providers(app_str)?;
            let existing_provider_ids: HashSet<&str> =
                providers.keys().map(|id| id.as_str()).collect();
            let current_queue_ids: Vec<String> = state
                .db
                .get_failover_queue(app_str)?
                .into_iter()
                .map(|item| item.provider_id)
                .collect();
            let (restored_ids, dangling, changed) =
                plan_failover_restore(&existing_provider_ids, &current_queue_ids, target_ids);

            for id in dangling {
                warnings.push(format!(
                    "[{app_str}] failover provider '{id}' no longer exists, skipped"
                ));
            }

            if changed {
                if let Err(e) = state.db.replace_failover_queue(app_str, &restored_ids) {
                    warnings.push(format!("[{app_str}] restore failover queue failed: {e}"));
                } else {
                    log::info!(
                        "[Profile] restored failover queue: source_id='{source_id}', scope='{}', app_type='{app_str}', before={:?}, after={:?}",
                        scope.as_str(),
                        current_queue_ids,
                        restored_ids
                    );
                }
            }
        }

        // 3. MCP diff（最小 toggle：仅动目标态≠当前态的条目；None = 该侧未拍过，不动）
        if let Some(Some(target_ids)) = payload.mcp.get(app) {
            let servers = state.db.get_all_mcp_servers()?;
            let current: Vec<(String, bool)> = servers
                .values()
                .map(|s| (s.id.clone(), s.apps.is_enabled_for(app)))
                .collect();
            let (toggles, dangling) = plan_toggles(&current, target_ids);
            for id in dangling {
                warnings.push(format!("[{app_str}] MCP '{id}' no longer exists, skipped"));
            }
            for (id, enabled) in toggles {
                if let Err(e) = McpService::toggle_app(state, &id, app.clone(), enabled) {
                    warnings.push(format!(
                        "[{app_str}] toggle MCP '{id}' -> {enabled} failed: {e}"
                    ));
                }
            }
        }

        // 5. Skills diff（SkillService 返回 anyhow::Result，收进 warning）
        if let Some(Some(target_ids)) = payload.skills.get(app) {
            let skills = state.db.get_all_installed_skills()?;
            let current: Vec<(String, bool)> = skills
                .values()
                .map(|s| (s.id.clone(), s.apps.is_enabled_for(app)))
                .collect();
            let (toggles, dangling) = plan_toggles(&current, target_ids);
            for id in dangling {
                warnings.push(format!(
                    "[{app_str}] skill '{id}' no longer exists, skipped"
                ));
            }
            for (id, enabled) in toggles {
                if let Err(e) = SkillService::toggle_app(&state.db, &id, app, enabled) {
                    warnings.push(format!(
                        "[{app_str}] toggle skill '{id}' -> {enabled} failed: {e}"
                    ));
                }
            }
        }

        // 6. Prompt（None = 不动；已激活则幂等跳过，避免无谓的文件写与备份）
        if let Some(Some(target_prompt)) = payload.prompts.get(app) {
            let prompts = state.db.get_prompts(app_str)?;
            match prompts.get(target_prompt) {
                None => warnings.push(format!(
                    "[{app_str}] prompt '{target_prompt}' no longer exists, skipped"
                )),
                Some(p) if p.enabled => {}
                Some(_) => {
                    if let Err(e) = PromptService::enable_prompt(state, app.clone(), target_prompt)
                    {
                        warnings.push(format!(
                            "[{app_str}] enable prompt '{target_prompt}' failed: {e}"
                        ));
                    }
                }
            }
        }
    }

    // 当前分组内所有接管已关闭；若其它应用也无接管，可停止代理服务。
    let should_stop_proxy = !state.db.is_live_takeover_active_sync();

    Ok((warnings, should_stop_proxy))
}

pub struct ProfileService;

impl ProfileService {
    /// 只读解析项目的 Codex provider 链，供代理请求级路由使用。
    ///
    /// 不调用 [`Self::apply`]、不写 settings，也不缓存查询结果：Codex 的
    /// `ccs_<profileId>_` key 只是本地便利路由，必须避免一次请求改动 UI 当前项目
    /// 或全局 current provider。
    pub fn resolve_codex_provider_chain_for_profile(
        db: &Database,
        profile_id: &str,
    ) -> Result<Option<Vec<String>>, AppError> {
        // 用户在 UI 里只能看到 profile 的 name（如 "GLM"），看不到 UUID id。
        // ccs_<profileId>_ 令牌里填的可能是 name 也可能是 id，先按 id 查、
        // 查不到再按 name 回退，保证两种写法都能路由到正确的 profile。
        let profile = match db.get_profile(profile_id)? {
            Some(p) => Some(p),
            None => db.get_profile_by_name(profile_id)?,
        };
        let Some(profile) = profile else {
            return Ok(None);
        };
        let payload: ProfilePayload = serde_json::from_str(&profile.payload)
            .map_err(|e| AppError::Config(format!("解析 profile payload 失败: {e}")))?;
        let Some(primary_id) = payload
            .providers
            .codex
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            return Ok(Some(Vec::new()));
        };

        let mut chain = vec![primary_id.to_string()];
        if let Some(failover_ids) = payload.failover.codex.as_ref() {
            for provider_id in failover_ids {
                let provider_id = provider_id.trim();
                if !provider_id.is_empty() && !chain.iter().any(|id| id == provider_id) {
                    chain.push(provider_id.to_string());
                }
            }
        }
        Ok(Some(chain))
    }

    /// 抓取分组内应用的当前配置状态生成快照（组外槽位保持默认值）
    pub fn snapshot_current(
        state: &AppState,
        scope: ProfileScope,
    ) -> Result<ProfilePayload, AppError> {
        let mut payload = ProfilePayload::default();
        let mcp_servers = state.db.get_all_mcp_servers()?;
        let skills = state.db.get_all_installed_skills()?;

        for app in scope.apps().iter() {
            if let Some(slot) = payload.providers.get_mut(app) {
                *slot = crate::settings::get_effective_current_provider(&state.db, app)?;
            }
            if let Some(slot) = payload.failover.get_mut(app) {
                *slot = Some(
                    state
                        .db
                        .get_failover_queue(app.as_str())?
                        .into_iter()
                        .map(|item| item.provider_id)
                        .collect(),
                );
            }
            if let Some(slot) = payload.mcp.get_mut(app) {
                *slot = Some(
                    mcp_servers
                        .values()
                        .filter(|s| s.apps.is_enabled_for(app))
                        .map(|s| s.id.clone())
                        .collect(),
                );
            }
            if let Some(slot) = payload.skills.get_mut(app) {
                *slot = Some(
                    skills
                        .values()
                        .filter(|s| s.apps.is_enabled_for(app))
                        .map(|s| s.id.clone())
                        .collect(),
                );
            }
            if let Some(slot) = payload.prompts.get_mut(app) {
                *slot = state
                    .db
                    .get_prompts(app.as_str())?
                    .values()
                    .find(|p| p.enabled)
                    .map(|p| p.id.clone());
            }
        }
        Ok(payload)
    }

    /// 列出所有项目（项目实体全应用共享，current 标记按分组单独读取）
    pub fn list(state: &AppState) -> Result<Vec<Profile>, AppError> {
        state.db.get_all_profiles()
    }

    /// 创建新项目：只拍发起页所属分组的当前状态，其余分组槽位留 None
    /// （其他应用可能正处于别的项目，不能替用户拍进来）
    pub fn create(state: &AppState, name: &str, scope: ProfileScope) -> Result<Profile, AppError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError::InvalidInput("Profile name is empty".to_string()));
        }
        let payload = Self::snapshot_current(state, scope)?;
        let now = chrono::Utc::now().timestamp();
        let profile = Profile {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            payload: serde_json::to_string(&payload)
                .map_err(|e| AppError::Config(format!("序列化 profile payload 失败: {e}")))?,
            sort_order: None,
            created_at: Some(now),
            updated_at: Some(now),
        };
        state.db.save_profile(&profile)?;
        Ok(profile)
    }

    /// 更新项目：重命名（作用于共享实体）和/或以当前状态重拍快照
    /// （resnapshot 只覆盖 scope 分组的槽位，其余分组原样保留；
    /// 快照重拍仅由 [`Self::apply`] 切换前的自动保存触发，UI 不再暴露手动入口）
    pub fn update(
        state: &AppState,
        id: &str,
        name: Option<String>,
        resnapshot: bool,
        scope: Option<ProfileScope>,
    ) -> Result<Profile, AppError> {
        let mut profile = state
            .db
            .get_profile(id)?
            .ok_or_else(|| AppError::InvalidInput(format!("Profile not found: {id}")))?;

        if let Some(name) = name {
            let name = name.trim().to_string();
            if name.is_empty() {
                return Err(AppError::InvalidInput("Profile name is empty".to_string()));
            }
            profile.name = name;
        }
        if resnapshot {
            let scope = scope.ok_or_else(|| {
                AppError::InvalidInput("Resnapshot requires a profile scope".to_string())
            })?;
            let mut payload: ProfilePayload = serde_json::from_str(&profile.payload)
                .map_err(|e| AppError::Config(format!("解析 profile payload 失败: {e}")))?;
            payload.merge_scope_from(&Self::snapshot_current(state, scope)?, scope);
            profile.payload = serde_json::to_string(&payload)
                .map_err(|e| AppError::Config(format!("序列化 profile payload 失败: {e}")))?;
        }
        profile.updated_at = Some(chrono::Utc::now().timestamp());
        state.db.save_profile(&profile)?;
        Ok(profile)
    }

    /// 删除项目；若删除的是某分组当前激活项目，一并清除该分组的激活标记
    pub fn delete(state: &AppState, id: &str) -> Result<(), AppError> {
        state.db.delete_profile(id)?;
        for scope in ProfileScope::ALL {
            if state.db.get_current_profile_id(scope.as_str())?.as_deref() == Some(id) {
                state.db.set_current_profile_id(scope.as_str(), None)?;
            }
        }
        Ok(())
    }

    /// 保存某分组的“未使用项目”运行态。
    ///
    /// “不使用项目”也需要像一个独立工作区一样保留 provider/failover 状态；
    /// 否则从无项目切回具体项目时，会把无项目期间的队列误认为项目队列。
    fn save_none_snapshot(state: &AppState, scope: ProfileScope) -> Result<(), AppError> {
        let payload = Self::snapshot_current(state, scope)?;
        let serialized = serde_json::to_string(&payload)
            .map_err(|e| AppError::Config(format!("序列化 no-profile payload 失败: {e}")))?;
        state
            .db
            .set_setting(&none_profile_payload_key(scope), &serialized)
    }

    fn load_none_snapshot(
        state: &AppState,
        scope: ProfileScope,
    ) -> Result<Option<ProfilePayload>, AppError> {
        state
            .db
            .get_setting(&none_profile_payload_key(scope))?
            .map(|payload| {
                serde_json::from_str(&payload)
                    .map_err(|e| AppError::Config(format!("解析 no-profile payload 失败: {e}")))
            })
            .transpose()
    }

    /// 切到“不使用项目”：先保存当前项目，再恢复该分组上一次的无项目运行态。
    pub fn clear_current(
        state: &AppState,
        scope: ProfileScope,
    ) -> Result<(Vec<String>, bool), AppError> {
        let mut warnings = Vec::new();

        if let Some(current_id) = state.db.get_current_profile_id(scope.as_str())? {
            if let Err(e) = Self::update(state, &current_id, None, true, Some(scope)) {
                warnings.push(format!(
                    "autosave profile '{current_id}' before clearing current project failed: {e}"
                ));
            }
        }

        let should_stop_proxy = if let Some(payload) = Self::load_none_snapshot(state, scope)? {
            let (apply_warnings, should_stop_proxy) =
                apply_payload(state, &payload, scope, "__none__")?;
            warnings.extend(apply_warnings);
            should_stop_proxy
        } else {
            !state.db.is_live_takeover_active_sync()
        };

        state.db.set_current_profile_id(scope.as_str(), None)?;
        Ok((warnings, should_stop_proxy))
    }

    /// 应用项目快照（best-effort，返回 warnings）
    ///
    /// 只作用于发起页所属分组内的应用，不碰其他分组的配置与 current 标记。
    /// 该分组从未拍过快照时不改动任何配置，仅标记 current 并返回提示
    /// （下次从该项目切走时，自动保存会补拍该侧快照）。
    ///
    /// **切换前会自动保存旧项目或“无项目”状态**：若当前分组已绑定到另一个项目，
    /// 先把当前状态写入那个旧项目；若处于“不使用项目”，则写入独立 no-profile
    /// 快照，避免无项目期间的 provider/failover 状态污染目标项目。
    ///
    /// 应用指定项目的快照到当前分组内的所有应用。
    ///
    /// 返回 `(warnings, should_stop_proxy)`：当当前分组内所有接管都被关闭、且
    /// 其它应用也没有接管时，建议调用者停止代理服务。
    pub fn apply(
        state: &AppState,
        profile_id: &str,
        scope: ProfileScope,
    ) -> Result<(Vec<String>, bool), AppError> {
        let mut warnings = Vec::new();

        // 自动保存旧项目当前状态（仅当前分组），失败不阻塞切换
        if let Some(current_id) = state.db.get_current_profile_id(scope.as_str())? {
            if current_id != profile_id {
                if let Err(e) = Self::update(state, &current_id, None, true, Some(scope)) {
                    warnings.push(format!(
                        "autosave profile '{current_id}' before switch failed: {e}"
                    ));
                }
            }
        } else if let Err(e) = Self::save_none_snapshot(state, scope) {
            log::warn!(
                "[Profile] autosave no-profile state before switch failed: scope='{}', error={e}",
                scope.as_str()
            );
        }

        let profile = state
            .db
            .get_profile(profile_id)?
            .ok_or_else(|| AppError::InvalidInput(format!("Profile not found: {profile_id}")))?;
        let payload: ProfilePayload = serde_json::from_str(&profile.payload)
            .map_err(|e| AppError::Config(format!("解析 profile payload 失败: {e}")))?;

        let (apply_warnings, should_stop_proxy) =
            apply_payload(state, &payload, scope, profile_id)?;
        warnings.extend(apply_warnings);

        state
            .db
            .set_current_profile_id(scope.as_str(), Some(profile_id))?;

        Ok((warnings, should_stop_proxy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_payload_serde_roundtrip() {
        let payload = ProfilePayload {
            providers: PerApp {
                claude: Some("p1".into()),
                claude_desktop: Some("d1".into()),
                codex: None,
            },
            failover: PerApp {
                claude: Some(ids(&["p1", "p2"])),
                claude_desktop: Some(vec![]),
                codex: None,
            },
            mcp: PerApp {
                claude: Some(ids(&["m1", "m2"])),
                claude_desktop: Some(vec![]),
                codex: None,
            },
            skills: PerApp {
                claude: Some(vec![]),
                claude_desktop: Some(vec![]),
                codex: Some(ids(&["s1"])),
            },
            prompts: PerApp {
                claude: None,
                claude_desktop: None,
                codex: Some("pr1".into()),
            },
        };
        let json = serde_json::to_string(&payload).unwrap();
        // per-app key 必须与 AppType 的 serde 形式一致（claude-desktop 是连字符）
        assert!(json.contains("\"claude\""));
        assert!(json.contains("\"claude-desktop\""));
        assert!(json.contains("\"codex\""));
        let back: ProfilePayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back, payload);
    }

    #[test]
    fn test_payload_tolerates_missing_fields() {
        // 前向兼容：旧版/部分字段缺失时应落到 None（"该侧未拍过"）而不是报错，
        // 应用时对缺失槽位不做任何改动
        let back: ProfilePayload =
            serde_json::from_str(r#"{"providers":{"claude":"p1"},"mcp":{"claude":["m1"]}}"#)
                .unwrap();
        assert_eq!(back.providers.claude, Some("p1".to_string()));
        assert_eq!(back.providers.claude_desktop, None);
        assert_eq!(back.providers.codex, None);
        assert_eq!(
            back.failover.claude, None,
            "old snapshot leaves failover untouched"
        );
        assert_eq!(back.failover.claude_desktop, None);
        assert_eq!(back.failover.codex, None);
        assert_eq!(back.mcp.claude, Some(ids(&["m1"])));
        assert_eq!(back.mcp.claude_desktop, None);
        assert_eq!(back.mcp.codex, None, "missing slot means untouched");
        assert_eq!(back.prompts.codex, None);

        let empty: ProfilePayload = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, ProfilePayload::default());
    }

    #[test]
    fn test_payload_tolerates_object_slots_from_legacy_snapshots() {
        // 回归 Owner 反馈：历史/错误快照可能把完整对象写进本应为字符串 id 的槽位，
        // 不能再因 `invalid type: map, expected a string` 导致项目应用整体失败。
        let back: ProfilePayload = serde_json::from_str(
            r#"{
                "providers": {
                    "claude": { "id": "p1", "name": "Provider 1" },
                    "codex": { "providerId": "c1" }
                },
                "failover": {
                    "claude": [
                        { "providerId": "p2", "providerName": "Provider 2" },
                        "p1",
                        { "id": "p3" }
                    ],
                    "codex": { "queue": [{ "providerId": "c2" }] }
                },
                "prompts": {
                    "claude": { "promptId": "pr1", "name": "Prompt 1" }
                }
            }"#,
        )
        .expect("legacy object slots should deserialize");

        assert_eq!(back.providers.claude, Some("p1".to_string()));
        assert_eq!(back.providers.codex, Some("c1".to_string()));
        assert_eq!(back.failover.claude, Some(ids(&["p2", "p1", "p3"])));
        assert_eq!(back.failover.codex, Some(ids(&["c2"])));
        assert_eq!(back.prompts.claude, Some("pr1".to_string()));
    }

    #[test]
    fn test_merge_scope_from_only_touches_scope_slots() {
        // 项目 A：两侧都已拍过快照
        let mut payload = ProfilePayload {
            providers: PerApp {
                claude: Some("p1".into()),
                claude_desktop: Some("d1".into()),
                codex: Some("c1".into()),
            },
            failover: PerApp {
                claude: Some(ids(&["p1", "p3"])),
                claude_desktop: Some(ids(&["d1"])),
                codex: Some(ids(&["c1"])),
            },
            mcp: PerApp {
                claude: Some(ids(&["m1"])),
                claude_desktop: Some(vec![]),
                codex: Some(ids(&["m9"])),
            },
            ..Default::default()
        };
        // 在 Claude 页"以当前状态更新"：只覆盖 claude 组槽位
        let fresh = ProfilePayload {
            providers: PerApp {
                claude: Some("p2".into()),
                claude_desktop: None,
                codex: Some("SHOULD-NOT-LEAK".into()),
            },
            failover: PerApp {
                claude: Some(ids(&["p2", "p1"])),
                claude_desktop: Some(ids(&["SHOULD-NOT-LEAK"])),
                codex: None,
            },
            mcp: PerApp {
                claude: Some(ids(&["m2"])),
                claude_desktop: Some(vec![]),
                codex: None,
            },
            ..Default::default()
        };
        payload.merge_scope_from(&fresh, ProfileScope::Claude);

        assert_eq!(payload.providers.claude, Some("p2".to_string()));
        assert_eq!(payload.failover.claude, Some(ids(&["p2", "p1"])));
        assert_eq!(
            payload.providers.claude_desktop,
            Some("d1".to_string()),
            "claude-desktop slot is in its own scope, untouched by claude merge"
        );
        assert_eq!(payload.mcp.claude, Some(ids(&["m2"])));
        // codex 侧完好：既没被覆盖也没被 fresh 的值污染
        assert_eq!(payload.providers.codex, Some("c1".to_string()));
        assert_eq!(payload.failover.codex, Some(ids(&["c1"])));
        assert_eq!(payload.mcp.codex, Some(ids(&["m9"])));
    }

    #[test]
    fn test_scope_captured_detects_per_scope_snapshot() {
        let mut payload = ProfilePayload::default();
        assert!(!payload.scope_captured(ProfileScope::Claude));
        assert!(!payload.scope_captured(ProfileScope::ClaudeDesktop));
        assert!(!payload.scope_captured(ProfileScope::Codex));

        // 只拍过 claude 组（哪怕拍到的是空集）
        payload.mcp.claude = Some(vec![]);
        assert!(payload.scope_captured(ProfileScope::Claude));
        assert!(!payload.scope_captured(ProfileScope::ClaudeDesktop));
        assert!(!payload.scope_captured(ProfileScope::Codex));

        // Desktop 槽位属于独立的 claude-desktop 组
        let mut desktop_only = ProfilePayload::default();
        desktop_only.providers.claude_desktop = Some("d1".into());
        assert!(desktop_only.scope_captured(ProfileScope::ClaudeDesktop));
        assert!(!desktop_only.scope_captured(ProfileScope::Claude));

        let mut failover_only = ProfilePayload::default();
        failover_only.failover.codex = Some(vec![]);
        assert!(
            failover_only.scope_captured(ProfileScope::Codex),
            "an empty captured failover queue is still a deliberate project snapshot"
        );
    }

    #[test]
    fn test_per_app_get_only_supports_profile_apps() {
        let per: PerApp<Option<String>> = PerApp::default();
        assert!(per.get(&AppType::Claude).is_some());
        assert!(per.get(&AppType::ClaudeDesktop).is_some());
        assert!(per.get(&AppType::Codex).is_some());
        assert!(per.get(&AppType::Gemini).is_none());
    }

    #[test]
    fn test_scope_serde_and_parse_roundtrip() {
        for scope in ProfileScope::ALL {
            // DB 存储字符串（as_str/parse）与 JSON 序列化必须是同一形式
            assert_eq!(
                serde_json::to_string(&scope).unwrap(),
                format!("\"{}\"", scope.as_str())
            );
            assert_eq!(ProfileScope::parse(scope.as_str()).unwrap(), scope);
        }
        assert!(ProfileScope::parse("gemini").is_err());
        assert!(ProfileScope::parse("").is_err());
    }

    #[test]
    fn test_scope_app_grouping() {
        // Claude Code 与 Claude Desktop 各自独立成组；
        // 组内应用与 for_app 反向映射必须一致
        assert_eq!(ProfileScope::Claude.apps(), &[AppType::Claude]);
        assert_eq!(
            ProfileScope::ClaudeDesktop.apps(),
            &[AppType::ClaudeDesktop]
        );
        assert_eq!(ProfileScope::Codex.apps(), &[AppType::Codex]);
        for scope in ProfileScope::ALL {
            for app in scope.apps() {
                assert_eq!(ProfileScope::for_app(app), Some(scope));
            }
        }
        assert_eq!(ProfileScope::for_app(&AppType::Gemini), None);
    }

    #[test]
    fn test_plan_toggles_minimal_diff() {
        let current = vec![
            ("a".to_string(), true),  // 目标含 a：不动
            ("b".to_string(), false), // 目标含 b：开
            ("c".to_string(), true),  // 目标不含 c：关
            ("d".to_string(), false), // 目标不含 d：不动
        ];
        let (toggles, dangling) = plan_toggles(&current, &ids(&["a", "b", "ghost"]));
        assert_eq!(
            toggles,
            vec![("b".to_string(), true), ("c".to_string(), false)]
        );
        assert_eq!(dangling, ids(&["ghost"]));
    }

    #[test]
    fn test_plan_toggles_empty_target_disables_all_enabled() {
        let current = vec![("a".to_string(), true), ("b".to_string(), false)];
        let (toggles, dangling) = plan_toggles(&current, &[]);
        assert_eq!(toggles, vec![("a".to_string(), false)]);
        assert!(dangling.is_empty());
    }

    #[test]
    fn test_plan_failover_restore_preserves_priority_and_drops_missing() {
        let existing = HashSet::from(["p1", "p2", "p3"]);
        let current = ids(&["p3", "p1"]);
        let target = ids(&["p2", "ghost", "p1", "p2"]);

        let (restored, dangling, changed) = plan_failover_restore(&existing, &current, &target);

        assert_eq!(restored, ids(&["p2", "p1"]));
        assert_eq!(dangling, ids(&["ghost"]));
        assert!(changed, "priority order changed and must be written back");
    }

    #[test]
    fn test_plan_failover_restore_is_idempotent_when_order_matches() {
        let existing = HashSet::from(["p1", "p2"]);
        let current = ids(&["p1", "p2"]);
        let target = ids(&["p1", "p2"]);

        let (restored, dangling, changed) = plan_failover_restore(&existing, &current, &target);

        assert_eq!(restored, current);
        assert!(dangling.is_empty());
        assert!(
            !changed,
            "repeat profile apply should not rewrite unchanged failover queue"
        );
    }
}
