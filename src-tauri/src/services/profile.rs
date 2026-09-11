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

use serde::{Deserialize, Serialize};

use crate::app_config::AppType;
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

/// 项目内保存的故障转移路由项。
///
/// 故障转移实际按 providers.sort_index 排序；只保存 provider id 会导致
/// 项目内调整的 P1/P2 优先级泄漏到其它项目/未使用项目，所以快照必须
/// 同时保存排序值。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailoverProfileItem {
    pub provider_id: String,
    pub sort_index: Option<usize>,
    #[serde(skip)]
    legacy_order_only: bool,
}

impl<'de> Deserialize<'de> for FailoverProfileItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Item {
            provider_id: String,
            sort_index: Option<usize>,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum ItemOrLegacyId {
            Item(Item),
            LegacyId(String),
        }

        match ItemOrLegacyId::deserialize(deserializer)? {
            ItemOrLegacyId::Item(item) => Ok(Self {
                provider_id: item.provider_id,
                sort_index: item.sort_index,
                legacy_order_only: false,
            }),
            ItemOrLegacyId::LegacyId(provider_id) => Ok(Self {
                provider_id,
                sort_index: None,
                legacy_order_only: true,
            }),
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
    pub providers: PerApp<Option<String>>,
    /// 每 app 启用的 MCP server id 集合
    pub mcp: PerApp<Option<Vec<String>>>,
    /// 每 app 启用的 Skill id 集合
    pub skills: PerApp<Option<Vec<String>>>,
    /// 每 app 激活的 prompt id
    pub prompts: PerApp<Option<String>>,
    /// 每 app 的故障转移队列成员与路由优先级。
    ///
    /// 队列本体存在 providers.in_failover_queue 全局布尔位上，路由优先级
    /// 复用 providers.sort_index；二者默认都不随项目切换，会把上一项目的
    /// P1/P2 路由带进新项目。这里随项目快照保存成员关系和排序值，apply
    /// 时一起恢复。
    pub failover: PerApp<Option<Vec<FailoverProfileItem>>>,
    /// 每 app 的故障转移路由开关（proxy_config.auto_failover_enabled）。
    ///
    /// 该开关与队列成员共同决定路由行为；若不随项目/未使用项目保存，
    /// GLM 项目里打开的路由会泄漏到“未使用项目”，切回项目又会被
    /// 切换流程关闭接管时丢失。只恢复 auto_failover_enabled，不恢复
    /// proxy enabled 接管态，保持“切项目必退出接管”的既有约束。
    #[serde(rename = "autoFailover")]
    pub auto_failover: PerApp<Option<bool>>,
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
            if let (Some(dst), Some(src)) = (self.mcp.get_mut(app), other.mcp.get(app)) {
                *dst = src.clone();
            }
            if let (Some(dst), Some(src)) = (self.skills.get_mut(app), other.skills.get(app)) {
                *dst = src.clone();
            }
            if let (Some(dst), Some(src)) = (self.prompts.get_mut(app), other.prompts.get(app)) {
                *dst = src.clone();
            }
            if let (Some(dst), Some(src)) = (self.failover.get_mut(app), other.failover.get(app)) {
                *dst = src.clone();
            }
            if let (Some(dst), Some(src)) = (
                self.auto_failover.get_mut(app),
                other.auto_failover.get(app),
            ) {
                *dst = *src;
            }
        }
    }

    /// 某分组是否拍过快照（任一槽位非 None 即视为拍过）
    pub fn scope_captured(&self, scope: ProfileScope) -> bool {
        scope.apps().iter().any(|app| {
            self.providers.get(app).is_some_and(|s| s.is_some())
                || self.mcp.get(app).is_some_and(|s| s.is_some())
                || self.skills.get(app).is_some_and(|s| s.is_some())
                || self.prompts.get(app).is_some_and(|s| s.is_some())
                || self.failover.get(app).is_some_and(|s| s.is_some())
                || self.auto_failover.get(app).is_some_and(|s| s.is_some())
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

/// 计算故障转移队列成员的最小增删集：返回 (待加入, 待移除)
///
/// 队列持久化是 providers.in_failover_queue 全局布尔位；路由优先级
/// 由 providers.sort_index 派生，需在恢复时另行按项目快照写回，避免
/// 只隔离成员而遗漏 P1/P2 路由顺序。
fn plan_failover_membership(
    current_ids: &[String],
    target_ids: &HashSet<String>,
) -> (Vec<String>, Vec<String>) {
    let current: HashSet<&str> = current_ids.iter().map(String::as_str).collect();
    let mut add: Vec<String> = target_ids
        .iter()
        .filter(|id| !current.contains(id.as_str()))
        .cloned()
        .collect();
    add.sort();
    let remove: Vec<String> = current_ids
        .iter()
        .filter(|id| !target_ids.contains(id.as_str()))
        .cloned()
        .collect();
    (add, remove)
}

fn infer_missing_failover_sort_index(failover: &mut PerApp<Option<Vec<FailoverProfileItem>>>) {
    for slot in [
        &mut failover.claude,
        &mut failover.claude_desktop,
        &mut failover.codex,
    ] {
        if let Some(items) = slot {
            for (index, item) in items.iter_mut().enumerate() {
                // 兼容旧版 payload/no-profile 设置：当时只保存按路由优先级
                // 排好序的 provider id。读取后把数组位置补成 sort_index，避免
                // 升级后切换“不使用项目”直接解析失败，且尽量保留旧顺序。
                if item.legacy_order_only {
                    item.sort_index = Some(index);
                    item.legacy_order_only = false;
                }
            }
        }
    }
}

fn parse_profile_payload(value: &str) -> Result<ProfilePayload, AppError> {
    let mut payload: ProfilePayload = serde_json::from_str(value)
        .map_err(|e| AppError::Config(format!("解析 profile payload 失败: {e}")))?;
    infer_missing_failover_sort_index(&mut payload.failover);
    Ok(payload)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct NoProfileFailoverPayload {
    failover: PerApp<Option<Vec<FailoverProfileItem>>>,
    #[serde(rename = "autoFailover")]
    auto_failover: PerApp<Option<bool>>,
}

fn parse_failover_payload(value: &str) -> Result<NoProfileFailoverPayload, AppError> {
    let raw: serde_json::Value = serde_json::from_str(value)
        .map_err(|e| AppError::Config(format!("解析 no-profile failover payload 失败: {e}")))?;
    let is_wrapper = raw
        .as_object()
        .is_some_and(|obj| obj.contains_key("failover") || obj.contains_key("autoFailover"));

    let mut payload = if is_wrapper {
        serde_json::from_value(raw)
            .map_err(|e| AppError::Config(format!("解析 no-profile failover payload 失败: {e}")))?
    } else {
        // 兼容已落盘的旧 no-profile 值：顶层就是 PerApp<failover>，不能让
        // 新 wrapper 的 serde(default) 吞掉 claude/codex 键而误判为空快照。
        let failover: PerApp<Option<Vec<FailoverProfileItem>>> = serde_json::from_value(raw)
            .map_err(|e| AppError::Config(format!("解析 no-profile failover payload 失败: {e}")))?;
        NoProfileFailoverPayload {
            failover,
            auto_failover: PerApp::default(),
        }
    };
    infer_missing_failover_sort_index(&mut payload.failover);
    Ok(payload)
}

fn no_profile_failover_key(scope: ProfileScope) -> String {
    format!("no_profile_failover_{}", scope.as_str())
}

pub struct ProfileService;

impl ProfileService {
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
            if let Some(slot) = payload.failover.get_mut(app) {
                // 只有带本地代理数据面的应用才有故障转移队列；Claude Desktop
                // 等无代理应用的槽位保持 None（"不适用"），apply 时不动它。
                if app.supports_local_proxy() {
                    *slot = Some(
                        state
                            .db
                            .get_failover_queue(app.as_str())?
                            .into_iter()
                            .map(|item| FailoverProfileItem {
                                provider_id: item.provider_id,
                                sort_index: item.sort_index,
                                legacy_order_only: false,
                            })
                            .collect(),
                    );
                }
            }
            if let Some(slot) = payload.auto_failover.get_mut(app) {
                if app.supports_local_proxy() {
                    let (_, auto_failover_enabled) = state.db.get_proxy_flags_sync(app.as_str());
                    *slot = Some(auto_failover_enabled);
                }
            }
        }
        Ok(payload)
    }

    /// 列出所有项目（项目实体全应用共享，current 标记按分组单独读取）
    pub fn list(state: &AppState) -> Result<Vec<Profile>, AppError> {
        state.db.get_all_profiles()
    }

    fn snapshot_failover_current(
        state: &AppState,
        scope: ProfileScope,
    ) -> Result<NoProfileFailoverPayload, AppError> {
        let mut payload = NoProfileFailoverPayload::default();
        for app in scope.apps() {
            if app.supports_local_proxy() {
                if let Some(slot) = payload.failover.get_mut(app) {
                    *slot = Some(
                        state
                            .db
                            .get_failover_queue(app.as_str())?
                            .into_iter()
                            .map(|item| FailoverProfileItem {
                                provider_id: item.provider_id,
                                sort_index: item.sort_index,
                                legacy_order_only: false,
                            })
                            .collect(),
                    );
                }
                if let Some(slot) = payload.auto_failover.get_mut(app) {
                    let (_, auto_failover_enabled) = state.db.get_proxy_flags_sync(app.as_str());
                    *slot = Some(auto_failover_enabled);
                }
            }
        }
        Ok(payload)
    }

    fn save_no_profile_failover(state: &AppState, scope: ProfileScope) -> Result<(), AppError> {
        let payload = Self::snapshot_failover_current(state, scope)?;
        let value = serde_json::to_string(&payload).map_err(|e| {
            AppError::Config(format!("序列化 no-profile failover payload 失败: {e}"))
        })?;
        state
            .db
            .set_setting(&no_profile_failover_key(scope), &value)
    }

    fn restore_failover_for_app(
        state: &AppState,
        app: &AppType,
        target_items: &[FailoverProfileItem],
        warnings: &mut Vec<String>,
    ) -> Result<(), AppError> {
        let app_str = app.as_str();
        let mut providers = state.db.get_all_providers(app_str)?;
        let mut target_set: HashSet<String> = HashSet::new();
        for item in target_items {
            let id = &item.provider_id;
            match providers.get(id) {
                None => warnings.push(format!(
                    "[{app_str}] failover provider '{id}' no longer exists, skipped"
                )),
                // Codex Official 账号卡禁止参与故障转移（请求携带所选账号的
                // Authorization 头，重放到其他账号会越界）；旧快照里即使
                // 混入也不恢复。
                Some(p)
                    if !crate::proxy::provider_router::provider_supports_failover(app_str, p) =>
                {
                    warnings.push(format!(
                        "[{app_str}] failover provider '{id}' does not support failover, skipped"
                    ));
                }
                Some(_) => {
                    target_set.insert(id.clone());
                }
            }
        }

        // 故障转移路由顺序由 provider.sort_index 决定；恢复队列成员但不恢复
        // sort_index 会让项目 A 的 P1/P2 排序污染项目 B 或“未使用项目”。
        for item in target_items {
            if !target_set.contains(&item.provider_id) {
                continue;
            }
            if let Some(provider) = providers.get_mut(&item.provider_id) {
                if provider.sort_index != item.sort_index {
                    provider.sort_index = item.sort_index;
                    if let Err(e) = state.db.save_provider(app_str, provider) {
                        warnings.push(format!(
                            "[{app_str}] restore failover provider '{id}' route order failed: {e}",
                            id = item.provider_id
                        ));
                    }
                }
            }
        }

        let current_ids: Vec<String> = state
            .db
            .get_failover_queue(app_str)?
            .into_iter()
            .map(|item| item.provider_id)
            .collect();
        let (to_add, to_remove) = plan_failover_membership(&current_ids, &target_set);
        for id in to_add {
            if let Err(e) = state.db.add_to_failover_queue(app_str, &id) {
                warnings.push(format!(
                    "[{app_str}] add failover provider '{id}' failed: {e}"
                ));
            }
        }
        for id in to_remove {
            if let Err(e) = state.db.remove_from_failover_queue(app_str, &id) {
                warnings.push(format!(
                    "[{app_str}] remove failover provider '{id}' failed: {e}"
                ));
            }
        }
        Ok(())
    }

    fn restore_failover_slots(
        state: &AppState,
        failover: &PerApp<Option<Vec<FailoverProfileItem>>>,
        scope: ProfileScope,
        warnings: &mut Vec<String>,
    ) -> Result<(), AppError> {
        for app in scope.apps() {
            if let Some(Some(target_ids)) = failover.get(app) {
                Self::restore_failover_for_app(state, app, target_ids, warnings)?;
            }
        }
        Ok(())
    }

    fn restore_auto_failover_for_app(
        state: &AppState,
        app: &AppType,
        target_auto_failover: bool,
        warnings: &mut Vec<String>,
    ) {
        let app_str = app.as_str();
        let (enabled, current_auto_failover) = state.db.get_proxy_flags_sync(app_str);
        if current_auto_failover == target_auto_failover {
            return;
        }
        // 项目切换会先关闭 proxy enabled（接管态），但用户看到的“路由开关”
        // 是 auto_failover_enabled；只恢复这个开关，防止 GLM 项目与“未使用项目”
        // 之间互相泄漏，同时不重新打开已退出的代理接管。
        if let Err(e) = state
            .db
            .set_proxy_flags_sync(app_str, enabled, target_auto_failover)
        {
            warnings.push(format!(
                "[{app_str}] restore auto failover switch -> {target_auto_failover} failed: {e}"
            ));
        }
    }

    fn restore_auto_failover_slots(
        state: &AppState,
        auto_failover: &PerApp<Option<bool>>,
        scope: ProfileScope,
        warnings: &mut Vec<String>,
    ) {
        for app in scope.apps() {
            if let Some(Some(target_auto_failover)) = auto_failover.get(app) {
                Self::restore_auto_failover_for_app(state, app, *target_auto_failover, warnings);
            }
        }
    }

    /// 离开项目时恢复“未使用项目”的故障转移队列，避免项目内新增的
    /// failover provider 泄漏到全局未绑定状态。
    pub fn clear_current(state: &AppState, scope: ProfileScope) -> Result<Vec<String>, AppError> {
        let mut warnings = Vec::new();
        if let Some(current_id) = state.db.get_current_profile_id(scope.as_str())? {
            if let Err(e) = Self::update(state, &current_id, None, true, Some(scope)) {
                warnings.push(format!(
                    "autosave profile '{current_id}' before clearing project failed: {e}"
                ));
            }
        }

        let saved = state.db.get_setting(&no_profile_failover_key(scope))?;
        let payload = match saved {
            Some(value) => parse_failover_payload(&value)?,
            None => NoProfileFailoverPayload::default(),
        };
        Self::restore_failover_slots(state, &payload.failover, scope, &mut warnings)?;
        Self::restore_auto_failover_slots(state, &payload.auto_failover, scope, &mut warnings);
        state.db.set_current_profile_id(scope.as_str(), None)?;
        Ok(warnings)
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
            let mut payload = parse_profile_payload(&profile.payload)?;
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

    /// 应用项目快照（best-effort，返回 warnings）
    ///
    /// 只作用于发起页所属分组内的应用，不碰其他分组的配置与 current 标记。
    /// 该分组从未拍过快照时不改动任何配置，仅标记 current 并返回提示
    /// （下次从该项目切走时，自动保存会补拍该侧快照）。
    ///
    /// **切换前会自动保存旧项目**：若当前分组已绑定到另一个项目，先把当前
    /// 状态写入那个旧项目（仅当前分组槽位），再加载目标项目。这样切走后
    /// 旧项目仍保留离开时的配置，回来时状态一致。自动保存失败时作为 warning
    /// 继续，不阻塞切换。
    ///
    /// 应用指定项目的快照到当前分组内的所有应用。
    ///
    /// 返回 `(warnings, should_stop_proxy)`：当当前分组内所有接管都被关闭、且
    /// 其它应用也没有接管时，建议调用者停止代理服务，以便 Claude Desktop 的
    /// "本地路由"总开关同步显示为关闭。
    pub fn apply(
        state: &AppState,
        profile_id: &str,
        scope: ProfileScope,
    ) -> Result<(Vec<String>, bool), AppError> {
        let mut warnings = Vec::new();

        // 自动保存旧项目当前状态（仅当前分组），失败不阻塞切换；
        // 若当前处于“未使用项目”，也保存它自己的故障转移队列，否则进入
        // 项目后添加的 failover provider 会在回到“未使用项目”时继续残留。
        if let Some(current_id) = state.db.get_current_profile_id(scope.as_str())? {
            if current_id != profile_id {
                if let Err(e) = Self::update(state, &current_id, None, true, Some(scope)) {
                    warnings.push(format!(
                        "autosave profile '{current_id}' before switch failed: {e}"
                    ));
                }
            }
        } else if let Err(e) = Self::save_no_profile_failover(state, scope) {
            warnings.push(format!(
                "autosave no-profile failover before switch failed: {e}"
            ));
        }

        let profile = state
            .db
            .get_profile(profile_id)?
            .ok_or_else(|| AppError::InvalidInput(format!("Profile not found: {profile_id}")))?;
        let payload = parse_profile_payload(&profile.payload)?;

        if !payload.scope_captured(scope) {
            warnings.push(format!(
                "no {} configuration captured in this project yet; marked as current without changes (it will be saved automatically when you switch away)",
                scope.as_str()
            ));
        }

        for app in scope.apps().iter() {
            let app_str = app.as_str();

            // 1. 切换项目前无条件关闭当前应用的代理接管。
            // 接管态下 live 文件属于代理；用户希望切换工作目录时总是退出当前
            // 代理环境，再按快照写入真实供应商配置。
            if let Err(e) = state.proxy_service.disable_takeover_for_app_sync(app) {
                warnings.push(format!(
                    "[{app_str}] auto-disable proxy takeover before profile switch failed: {e}"
                ));
            }

            // 2. 供应商
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

            // 3. 故障转移队列成员（None = 该侧未拍过或该应用不支持代理，不动）
            if let Some(Some(target_ids)) = payload.failover.get(app) {
                Self::restore_failover_for_app(state, app, target_ids, &mut warnings)?;
            }
            if let Some(Some(target_auto_failover)) = payload.auto_failover.get(app) {
                Self::restore_auto_failover_for_app(
                    state,
                    app,
                    *target_auto_failover,
                    &mut warnings,
                );
            }

            // 4. MCP diff（最小 toggle：仅动目标态≠当前态的条目；None = 该侧未拍过，不动）
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
                        if let Err(e) =
                            PromptService::enable_prompt(state, app.clone(), target_prompt)
                        {
                            warnings.push(format!(
                                "[{app_str}] enable prompt '{target_prompt}' failed: {e}"
                            ));
                        }
                    }
                }
            }
        }

        state
            .db
            .set_current_profile_id(scope.as_str(), Some(profile_id))?;

        // 当前分组内所有接管已关闭；若其它应用也无接管，可停止代理服务。
        let should_stop_proxy = !state.db.is_live_takeover_active_sync();

        Ok((warnings, should_stop_proxy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn failover_items(v: &[(&str, Option<usize>)]) -> Vec<FailoverProfileItem> {
        v.iter()
            .map(|(provider_id, sort_index)| FailoverProfileItem {
                provider_id: provider_id.to_string(),
                sort_index: *sort_index,
                legacy_order_only: false,
            })
            .collect()
    }

    fn state_with_providers(providers: &[(&str, usize, bool)]) -> Result<AppState, AppError> {
        let state = AppState::new(std::sync::Arc::new(crate::database::Database::memory()?));
        for (id, sort_index, in_failover_queue) in providers {
            let mut provider = crate::provider::Provider::with_id(
                (*id).to_string(),
                (*id).to_string(),
                serde_json::json!({
                    "env": {
                        "ANTHROPIC_AUTH_TOKEN": format!("token-{id}"),
                        "ANTHROPIC_BASE_URL": "https://example.test"
                    }
                }),
                None,
            );
            provider.sort_index = Some(*sort_index);
            provider.in_failover_queue = *in_failover_queue;
            state
                .db
                .save_provider(AppType::Claude.as_str(), &provider)?;
        }
        Ok(state)
    }

    fn failover_ids_and_order(state: &AppState) -> Result<Vec<(String, Option<usize>)>, AppError> {
        Ok(state
            .db
            .get_failover_queue(AppType::Claude.as_str())?
            .into_iter()
            .map(|item| (item.provider_id, item.sort_index))
            .collect())
    }

    #[test]
    fn test_payload_serde_roundtrip() {
        let payload = ProfilePayload {
            providers: PerApp {
                claude: Some("p1".into()),
                claude_desktop: Some("d1".into()),
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
            failover: PerApp {
                claude: Some(failover_items(&[("p1", Some(0)), ("p2", Some(1))])),
                claude_desktop: None,
                codex: Some(failover_items(&[("c1", Some(3))])),
            },
            auto_failover: PerApp {
                claude: Some(true),
                claude_desktop: None,
                codex: Some(false),
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
    fn test_legacy_failover_provider_id_payload_is_accepted() {
        let mut payload = parse_profile_payload(
            r#"{
                "providers":{},
                "failover":{
                    "claude":["p1","p2"],
                    "codex":[{"providerId":"c1","sortIndex":7}]
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            payload.failover.claude.take(),
            Some(failover_items(&[("p1", Some(0)), ("p2", Some(1))])),
            "legacy project snapshots stored only ordered provider ids; upgrade must keep that order"
        );
        assert_eq!(
            payload.failover.codex.take(),
            Some(failover_items(&[("c1", Some(7))])),
            "new failover items must keep their explicit route order"
        );
    }

    #[test]
    fn test_legacy_no_profile_failover_provider_id_payload_is_accepted() {
        let payload = parse_failover_payload(
            r#"{
                "claude":["p1","p2"],
                "claude-desktop":null,
                "codex":[{"providerId":"c1","sortIndex":3}]
            }"#,
        )
        .unwrap();

        assert_eq!(
            payload.failover.claude,
            Some(failover_items(&[("p1", Some(0)), ("p2", Some(1))])),
            "switching back to no-profile must not fail on the previous string-id payload format"
        );
        assert_eq!(
            payload.failover.codex,
            Some(failover_items(&[("c1", Some(3))]))
        );
        assert_eq!(
            payload.auto_failover.codex, None,
            "legacy no-profile baseline did not capture the route switch"
        );
    }

    #[test]
    fn test_no_profile_failover_wrapper_payload_is_accepted() {
        let payload = parse_failover_payload(
            r#"{
                "failover": {
                    "claude": [{"providerId":"p1","sortIndex":4}],
                    "claude-desktop": null,
                    "codex": []
                },
                "autoFailover": {
                    "claude": true,
                    "claude-desktop": null,
                    "codex": false
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            payload.failover.claude,
            Some(failover_items(&[("p1", Some(4))]))
        );
        assert_eq!(payload.auto_failover.claude, Some(true));
        assert_eq!(payload.auto_failover.codex, Some(false));
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
        assert_eq!(back.mcp.claude, Some(ids(&["m1"])));
        assert_eq!(back.mcp.claude_desktop, None);
        assert_eq!(back.mcp.codex, None, "missing slot means untouched");
        assert_eq!(back.prompts.codex, None);
        assert_eq!(
            back.failover.codex, None,
            "old profiles did not capture failover"
        );
        assert_eq!(
            back.auto_failover.codex, None,
            "old profiles did not capture the route switch"
        );

        let empty: ProfilePayload = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, ProfilePayload::default());
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
            mcp: PerApp {
                claude: Some(ids(&["m1"])),
                claude_desktop: Some(vec![]),
                codex: Some(ids(&["m9"])),
            },
            failover: PerApp {
                claude: Some(failover_items(&[("f1", Some(1))])),
                claude_desktop: None,
                codex: Some(failover_items(&[("cf1", Some(2))])),
            },
            auto_failover: PerApp {
                claude: Some(true),
                claude_desktop: None,
                codex: Some(false),
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
            mcp: PerApp {
                claude: Some(ids(&["m2"])),
                claude_desktop: Some(vec![]),
                codex: None,
            },
            failover: PerApp {
                claude: Some(failover_items(&[("f2", Some(0)), ("f3", Some(4))])),
                claude_desktop: None,
                codex: Some(failover_items(&[("SHOULD-NOT-LEAK", Some(9))])),
            },
            auto_failover: PerApp {
                claude: Some(false),
                claude_desktop: Some(true),
                codex: Some(true),
            },
            ..Default::default()
        };
        payload.merge_scope_from(&fresh, ProfileScope::Claude);

        assert_eq!(payload.providers.claude, Some("p2".to_string()));
        assert_eq!(
            payload.providers.claude_desktop,
            Some("d1".to_string()),
            "claude-desktop slot is in its own scope, untouched by claude merge"
        );
        assert_eq!(payload.mcp.claude, Some(ids(&["m2"])));
        assert_eq!(
            payload.failover.claude,
            Some(failover_items(&[("f2", Some(0)), ("f3", Some(4))]))
        );
        assert_eq!(
            payload.auto_failover.claude,
            Some(false),
            "route switch is part of the scoped failover snapshot and must update with it"
        );
        // codex 侧完好：既没被覆盖也没被 fresh 的值污染
        assert_eq!(payload.providers.codex, Some("c1".to_string()));
        assert_eq!(payload.mcp.codex, Some(ids(&["m9"])));
        assert_eq!(
            payload.failover.codex,
            Some(failover_items(&[("cf1", Some(2))]))
        );
        assert_eq!(payload.auto_failover.codex, Some(false));
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

        let mut codex_failover_only = ProfilePayload::default();
        codex_failover_only.failover.codex = Some(vec![]);
        assert!(
            codex_failover_only.scope_captured(ProfileScope::Codex),
            "an intentionally empty failover queue is still a captured project setting"
        );
        assert!(!codex_failover_only.scope_captured(ProfileScope::Claude));

        let mut codex_route_switch_only = ProfilePayload::default();
        codex_route_switch_only.auto_failover.codex = Some(false);
        assert!(
            codex_route_switch_only.scope_captured(ProfileScope::Codex),
            "an explicitly off route switch is still a captured project setting"
        );

        // Desktop 槽位属于独立的 claude-desktop 组
        let mut desktop_only = ProfilePayload::default();
        desktop_only.providers.claude_desktop = Some("d1".into());
        assert!(desktop_only.scope_captured(ProfileScope::ClaudeDesktop));
        assert!(!desktop_only.scope_captured(ProfileScope::Claude));
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
    fn test_plan_failover_membership_minimal_diff() {
        let current = ids(&["keep", "remove"]);
        let mut target = HashSet::new();
        target.insert("keep".to_string());
        target.insert("add".to_string());

        let (to_add, to_remove) = plan_failover_membership(&current, &target);
        assert_eq!(to_add, ids(&["add"]));
        assert_eq!(to_remove, ids(&["remove"]));
    }

    #[test]
    fn test_restore_failover_for_app_restores_membership_and_route_order() -> Result<(), AppError> {
        let state = state_with_providers(&[("a", 7, true), ("b", 2, false), ("c", 1, true)])?;
        let mut warnings = Vec::new();

        ProfileService::restore_failover_for_app(
            &state,
            &AppType::Claude,
            &failover_items(&[("b", Some(0)), ("a", Some(3))]),
            &mut warnings,
        )?;

        assert!(
            warnings.is_empty(),
            "unexpected restore warnings: {warnings:?}"
        );
        assert_eq!(
            failover_ids_and_order(&state)?,
            vec![("b".to_string(), Some(0)), ("a".to_string(), Some(3))],
            "project failover restore must reset both queue membership and route priority"
        );
        assert!(
            !state
                .db
                .get_provider_by_id("c", AppType::Claude.as_str())?
                .expect("provider c")
                .in_failover_queue,
            "providers only added inside another project must be removed from this project's queue"
        );

        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_no_profile_failover_roundtrip_restores_queue_and_route_order() -> Result<(), AppError> {
        let temp_home = tempfile::tempdir().expect("temp test home");
        let previous_home = std::env::var_os("CC_SWITCH_TEST_HOME");
        std::env::set_var("CC_SWITCH_TEST_HOME", temp_home.path());

        let result = (|| {
            let state = state_with_providers(&[("a", 0, true), ("b", 1, false), ("c", 2, false)])?;
            let (_, initial_auto) = state.db.get_proxy_flags_sync(AppType::Claude.as_str());
            assert!(!initial_auto, "initial auto_failover should be false");

            let profile_payload = ProfilePayload {
                failover: PerApp {
                    claude: Some(failover_items(&[("b", Some(0)), ("c", Some(1))])),
                    ..Default::default()
                },
                auto_failover: PerApp {
                    claude: Some(true),
                    ..Default::default()
                },
                ..Default::default()
            };
            state.db.save_profile(&Profile {
                id: "profile-1".to_string(),
                name: "Project".to_string(),
                payload: serde_json::to_string(&profile_payload).unwrap(),
                sort_order: None,
                created_at: Some(1),
                updated_at: Some(1),
            })?;

            let (apply_warnings, _should_stop_proxy) =
                ProfileService::apply(&state, "profile-1", ProfileScope::Claude)?;
            assert!(
                apply_warnings.is_empty(),
                "unexpected apply warnings: {apply_warnings:?}"
            );
            assert_eq!(
                failover_ids_and_order(&state)?,
                vec![("b".to_string(), Some(0)), ("c".to_string(), Some(1))],
                "using a project should replace the no-profile failover queue"
            );
            let (_, auto_after_apply) = state.db.get_proxy_flags_sync(AppType::Claude.as_str());
            assert!(
                auto_after_apply,
                "applying a project with auto_failover=true must turn on the route switch"
            );

            // 模拟用户在项目内调整故障转移：如果 clear_current 不恢复 no-profile
            // 快照，这些成员、P1/P2 排序和路由开关都会泄漏回“未使用项目”。
            let mut a = state
                .db
                .get_provider_by_id("a", AppType::Claude.as_str())?
                .expect("provider a");
            a.sort_index = Some(9);
            state.db.save_provider(AppType::Claude.as_str(), &a)?;
            let mut b = state
                .db
                .get_provider_by_id("b", AppType::Claude.as_str())?
                .expect("provider b");
            b.sort_index = Some(8);
            state.db.save_provider(AppType::Claude.as_str(), &b)?;
            state
                .db
                .add_to_failover_queue(AppType::Claude.as_str(), "a")?;

            let clear_warnings = ProfileService::clear_current(&state, ProfileScope::Claude)?;
            assert!(
                clear_warnings.is_empty(),
                "unexpected clear warnings: {clear_warnings:?}"
            );
            assert_eq!(
                state
                    .db
                    .get_current_profile_id(ProfileScope::Claude.as_str())?,
                None,
                "clearing the project should return to no-profile mode"
            );
            assert_eq!(
                failover_ids_and_order(&state)?,
                vec![("a".to_string(), Some(0))],
                "returning to no-profile must restore its own queue membership and route order"
            );
            assert!(
                !state
                    .db
                    .get_provider_by_id("b", AppType::Claude.as_str())?
                    .expect("provider b")
                    .in_failover_queue,
                "provider added by the project must not remain in the no-profile queue"
            );
            let (_, auto_after_clear) = state.db.get_proxy_flags_sync(AppType::Claude.as_str());
            assert!(
                !auto_after_clear,
                "returning to no-profile must restore the route switch to its original off state"
            );

            Ok(())
        })();

        match previous_home {
            Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
            None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
        }
        result
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
}
