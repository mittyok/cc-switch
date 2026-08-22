# CC-Switch 性能深度分析与优化方案

## 概述

对 cc-switch 代理转发核心路径进行全面排查后，发现了 **5 个关键性能瓶颈**和若干次要问题。
以下按影响程度排序，附带量化估算和具体优化方案。

---

## 🔴 P0 级：每请求新建 TCP + TLS 连接（Claude/Anthropic 路径）

### 问题描述

`hyper_client.rs::send_raw_request()` 是 Claude Code → Anthropic API 的主路径。
**每次请求都会重新：TCP 三次握手 → TLS 握手 → 发送请求。**

```rust
// hyper_client.rs:414-428 — 每次都新建连接
tokio::net::TcpStream::connect((host, port)).await   // ~20-50ms
tls_connector.connect(server_name, stream).await      // ~100-200ms (RSA/ECDHE)
```

### 为什么这样？

为了 **保持原始 HTTP header 大小写** — 直接写 raw bytes 到 TLS stream，绕过 hyper 的 header 标准化。

### 影响量化

| 指标 | 数值 |
|------|------|
| TCP 握手 (国内→海外 API) | 30-80ms |
| TLS 1.3 握手 | 100-200ms |
| **总额外延迟/请求** | **150-300ms** |
| 对比：reqwest 池化路径 (Codex/Gemini) | 0ms（复用连接） |

**在一次 Claude Code 会话中，平均每轮对话有 2-5 次 API 调用（thinking + tool calls），
累积额外延迟 300ms-1.5s/轮。**

### 为什么可以安全优化

1. hyper-util Client **已经支持** `http1_preserve_header_case(true)` — 通过 hyper 内部的 `HeaderCaseMap` extension 实现 header 大小写保持
2. Anthropic API **不依赖** header 大小写 — HTTP/1.1 规范明确说 header names 是 case-insensitive
3. Claude Code 客户端发出的 headers 本身就是标准 title-case（`Content-Type`, `Authorization`）
4. 代码中已有 `global_hyper_client()` 作为 fallback，它就是池化的

### 优化方案

```rust
// 方案 A（最小改动）：将 global_hyper_client 升级为 primary path
// 原始 cases 通过 HeaderCaseMap extension 传入 hyper-util Client
fn should_preserve_exact_header_case(...) -> bool {
    false  // 全部走 pooled path，hyper-util 的 preserve_header_case 已足够
}

// 方案 B（最佳性能）：启用 HTTP/2
// Cargo.toml:
hyper-rustls = { features = ["http1", "http2", ...] }
// 连接器：
HttpsConnectorBuilder::new()
    .with_webpki_roots()
    .https_or_http()
    .enable_http1()
    .enable_http2()  // 新增
    .build()
```

### 预计收益

- **方案 A**：消除 150-300ms/请求（连接池复用）
- **方案 B**：在 A 的基础上，HTTP/2 多路复用进一步消除 HoL blocking，并发请求共享连接

---

## 🔴 P1 级：每请求 7-9 次 SQLite 查询（无缓存）

### 问题描述

`RequestContext::new()` + `ProviderRouter::select_providers()` 在每次代理请求时查询数据库：

| 函数调用 | SQLite 查询次数 | 数据变化频率 |
|----------|----------------|-------------|
| `get_proxy_config_for_app` | 1 | 用户手动改动时 |
| `get_rectifier_config` | 1 | 用户手动改动时 |
| `get_optimizer_config` | 1 | 用户手动改动时 |
| `get_copilot_optimizer_config` | 1 | 用户手动改动时 |
| `get_effective_current_provider` → `get_all_providers` | 1 | 用户切换时 |
| `get_current_provider` | 1 | 用户切换时 |
| `get_provider_by_id` | 1 | 极少 |
| `get_proxy_config_for_app`（again in select_providers）| 1 | 同上 |
| `get_failover_queue` | 0-1 | 用户配置时 |

**且 SQLite 连接被 `Mutex<Connection>` 包装 — 所有请求串行竞争同一把锁。**

### 影响量化

- SQLite 查询延迟：~0.1-0.5ms/次
- Mutex 竞争（并发场景）：0-5ms
- **每请求总开销：1-10ms**
- 在 usage log 写入同时竞争时：可能 5-20ms

### 优化方案

```rust
// 引入内存配置缓存（write-through with invalidation）
pub struct ConfigCache {
    proxy_configs: DashMap<String, (AppProxyConfig, Instant)>,
    rectifier: ArcSwap<RectifierConfig>,
    optimizer: ArcSwap<OptimizerConfig>,
    providers: ArcSwap<HashMap<String, Vec<Provider>>>,
    ttl: Duration,
}

// 配置变更时 invalidate：
// - 在 set_rectifier_config / set_optimizer_config / save_provider 时清缓存
// - 或者用 database update_hook 自动 invalidate
```

### 预计收益

- **配置缓存**：7-9 次 SQLite 查询 → 0 次（cache hit）→ 节省 1-10ms/请求
- **消除 Mutex 竞争**：并发场景下节省 5-20ms

---

## 🟡 P2 级：未启用 HTTP/2

### 问题描述

- `hyper-rustls` Cargo feature 只启用了 `http1`，没有 `http2`
- reqwest 虽然链接了 h2 crate（通过 rustls-tls），但 hyper 直连路径无法使用 HTTP/2
- Anthropic API **支持 HTTP/2**

### 影响

- 无法多路复用：并发请求各开独立连接
- 无 header 压缩 (HPACK)
- 存在 head-of-line blocking（HTTP/1.1 pipeline 限制）

### 优化方案

```toml
# Cargo.toml
hyper-rustls = { version = "0.27", features = ["http1", "http2", "tls12", "ring", "webpki-tokio"] }
```

```rust
// hyper_client.rs - global_hyper_client
let connector = HttpsConnectorBuilder::new()
    .with_webpki_roots()
    .https_or_http()
    .enable_http1()
    .enable_http2()  // 新增
    .build();
```

### 预计收益

- 连接多路复用：并发请求共享 1-2 条连接
- Header 压缩：减少 ~2KB/请求
- 消除 HTTP/1.1 HoL blocking

---

## 🟡 P3 级：未设置 TCP_NODELAY

### 问题描述

代理服务器的 accept loop 中，新建的 `TcpStream` **没有设置 `TCP_NODELAY`**。

Nagle 算法会将小包合并后再发送，对 SSE 流式响应造成高达 **40ms 延迟**（Nagle 默认等待时间）。
这对小 SSE chunk（如单个 token 的 streaming）特别致命。

上游连接（raw write path）的 TcpStream 也没设置 TCP_NODELAY。

### 影响量化

| 场景 | Nagle 延迟 | 说明 |
|------|-----------|------|
| SSE 小 chunk 转发（<MSS） | 0-40ms/chunk | 典型 token 约 10-50 bytes |
| 大响应体 | 0ms | 数据量够大不触发 Nagle |
| **长对话流式输出** | 累积 1-5s/响应 | 100 个 token × 40ms |

### 优化方案

```rust
// server.rs accept loop:
let (stream, _remote_addr) = listener.accept().await?;
stream.set_nodelay(true)?;  // 新增

// hyper_client.rs raw write path:
let tcp = tokio::net::TcpStream::connect((host, port)).await?;
tcp.set_nodelay(true)?;  // 新增
```

### 预计收益

- SSE 流式延迟降低 0-40ms/chunk
- 对感知速度提升最为明显（输出不再"一卡一卡"）

---

## 🟡 P4 级：请求路径上的 RwLock 竞争

### 问题描述

`forwarder.rs` 中 per-request 热路径有 **10+ 次 `write().await`** 操作在共享状态上：

- `status.write().await` — 更新 total_requests、active_connections、current_provider、success_rate 等
- `current_providers.write().await` — 更新当前使用的 provider

这些字段多数只是 UI 展示用的统计数据，完全可以用原子操作替代。

### 影响量化

- 无竞争时：<1μs
- 有并发时（如 Claude Code tool use 并发请求）：1-5ms
- **但与 P0/P1 相比是次要影响**

### 优化方案

```rust
pub struct ProxyStatus {
    pub running: AtomicBool,
    pub total_requests: AtomicU64,
    pub success_requests: AtomicU64,
    pub failed_requests: AtomicU64,
    pub active_connections: AtomicU32,
    // 只有这些需要保留 RwLock:
    pub current_provider: RwLock<Option<String>>,
    pub last_error: RwLock<Option<String>>,
}
```

### 预计收益

- 10+ 次 write lock → 2-3 次 + 原子操作
- 并发场景节省 1-5ms

---

## 📊 汇总：优化收益预估

| 优化项 | 首包延迟改善 | 流式体验改善 | 实施难度 |
|--------|------------|------------|---------|
| P0: 连接池复用 | -150~300ms | 间接（连接建立更快） | ⭐⭐ |
| P1: 配置缓存 | -1~10ms | 无 | ⭐⭐ |
| P2: HTTP/2 | -50~100ms（并发场景） | 多路复用 | ⭐ |
| P3: TCP_NODELAY | -0~5ms | **显著：每 chunk -0~40ms** | ⭐（一行代码） |
| P4: Atomic 替代 RwLock | -1~5ms | 无 | ⭐⭐⭐ |

**综合预估：对于典型 Claude Code 使用场景：**
- **首包响应（TTFB）**：从当前额外 200-350ms → 接近 0ms（连接池 + HTTP/2）
- **流式输出卡顿**：从 0-40ms/chunk → ~0ms（TCP_NODELAY）
- **整轮对话（2-5次 API call）**：节省 400ms-1.5s

---

## 🛠 实施优先级建议

1. **立即做** — P3: TCP_NODELAY（一行代码，立竿见影）
2. **优先做** — P0: 连接池（核心问题，最大收益）
3. **一起做** — P2: HTTP/2（和 P0 一起改，改 Cargo.toml + 几行代码）
4. **之后做** — P1: 配置缓存（需要设计 invalidation 机制）
5. **选做** — P4: Atomic 替代 RwLock（重构较大，收益相对小）

---

## 额外发现（非阻塞性）

1. **peek buffer 堆分配**：每个连接 `vec![0u8; 8192]`，可改用栈数组 `[0u8; 8192]`
2. **JSON canonicalize**：对大 body 做递归 key 排序 + SHA256 摘要，只用于日志，可跳过
3. **body.clone()**：failover/optimizer 场景下请求体被 clone 2-3 次，大对话时有 GC 压力
4. **usage_events 用 std::thread::sleep**：应改为 tokio::time::sleep 避免线程创建开销

---

*分析完成于 2026-08-22*

---

## ✅ 已实施的优化（本次提交）

### 改动文件

| 文件 | 改动内容 |
|------|---------|
| `Cargo.toml` | 启用 `http2` feature (hyper-rustls + hyper-util) |
| `proxy/hyper_client.rs` | 池化 Client 升级为 primary path + HTTP/2 + TCP_NODELAY |
| `proxy/http_client.rs` | reqwest Client 加 `tcp_nodelay(true)` |
| `proxy/server.rs` | accept loop 加 `set_nodelay(true)` + peek buffer 改栈分配 |

### 测试验证

- 2707 tests passed, 0 regressions
- 3 pre-existing failures (unrelated `transform_codex_chat` + `session_usage_codex`)

### 预计效果

- **首包延迟（TTFB）**：Claude API 路径减少 150-300ms/请求（连接复用生效）
- **流式输出体感**：消除 Nagle 导致的 0-40ms/chunk 卡顿
- **HTTP/2 多路复用**：并发请求共享连接，减少资源消耗
- **整体**：典型单轮对话（2-5 API calls）快 400ms-1.5s
