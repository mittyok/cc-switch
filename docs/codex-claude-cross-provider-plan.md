# Codex 桌面使用 Claude Code 供应商 — 跨应用供应商同步方案

## Context

CC Switch 同时管理 Claude Code 和 Codex 两个 CLI 工具，但它们使用**完全不同的配置格式和 API 协议**：

| 维度 | Claude Code | Codex |
|------|------------|-------|
| **配置格式** | JSON (`settings.json`) | TOML (`config.toml`) + JSON (`auth.json`) |
| **API 协议** | Anthropic API | OpenAI Responses API |
| **认证方式** | `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` | `OPENAI_API_KEY` |
| **模型标识** | `claude-sonnet-4-6` 等 | `gpt-5.4` 等 |
| **Base URL** | `ANTHROPIC_BASE_URL`（env 内） | `base_url`（TOML section 内） |

当前用户遇到的痛点：

1. **重复配置** — 很多第三方 API 网关（如 AiHubMix、DMXAPI、OpenRouter）同时支持 Anthropic 和 OpenAI 协议，但用户需要在 Claude Code 和 Codex 中**分别手动添加**相同供应商
2. **Universal Provider 局限** — 现有统一供应商仅支持有限的预设（NewAPI、自定义网关），无法从已有的 Claude Code 供应商**一键派生** Codex 供应商
3. **同步困难** — 修改 API Key 或 Base URL 后，需要在两个应用中分别更新

**本方案**目标：实现 Claude Code 供应商到 Codex 供应商的**一键派生与双向同步**，让用户在一处配置、多处使用。

---

## 一、功能概述

### 1.1 核心能力

| 功能 | 描述 |
|------|------|
| **一键派生** | 从已有 Claude Code 供应商，自动生成对应的 Codex 供应商配置 |
| **配置转换** | 自动完成 JSON → TOML 格式转换、API Key 字段映射、Base URL 提取与重写 |
| **关联绑定** | 派生后的供应商通过 `linkedProviderId` 建立关联，修改一方自动同步另一方 |
| **智能映射** | 根据供应商的 `category` 和 `apiFormat` 自动推断 Codex 侧的 `wire_api` 和模型名称 |
| **批量操作** | 支持选择多个 Claude Code 供应商批量派生到 Codex |

### 1.2 不做什么

- **不做协议转换代理** — CC Switch 不在中间做 Anthropic ↔ OpenAI 请求转发（这是供应商/网关的职责）
- **不自动检测兼容性** — 用户需确认供应商同时支持两种协议（大多数第三方网关已支持）
- **不影响现有供应商** — 已有的独立 Claude/Codex 供应商不受影响

---

## 二、配置转换规则

### 2.1 字段映射

```text
Claude Code (settings.json)          →    Codex (auth.json + config.toml)
─────────────────────────────────────────────────────────────────────────
env.ANTHROPIC_AUTH_TOKEN             →    auth.OPENAI_API_KEY
env.ANTHROPIC_API_KEY                →    auth.OPENAI_API_KEY
env.ANTHROPIC_BASE_URL               →    [model_providers.X].base_url (需追加路径转换)
env.ANTHROPIC_MODEL                  →    model (需模型名称映射)
env.ANTHROPIC_DEFAULT_SONNET_MODEL   →    (忽略，Codex 无对应字段)
env.ANTHROPIC_DEFAULT_HAIKU_MODEL    →    (忽略)
env.ANTHROPIC_DEFAULT_OPUS_MODEL     →    (忽略)
```

### 2.2 Base URL 转换策略

不同类型的供应商，Base URL 映射方式不同：

| 供应商类型 | Claude Code Base URL | Codex Base URL | 转换逻辑 |
|-----------|---------------------|----------------|---------|
| **官方** | （空/默认） | （空/默认） | 不转换 |
| **第三方网关** | `https://api.example.com/anthropic` | `https://api.example.com/v1` | 路径替换 `/anthropic` → `/v1` |
| **聚合器** | `https://api.example.com/v1` | `https://api.example.com/v1` | 直接复用 |
| **云服务商** | `https://bedrock-runtime.*.amazonaws.com` | 不支持 | 标记为不可派生 |
| **已知预设** | 按预设查表 | 按预设查表 | 预设映射表 |

### 2.3 Base URL 路径转换规则

```text
输入 URL                                    →  输出 URL
──────────────────────────────────────────────────────────────
https://api.example.com/anthropic           →  https://api.example.com/v1
https://api.example.com/anthropic/v1        →  https://api.example.com/v1
https://api.example.com/v1                  →  https://api.example.com/v1  (保持不变)
https://api.example.com                     →  https://api.example.com/v1  (追加 /v1)
https://api.example.com/custom/path         →  https://api.example.com/custom/path  (保持不变，用户确认)
```

### 2.4 模型名称映射

| Claude Code 模型 | Codex 默认模型 |
|------------------|---------------|
| `claude-sonnet-4-6` | `gpt-5.4` |
| `claude-opus-4-7` | `gpt-5.4` |
| `claude-haiku-4-5-*` | `gpt-5.4` |
| 自定义模型 | 保留原始名称（用户可编辑） |

### 2.5 不可派生的供应商

以下类型的 Claude Code 供应商**不支持**一键派生到 Codex：

- `category === "official"` — Claude 官方（Anthropic 直连）
- `category === "cloud_provider"` 且使用 Bedrock/Vertex — 协议不兼容
- `providerType === "github_copilot"` — OAuth 绑定，无法复用
- `apiFormat === "gemini_native"` — Gemini 原生协议

---

## 三、数据模型变更

### 3.1 Provider 新增字段

```rust
// src-tauri/src/provider.rs — Provider 结构体新增

/// 关联的跨应用供应商 ID（用于双向同步）
#[serde(skip_serializing_if = "Option::is_none")]
pub linked_provider_id: Option<String>,

/// 关联的源应用类型（派生来源）
#[serde(skip_serializing_if = "Option::is_none")]
pub linked_source_app: Option<String>,
```

### 3.2 ProviderMeta 新增字段

```typescript
// src/types.ts — ProviderMeta 新增

/** 跨应用派生来源 */
crossAppSource?: {
  /** 源供应商 ID */
  sourceProviderId: string;
  /** 源应用类型 */
  sourceApp: AppId;
  /** 派生时间戳 */
  derivedAt: number;
};
```

### 3.3 数据库 Schema

无需新增表——关联信息存储在 Provider 的 `settings_config` JSON 中的 `meta` 字段内，随供应商一起持久化。

---

## 四、后端实现

### 4.1 配置转换模块 — `src-tauri/src/services/provider/cross_app.rs`（新增）

```rust
use crate::app_config::AppType;
use crate::provider::Provider;
use crate::error::AppError;
use serde_json::{json, Value};

/// 从 Claude Code 供应商派生 Codex 供应商
pub fn derive_codex_from_claude(
    claude_provider: &Provider,
) -> Result<Provider, AppError> {
    let settings = &claude_provider.settings_config;

    // 1. 提取 API Key
    let api_key = extract_api_key(settings)?;

    // 2. 提取并转换 Base URL
    let claude_base_url = extract_claude_base_url(settings);
    let codex_base_url = convert_base_url(&claude_base_url)?;

    // 3. 映射模型名称
    let claude_model = extract_claude_model(settings);
    let codex_model = map_model_name(&claude_model);

    // 4. 生成 Codex 配置
    let provider_name = sanitize_provider_name(&claude_provider.name);
    let config_toml = generate_codex_config(&provider_name, &codex_base_url, &codex_model);
    let auth_json = json!({ "OPENAI_API_KEY": api_key });

    // 5. 构建 Provider
    Ok(Provider {
        id: generate_provider_id(),
        name: format!("{} (Codex)", claude_provider.name),
        settings_config: json!({
            "auth": auth_json,
            "config": config_toml,
        }),
        website_url: claude_provider.website_url.clone(),
        category: claude_provider.category.clone(),
        linked_provider_id: Some(claude_provider.id.clone()),
        linked_source_app: Some("claude".to_string()),
        ..Default::default()
    })
}

/// 检查 Claude 供应商是否可以派生到 Codex
pub fn can_derive_to_codex(provider: &Provider) -> bool {
    let category = provider.category.as_deref().unwrap_or("");
    let api_format = provider.meta_api_format().unwrap_or_default();
    let provider_type = provider.meta_provider_type().unwrap_or_default();

    // 不可派生的类型
    if category == "official" { return false; }
    if category == "cloud_provider" && is_bedrock_or_vertex(provider) { return false; }
    if provider_type == "github_copilot" { return false; }
    if api_format == "gemini_native" { return false; }

    true
}

/// Base URL 转换：Anthropic 路径 → OpenAI 路径
fn convert_base_url(claude_url: &str) -> Result<String, AppError> {
    if claude_url.is_empty() {
        return Err(AppError::localized(
            "empty_base_url",
            "Claude 供应商未配置 Base URL，无法派生",
            "Claude provider has no Base URL configured, cannot derive",
        ));
    }

    let url = claude_url.trim_end_matches('/');

    // 已知路径模式替换
    if url.ends_with("/anthropic") || url.ends_with("/anthropic/v1") {
        let base = url.split("/anthropic").next().unwrap_or(url);
        return Ok(format!("{}/v1", base));
    }

    // 如果已经是 /v1 结尾，直接使用
    if url.ends_with("/v1") {
        return Ok(url.to_string());
    }

    // 默认追加 /v1
    Ok(format!("{}/v1", url))
}

/// API Key 提取
fn extract_api_key(settings: &Value) -> Result<String, AppError> {
    let env = settings.get("env").unwrap_or(&Value::Null);
    env.get("ANTHROPIC_AUTH_TOKEN")
        .or_else(|| env.get("ANTHROPIC_API_KEY"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| AppError::localized(
            "no_api_key",
            "未找到 API Key",
            "No API key found",
        ))
}

/// 模型名称映射
fn map_model_name(claude_model: &str) -> String {
    // Codex 默认使用 gpt-5.4
    // 如果用户通过第三方网关使用，保留原始模型名让网关路由
    if claude_model.starts_with("claude-") {
        "gpt-5.4".to_string()
    } else {
        claude_model.to_string()
    }
}
```

### 4.2 同步服务 — `sync_linked_providers()`

```rust
/// 当源供应商更新时，同步到所有关联的派生供应商
pub fn sync_linked_providers(
    state: &AppState,
    source_app: &AppType,
    source_provider_id: &str,
    updated_fields: &UpdatedFields,
) -> Result<Vec<String>, AppError> {
    let mut synced_ids = Vec::new();

    // 查找所有 linked_provider_id == source_provider_id 的供应商
    for target_app in AppType::all() {
        if target_app == *source_app { continue; }

        let linked = find_linked_providers(
            &state.db,
            &target_app,
            source_provider_id,
        )?;

        for mut target_provider in linked {
            // 按字段增量同步
            if let Some(new_key) = &updated_fields.api_key {
                update_target_api_key(&mut target_provider, &target_app, new_key)?;
            }
            if let Some(new_url) = &updated_fields.base_url {
                let converted_url = convert_base_url_for_app(&target_app, new_url)?;
                update_target_base_url(&mut target_provider, &target_app, &converted_url)?;
            }

            save_provider(&state.db, &target_app, &target_provider)?;
            synced_ids.push(target_provider.id.clone());
        }
    }

    Ok(synced_ids)
}
```

### 4.3 Tauri 命令 — `src-tauri/src/commands/cross_app.rs`（新增）

```rust
/// 从 Claude 供应商派生 Codex 供应商
#[tauri::command]
pub fn derive_codex_from_claude(
    state: State<'_, AppState>,
    claude_provider_id: String,
) -> Result<Provider, AppError> {
    let claude_provider = get_provider(&state.db, &AppType::Claude, &claude_provider_id)?;
    let codex_provider = cross_app::derive_codex_from_claude(&claude_provider)?;
    save_provider(&state.db, &AppType::Codex, &codex_provider)?;
    sync_current_to_live_for_app(&state, &AppType::Codex)?;
    Ok(codex_provider)
}

/// 检查供应商是否可派生
#[tauri::command]
pub fn can_derive_to_codex(
    state: State<'_, AppState>,
    claude_provider_id: String,
) -> Result<bool, AppError> {
    let provider = get_provider(&state.db, &AppType::Claude, &claude_provider_id)?;
    Ok(cross_app::can_derive_to_codex(&provider))
}

/// 批量派生
#[tauri::command]
pub fn batch_derive_codex_from_claude(
    state: State<'_, AppState>,
    claude_provider_ids: Vec<String>,
) -> Result<Vec<Provider>, AppError> {
    let mut results = Vec::new();
    for id in claude_provider_ids {
        let claude_provider = get_provider(&state.db, &AppType::Claude, &id)?;
        if cross_app::can_derive_to_codex(&claude_provider) {
            let codex_provider = cross_app::derive_codex_from_claude(&claude_provider)?;
            save_provider(&state.db, &AppType::Codex, &codex_provider)?;
            results.push(codex_provider);
        }
    }
    sync_current_to_live_for_app(&state, &AppType::Codex)?;
    Ok(results)
}

/// 解除供应商关联
#[tauri::command]
pub fn unlink_provider(
    state: State<'_, AppState>,
    app_type: String,
    provider_id: String,
) -> Result<(), AppError> {
    let app = AppType::from_str(&app_type)?;
    let mut provider = get_provider(&state.db, &app, &provider_id)?;
    provider.linked_provider_id = None;
    provider.linked_source_app = None;
    save_provider(&state.db, &app, &provider)?;
    Ok(())
}
```

---

## 五、前端实现

### 5.1 API 封装 — `src/lib/api/crossApp.ts`（新增）

```typescript
import { invoke } from "@tauri-apps/api/core";
import type { Provider } from "@/types";

export const crossAppApi = {
  /** 从 Claude 供应商派生 Codex 供应商 */
  deriveCodexFromClaude: (claudeProviderId: string) =>
    invoke<Provider>("derive_codex_from_claude", {
      claudeProviderId,
    }),

  /** 检查是否可派生 */
  canDeriveToCodex: (claudeProviderId: string) =>
    invoke<boolean>("can_derive_to_codex", {
      claudeProviderId,
    }),

  /** 批量派生 */
  batchDeriveCodexFromClaude: (claudeProviderIds: string[]) =>
    invoke<Provider[]>("batch_derive_codex_from_claude", {
      claudeProviderIds,
    }),

  /** 解除关联 */
  unlinkProvider: (appType: string, providerId: string) =>
    invoke<void>("unlink_provider", { appType, providerId }),
};
```

### 5.2 供应商卡片增强 — 派生按钮

在 Claude Code 供应商卡片的操作菜单中新增 **"派生到 Codex"** 按钮：

```typescript
// src/components/providers/ProviderCard.tsx — 操作菜单新增项

{activeApp === "claude" && canDerive && (
  <DropdownMenuItem onClick={handleDeriveToCodex}>
    <ArrowRightLeft className="mr-2 h-4 w-4" />
    {t("provider.deriveToCodex")}
  </DropdownMenuItem>
)}
```

### 5.3 派生确认对话框 — `DeriveProviderDialog.tsx`（新增）

```text
┌──────────────────────────────────────────────┐
│  派生供应商到 Codex                            │
│                                              │
│  源供应商: AiHubMix (Claude Code)              │
│                                              │
│  ┌────────────────────────────────────────┐   │
│  │ 名称:     AiHubMix (Codex)             │   │
│  │ Base URL: https://aihubmix.com/v1      │   │
│  │ API Key:  sk-***...***                 │   │
│  │ 模型:     gpt-5.4                      │   │
│  │ ☑ 保持关联（修改自动同步）                │   │
│  └────────────────────────────────────────┘   │
│                                              │
│  ⚠ Base URL 已从 /anthropic 转换为 /v1        │
│    请确认供应商支持 OpenAI Responses API        │
│                                              │
│            [取消]     [确认派生]                │
└──────────────────────────────────────────────┘
```

### 5.4 关联状态指示器

已关联的供应商卡片上显示链接图标：

```typescript
// 供应商卡片标题旁
{provider.meta?.crossAppSource && (
  <Tooltip content={t("provider.linkedFrom", {
    app: provider.meta.crossAppSource.sourceApp,
  })}>
    <Link2 className="h-3 w-3 text-muted-foreground" />
  </Tooltip>
)}
```

### 5.5 批量派生入口

在 Claude Code 供应商列表的工具栏增加 **"批量派生到 Codex"** 按钮：

```text
┌──────────────────────────────────────────────┐
│ Claude Code 供应商     [+ 添加] [⇄ 批量派生]   │
│──────────────────────────────────────────────│
│ ☑ AiHubMix          ● 当前使用                │
│ ☑ DMXAPI                                     │
│ ☐ Claude Official    (不可派生)               │
│ ☑ OpenRouter                                 │
│ ☐ AWS Bedrock        (不可派生)               │
│──────────────────────────────────────────────│
│          已选 3 个可派生供应商                  │
│              [取消]  [派生到 Codex]            │
└──────────────────────────────────────────────┘
```

---

## 六、预设映射表

对于已知的供应商预设，提供精确的跨应用 Base URL 映射，避免猜测：

```typescript
// src/config/crossAppPresetMap.ts（新增）

export interface CrossAppMapping {
  claudePresetId: string;
  codexPresetId: string;
  /** Claude Base URL 后缀 → Codex Base URL 后缀 */
  urlMapping: Record<string, string>;
}

export const crossAppPresetMap: CrossAppMapping[] = [
  {
    claudePresetId: "claude-3",   // AiHubMix
    codexPresetId: "codex-3",     // AiHubMix
    urlMapping: {
      "/v1": "/v1",
      "/api/v1": "/v1",
    },
  },
  {
    claudePresetId: "claude-4",   // DMXAPI
    codexPresetId: "codex-4",     // DMXAPI
    urlMapping: {
      "/v1": "/v1",
    },
  },
  {
    claudePresetId: "claude-44",  // OpenRouter
    codexPresetId: "codex-22",    // OpenRouter
    urlMapping: {
      "/api/v1": "/api/v1",
    },
  },
  // ... 更多预设映射
];
```

当派生时，如果检测到源供应商匹配已知预设，则直接使用预设映射的 URL 和配置模板，确保准确性。

---

## 七、同步流程时序

### 7.1 一键派生流程

```text
用户点击 "派生到 Codex"
    │
    ▼
前端调用 crossAppApi.canDeriveToCodex(id)
    │
    ├── false → 提示不可派生原因 → 结束
    │
    ▼ true
展示 DeriveProviderDialog（预览转换结果）
    │
    ▼ 用户确认
前端调用 crossAppApi.deriveCodexFromClaude(id)
    │
    ▼
后端 derive_codex_from_claude()
    │
    ├── 1. 读取 Claude 供应商配置
    ├── 2. 提取 API Key、Base URL、模型
    ├── 3. 查预设映射表 → 精确映射 or 通用转换
    ├── 4. 生成 Codex auth.json + config.toml
    ├── 5. 设置 linked_provider_id 关联
    ├── 6. 保存到 Codex 供应商列表
    └── 7. sync_current_to_live(Codex)
    │
    ▼
前端 invalidateQueries → 刷新供应商列表
    │
    ▼ 完成
```

### 7.2 关联同步流程

```text
用户修改 Claude 供应商 API Key
    │
    ▼
后端 update_provider(Claude, id, new_config)
    │
    ▼
检测到 has_linked_providers(id) == true
    │
    ▼
sync_linked_providers(Claude, id, { api_key: new_key })
    │
    ├── 查找 Codex 侧 linked_provider_id == id 的供应商
    ├── 更新 Codex auth.json 中的 OPENAI_API_KEY
    ├── 保存 Codex 供应商
    └── 写入 Codex live 配置（如果是当前激活供应商）
    │
    ▼
返回前端 { synced: ["codex-provider-id"] }
```

---

## 八、边界情况处理

| 场景 | 处理方式 |
|------|---------|
| Claude 供应商无 Base URL（官方直连） | 标记为不可派生，提示用户 |
| Claude 供应商使用 Bedrock/Vertex | 标记为不可派生，提示协议不兼容 |
| Codex 侧已存在同名供应商 | 名称自动加序号后缀 `(Codex 2)` |
| 目标供应商实际不支持 OpenAI 协议 | 对话框中显示⚠️警告，由用户确认 |
| 删除源 Claude 供应商 | 不级联删除 Codex 派生供应商，仅解除关联 |
| 删除派生的 Codex 供应商 | 不影响源 Claude 供应商 |
| 修改派生供应商的 Base URL | 检测到与源不一致时，提示是否解除关联 |
| 用户手动解除关联 | 双方变为独立供应商，不再自动同步 |
| Universal Provider 已存在 | 检测重复，提示用户已有统一供应商覆盖该网关 |

---

## 九、国际化

### 新增 i18n Key

```json
{
  "provider.deriveToCodex": "派生到 Codex",
  "provider.deriveToCodex.en": "Derive to Codex",
  "provider.deriveToCodex.ja": "Codex に派生",

  "provider.batchDerive": "批量派生到 Codex",
  "provider.batchDerive.en": "Batch Derive to Codex",

  "provider.linkedFrom": "已关联自 {{app}}",
  "provider.linkedFrom.en": "Linked from {{app}}",

  "provider.deriveDialog.title": "派生供应商到 Codex",
  "provider.deriveDialog.confirm": "确认派生",
  "provider.deriveDialog.keepLinked": "保持关联（修改自动同步）",
  "provider.deriveDialog.urlConverted": "Base URL 已从 {{from}} 转换为 {{to}}",
  "provider.deriveDialog.confirmProtocol": "请确认供应商支持 OpenAI Responses API",

  "provider.cannotDerive.official": "官方供应商无法派生",
  "provider.cannotDerive.cloudProvider": "云服务商（Bedrock/Vertex）协议不兼容",
  "provider.cannotDerive.noBaseUrl": "未配置 Base URL",

  "provider.unlinkConfirm": "确定解除关联？解除后修改不再同步。",
  "provider.synced": "已同步到 {{count}} 个关联供应商"
}
```

---

## 十、实施顺序

### Phase 1: 核心转换逻辑（后端）
1. `src-tauri/src/services/provider/cross_app.rs` — 配置转换、可派生检查
2. `src-tauri/src/commands/cross_app.rs` — 3 个 Tauri 命令
3. `src-tauri/src/provider.rs` — Provider 新增 `linked_provider_id` 字段
4. `src-tauri/src/services/provider/mod.rs` — 注册模块、`update_provider` 中加入同步触发
5. `src-tauri/src/lib.rs` — invoke_handler 注册
6. 单元测试：转换正确性、边界情况

### Phase 2: 预设映射表
7. `src/config/crossAppPresetMap.ts` — 已知供应商的精确映射
8. 后端读取映射表（或编译为 Rust 常量）

### Phase 3: 前端 UI
9. `src/lib/api/crossApp.ts` — API 封装
10. `src/components/providers/DeriveProviderDialog.tsx` — 派生确认对话框
11. `src/components/providers/ProviderCard.tsx` — 操作菜单新增派生按钮
12. `src/components/providers/ProviderList.tsx` — 批量派生工具栏
13. 关联状态图标

### Phase 4: 同步机制
14. `sync_linked_providers()` — 关联供应商自动同步
15. 解除关联 UI 及命令
16. `src/i18n/locales/{zh,en,ja}.json` — 国际化文案

### Phase 5: 优化（可选）
17. 反向派生：Codex → Claude Code
18. 扩展到 Gemini CLI
19. 派生历史记录

---

## 十一、关键文件索引

### 新增文件（5 个）
- `src-tauri/src/services/provider/cross_app.rs` — 核心转换逻辑
- `src-tauri/src/commands/cross_app.rs` — Tauri 命令
- `src/lib/api/crossApp.ts` — 前端 API 封装
- `src/components/providers/DeriveProviderDialog.tsx` — 派生对话框
- `src/config/crossAppPresetMap.ts` — 预设映射表

### 必须修改的后端文件（4 个）
- `src-tauri/src/provider.rs` — Provider 新增关联字段
- `src-tauri/src/services/provider/mod.rs` — 模块注册 + 同步触发
- `src-tauri/src/commands/mod.rs` — 模块注册
- `src-tauri/src/lib.rs` — invoke_handler 注册

### 必须修改的前端文件（5 个）
- `src/types.ts` — ProviderMeta 新增 crossAppSource
- `src/components/providers/ProviderCard.tsx` — 派生按钮
- `src/components/providers/ProviderList.tsx` — 批量派生工具栏
- `src/i18n/locales/zh.json` — 中文
- `src/i18n/locales/en.json` — 英文
- `src/i18n/locales/ja.json` — 日文

### 参考文件
- `src-tauri/src/services/provider/live.rs` — `write_live_snapshot()` 理解写入逻辑
- `src-tauri/src/codex_config.rs` — `write_codex_live_atomic()` 理解 Codex 写入
- `src-tauri/src/provider.rs` — `UniversalProvider::to_codex_provider()` 参考现有转换
- `src/config/codexProviderPresets.ts` — `generateThirdPartyConfig()` 参考 TOML 生成
- `src/utils/providerConfigUtils.ts` — TOML 操作工具函数

---

## 十二、验证计划

### 后端验证
1. **单元测试** — `cross_app.rs`
   - `derive_codex_from_claude`: 验证各类供应商的转换结果
   - `convert_base_url`: 验证 URL 转换规则覆盖所有模式
   - `can_derive_to_codex`: 验证不可派生类型被正确排除
   - `sync_linked_providers`: 验证 API Key / Base URL 同步
2. **集成测试** — 完整派生流程
   - 派生后 Codex live 文件内容正确
   - 修改源供应商后关联供应商同步更新
   - 解除关联后不再同步

### 前端验证
1. `pnpm typecheck` — TypeScript 类型检查
2. `pnpm lint` — ESLint 检查
3. **手动 UI 测试**
   - 派生对话框展示正确的转换预览
   - 不可派生供应商的菜单项正确禁用/隐藏
   - 关联图标正确显示
   - 批量派生选择与执行
   - 解除关联交互
