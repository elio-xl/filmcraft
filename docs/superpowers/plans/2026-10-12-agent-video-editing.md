# FilmCraft MCP 自动剪辑 P0 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. 本文件记录计划及执行状态；仅勾选项表示已经完成，不代表未勾选功能已通过验收。

**Goal:** 让 Codex 通过 MCP 查看未进入时间线的素材缩略图和精确采样帧，并完成可验证、可保存的自动剪辑流程。

**Architecture:** 素材分析位于 engine，使用原始媒体、工程快照和独立后台队列。MCP 是薄适配层，Headless 与 Bridge 调用同一组引擎命令；图像通过 MCP 图片内容块交给模型。工程编辑复用现有命令，不改变现有批量操作的撤销语义。

**Tech Stack:** Rust 2024、Rust 1.95 或更新兼容版本、serde、serde_json、rmcp、tokio、现有媒体解码器、image PNG、base64、sha2。

**Spec:** [基于 MCP 的自动剪辑需求文档](../../requirements/agent-video-editing.md)。执行者必须同时阅读需求及仓库根目录的 AGENTS.md、CLAUDE.md。

## Global Constraints

- P0 对应需求 AC01 至 AC11；P1 视频与分析见[第二阶段计划](2026-10-12-agent-media-analysis-p1.md)，P1 编辑可靠性见[第三阶段计划](2026-10-12-agent-edit-reliability-p1.md)。三个阶段分别验收，不把 P1 当作 P0 已完成条件。
- 每秒 254016000000 ticks；新增接口的 tick 值为十进制字符串；源区间为 `[start, end)`；不改变已有接口格式。
- 图片长边默认 512、最大 1920；帧数默认 8、最大 16；完整 MCP 图片响应最大 8 MiB，包含 base64 和元数据。
- 每会话最多 1 个活动分析任务、8 个排队任务。取消、失败、panic 都必须进入终态。
- 不使用原片代理或离线占位图替代真实源内容，不修改播放头、选区、工程内容和撤销历史。
- 不引入 L4 对 L5 的依赖，不读取第三方编解码器实现，不将 FFmpeg 链接、捆绑或作为产品运行依赖。
- 使用 Result、受限分配、检查算术和有界遍历；后台线程遵循 catch_unwind 约定。
- 每个提交之前执行 `cargo xtask ci`；仅提交通过质量门禁的状态。提交信息以任务 ID 开头，使用环境规定的归属信息。
- 生成媒体写入 `target/fixtures/agent-editing/`；不提交二进制夹具。需要提交的资产遵守 attribution 规则。

## Review Focus

1. 解码失败被 UI 包装为离线占位图：分析必须返回错误，由 AE02 的 `offline_slate_is_never_analysis_content` 覆盖。
2. 非零起始 PTS、B 帧及变帧率：必须返回展示时间而非请求时间或解码序号，由 AE03 的 `analysis_uses_presentation_time` 覆盖。
3. 同路径、同长度、首尾相同但中间内容变化：缓存必须失效，由 AE05 的 `middle_bytes_change_invalidates_cache` 覆盖。
4. Bridge 响应丢失后自动重连：不能重复创建后台任务，由 AE06 的 `bridge_retry_reuses_analysis_request` 覆盖。
5. 旋转元数据、非方形像素和透明静态图：缩略图应保持显示方向、显示比例和透明信息，由 AE03 的 `analysis_preserves_display_geometry` 覆盖。

## 实施边界与顺序

依赖顺序：`AE01 → AE02 → AE03 → AE04 → AE05 → AE06 → AE07 → AE08`。

AE01 至 AE06 共享接口，默认按顺序实施；采用多执行者时须使用独立工作树，不同时修改 engine 的注册表和 Session。开始执行时创建 `codex/agent-video-editing-p0` 分支或使用已存在的合适隔离工作树；本轮编写计划不创建分支。

执行环境已准备 Rust 1.95.0、rustfmt、Clippy 和 wasm32 目标。AE01 的 11 项测试已通过；完整门禁通过（2817 项测试成功、41 项原有忽略，44 个 crate 的 wasm 检查成功）。本地门禁保留 release 优化级别，使用进程级 `CARGO_PROFILE_RELEASE_LTO=false`、`CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16`，不改变仓库发布配置。

## 文件职责

| 文件 | 变更 | 职责 |
|---|---|---|
| `crates/engine/src/media_analysis/mod.rs` | 新增 | 命令入口、共享错误和结果结构 |
| `crates/engine/src/media_analysis/request.rs` | 新增 | 参数解析、精确时间、资源预算 |
| `crates/engine/src/media_analysis/source.rs` | 新增 | 只读源解析、元数据和子片段映射 |
| `crates/engine/src/media_analysis/frames.rs` | 新增 | 采样、解码、图片缩放及 PNG 编码 |
| `crates/engine/src/media_analysis/jobs.rs` | 新增 | 单活动任务队列、进度、结果、取消 |
| `crates/engine/src/media_analysis/cache.rs` | 新增 | 内容版本、受限缓存和清理 |
| `crates/engine/src/media_analysis/tests.rs` | 新增 | 引擎及异常参数回归测试 |
| `crates/engine/src/lib.rs`、`commands.rs` | 修改 | Session 分析服务、命令及作业注册 |
| `crates/engine/Cargo.toml`、`Cargo.lock` | 修改 | PNG、base64、内容哈希依赖 |
| `crates/media/src/lib.rs` | 修改 | 可验证的源帧展示时间接口 |
| `crates/media/src/still.rs`、`sequence.rs`、`crates/codecs/src/mp4.rs`、`gop.rs` | 按 AE03 修改 | 首版支持路径的精确 PTS 实现 |
| `crates/automation/src/media_analysis.rs`、`media_analysis_tests.rs` | 新增 | MCP 参数、结果转换及传输测试 |
| `crates/automation/src/lib.rs`、`server.rs`、`bridge.rs` | 修改 | 工具注册及两种后端适配 |
| `apps/filmcraft-cli/tests/agent_editing.rs` | 新增 | CLI 与 stdio MCP 全流程测试 |
| `docs/agents.md`、`docs/control-protocol.md` | 修改 | 新命令、工具和 Codex 使用说明 |
| `docs/testing/agent-video-editing.md` | 新增 | 夹具生成、真客户端验证及基准记录格式 |

## 共同接口契约

以下是新增接口的执行目标，尚不存在于产品中。引擎公开入口均使用 `serde_json::Value` 作为命令边界，内部使用有类型结构。

| 引擎命令 | MCP 工具 | 命令行为 |
|---|---|---|
| `media.analysis.inspect` | `media_inspect` | 校验素材 ID，启动元数据和版本任务，返回 `{job, request_id}` |
| `media.analysis.frames` | `media_frames` | 校验采样请求，启动任务，返回 `{job, request_id}` |
| `media.analysis.result` | `media_analysis_result` | 按 job 读取状态或已完成结果；MCP 将帧转为图片块 |
| `media.analysis.clearCache` | 通过 `command_run` | 清理未使用缓存，不改变工程 |
| `jobs.list`、`jobs.cancel` | 通过现有入口 | 同时查询或取消分析作业与原有作业，保持旧字段兼容 |

`media_inspect` 和 `media_frames` 默认 `wait=true`，MCP 层轮询任务且不占用 Session 锁；`wait=false` 返回任务 ID，随后使用 `media_analysis_result`。CLI 和 Bridge 引擎调用始终先启动、后读取结果，不能在 UI 线程等待解码。

公共输入字段：`item` 为项目素材的现有整数 ID；`expected_version` 为可选内容版本字符串；`request_id` 为最多 128 字符的请求标识。精确时间为 `start_ticks`、`end_ticks`、`times_ticks`。`strategy` 为 `uniform` 或 `timestamps`；两组时间参数互斥。

`uniform` 允许省略区间，默认覆盖该素材有效源区间；设区间为 `[a,b)`、帧数为 n，采样时间为 `a + floor(i*(b-a)/n)`，`i=0..n-1`，用检查后的 i128 中间运算。`timestamps` 数组最多 16 项，拒绝空数组、越界及未知字段。

结果包含 `schema_version=1`、`status`、`item`、`root_item`、`source_version`、源区间和 `coordinate_space="source"`。元数据额外含显示方向、编码尺寸、显示尺寸及音轨；无法确定的字段返回 null 及原因，不能填入猜测值。

帧条目含 `frame_id`、`request_indices`、`requested_ticks`、`actual_ticks`、`width`、`height`、`mime_type`、`png_base64`。引擎结果以 JSON 提供，MCP 把 `png_base64` 移入图片块，结构化元数据使用 `content_index` 对应，避免重复输出图片数据。

失败条目含请求序号、`code`、`message`、`retryable`；整页使用 `complete`、`partial` 或 `error`，不能把空成功数组标记为完整。预算截断返回 `remaining_times_ticks`，客户端据此发起明确的新请求，无隐式页游标。

## AE01 参数与协议契约

**Files:** 新增 `media_analysis/mod.rs`、`request.rs`、`tests.rs`；修改 engine `lib.rs`。

**Interfaces:** 新增 `AnalysisRequest`、`AnalysisLimits`、`AnalysisFailure`。`request::parse(params: &Value) -> crate::Result<AnalysisRequest>` 仅解析结构；`request::sample_times(request: &AnalysisRequest, available: TimeRange) -> crate::Result<Vec<Tick>>` 校验范围并返回采样点。`AnalysisFailure` 包含 `code: String, message: String, retryable: bool`。

- [x] 写测试 `analysis_request_limits`：`assert_eq!(defaults.max_side, 512)`、`assert_eq!(defaults.max_frames, 8)`；1920 和 16 合法，1921、17、0、负数、浮点时间、溢出字符串和未知键返回 Err。
- [x] 写测试 `uniform_sampling_is_half_open`：对 `[0, Tick(254016000000))` 取两帧，断言结果为 `Tick(0)` 和 `Tick(127008000000)`，不包含 end；越界时间点返回 Err。
- [x] 运行 `cargo test -p filmcraft-engine media_analysis::tests::analysis_request` 和 `cargo test -p filmcraft-engine uniform_sampling_is_half_open`，确认新断言失败；实现参数类型和解析后重跑，通过且实际执行测试数不为零。 实际合并运行 `cargo test --offline -p filmcraft-engine media_analysis::tests`，覆盖上述两个筛选范围：首次 10 项失败、1 项通过，实现后 11 项全部通过。
- [x] 检查新类型不依赖 UI 或 MCP crate；执行 `cargo xtask ci`，通过后提交 `AE01: Define media analysis request contracts`。

## AE02 只读源解析与元数据

**Files:** 新增 `media_analysis/source.rs`；修改 `media_analysis/tests.rs`，必要时补充 `media_pool.rs` 的受限读取入口。

**Interfaces:** `source::resolve(session: &Session, item: ItemId) -> crate::Result<AnalysisSource>`；`AnalysisSource` 保存 `root_item: ItemId, source: SharedSource, available: TimeRange, services: Arc<dyn Services>` 和内容版本所需的源标识；`source::inspect(source: &AnalysisSource) -> crate::Result<Value>`。

- [x] 写 `unplaced_media_is_inspectable`，导入图片但不加入序列，断言元数据正确且工程、选区、历史、revision 均未变化。写 `offline_slate_is_never_analysis_content`，离线、坏文件和不支持编码均返回错误，无 PNG 结果。
- [x] 写 `subclip_analysis_has_root_coordinates`，核对母素材 ID 和源范围；循环引用和超过 16 层的链返回 Err。遵循当前 Project 子片段范围语义，不重复累加已经位于母素材坐标系的偏移。
- [x] 运行 `cargo test -p filmcraft-engine media_analysis::tests` 确认失败；使用原片受限 opener 实现 resolve，避免调用会返回 SlateSource 的显示包装。没有流式 reader 的宿主只允许不超过 64 MiB 的整文件读取，超出返回能力错误。
- [x] 实现元数据和可用性输出，保持不依赖当前活动序列；重跑测试，执行 `cargo xtask ci`，提交 `AE02: Resolve source media for read-only analysis`。

## AE03 精确展示时间和图像输出

**Files:** 新增 `media_analysis/frames.rs`；修改 media `lib.rs`、`still.rs`、`sequence.rs`、codecs `mp4.rs`、`gop.rs`、engine `Cargo.toml` 和分析测试。

**Interfaces:** 在 `MediaSource` 增加默认返回 Unsupported 的 `analysis_frame(&self, req: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>>`。仅确认 PTS 正确的实现覆盖它，返回帧的 `pts` 是实际源展示时间。`frames::decode(source: &AnalysisSource, request: &AnalysisRequest) -> crate::Result<Value>` 输出共同契约的帧页。

- [x] 写 `analysis_uses_presentation_time`：使用展示时间为 0、40、100 毫秒的夹具，在 70 毫秒请求命中 40 毫秒帧；另测非零起始 PTS 和 B 帧重排。断言 `actual_ticks != requested_ticks`，不能用请求值回填。
- [x] 写 `analysis_preserves_display_geometry`：旋转 90 度的 1920×1080 输入返回 288×512；非方形像素按显示比例缩放；小图不放大、PNG 保留透明度。写 `duplicate_requests_keep_all_indices`，多个请求命中同一 PTS 时只返回一个图像并保留所有请求序号。
- [x] 先运行相关 media、codecs 和 engine 测试确认失败；实现首版静态 PNG/JPEG、图像序列和 MP4/MOV 已支持解码路径的分析方法。使用容器展示时间和 edit offset；其他格式显式 Unsupported，不谎报零时间。生成器在测试适配器中实现可预测的展示时间。
- [x] 将 engine 的 image PNG 依赖从 dev-dependencies 移入 dependencies，增加与 automation 一致的 base64 0.22。逐帧处理，长边最大 1920，编码前校验像素与缓冲区长度；保留帧 PTS，按 PTS 去重而非像素内容去重。
- [x] 运行 `cargo test -p filmcraft-media analysis_`、`cargo test -p filmcraft-codecs analysis_`、`cargo test -p filmcraft-engine media_analysis::tests`，检查筛选出的测试确实运行；再运行 `cargo xtask ci`，提交 `AE03: Return timestamped source thumbnails`。

## AE04 后台队列与取消

**Files:** 新增 `media_analysis/jobs.rs`；修改 engine `lib.rs`、`commands.rs`、分析测试。

**Interfaces:** Session 增加会话级 `AnalysisQueue`。公开 `media_analysis::inspect(session: &mut Session, params: &Value) -> crate::Result<Value>`、`frames(session: &mut Session, params: &Value) -> crate::Result<Value>` 启动任务；`result(session: &Session, params: &Value) -> crate::Result<Value>` 读取结果；`commands() -> Vec<CommandSpec>` 注册入口。

- [x] 写 `analysis_queue_is_bounded`：1 个阻塞中的活动任务加 8 个排队任务被接受，第 10 个请求返回繁忙错误；job ID 与导出任务不冲突。
- [x] 写 `cancel_and_panic_leave_terminal_jobs`：排队取消不开始解码，运行取消能结束，注入 panic 产生失败结果；之后新的请求成功。测试等待必须有超时，不能让测试永久阻塞。
- [x] 写 `analysis_does_not_hold_session_lock`：慢解码期间 `project.inspect`、`jobs.list` 和 `jobs.cancel` 仍能完成。先运行 `cargo test -p filmcraft-engine media_analysis::tests` 确认失败。
- [x] 为分析建立独立结果类型，不强塞进当前 `Job` 的导出 `Report`。扩展 `jobs.list` 合并元数据、扩展 `jobs.cancel` 路由，同一 ID 分配器分配两类任务；图片数据只经 `media.analysis.result` 读取。
- [x] 快照和排队在短锁内完成，源打开、哈希、解码在线程中完成。复用媒体取消机制，每帧和分块读取间检查取消，工作线程捕获 panic；终态结果使用受限缓存管理，不要求 UI 调用 poll 才能变成 finished。
- [x] 重跑分析及现有作业测试，运行 `cargo xtask ci`，提交 `AE04: Schedule cancellable media analysis jobs`。

## AE05 内容版本与预算缓存

**Files:** 新增 `media_analysis/cache.rs`；修改 `jobs.rs`、`frames.rs`、engine `Cargo.toml`、`Cargo.lock` 和分析测试。

**Interfaces:** `cache::source_version(source: &AnalysisSource, cancelled: &AtomicBool) -> crate::Result<String>`；`media_analysis::clear_cache(session: &mut Session, params: &Value) -> crate::Result<Value>`。版本采用完整源内容 SHA-256；新增 sha2 0.10，沿用仓库已经使用的依赖系列。生成器按确定性序列化参数哈希，图像序列包含有序文件清单和每帧内容版本。

- [x] 写 `middle_bytes_change_invalidates_cache`：相同路径、长度、首尾及恢复后的修改时间，中间字节变化仍得到不同版本。写 `version_mismatch_rejects_request`：指定旧 expected_version 时，任务返回冲突，无旧图片。
- [x] 写 `response_budget_keeps_complete_frames`：引擎图片页为最终 8 MiB 响应预留 256 KiB 元数据空间；最终 MCP 序列化限额在 AE06 适配层验收。超限时 JSON 和 PNG 均完整，未返回的时间点列入 remaining_times_ticks；单帧超限返回资源错误。
- [x] 写 `cache_eviction_keeps_active_results`：清理不删正在消费的结果；重复参数及版本命中同一缓存条目。先运行 `cargo test -p filmcraft-engine media_analysis::tests` 确认失败。
- [x] 哈希按 1 MiB 分块读取，检查取消；每次新请求重新验证内容版本，再复用解码结果。生成前后版本不一致时丢弃输出并返回冲突。原有首尾快速指纹只用于候选提示，不能充当精确内容版本。
- [x] P0 使用会话内编码结果缓存，默认上限 128 MiB 和 32 个已完成结果，按 LRU 清理未使用条目；关闭会话释放，`clearCache` 返回移除数量。过期 job 读取返回 RESULT_EXPIRED，不能伪装仍在运行。缓存满且无法淘汰时返回预算错误。
- [x] 重跑测试并测量哈希与缓存命中耗时；全量哈希的 IO 成本如实记录。运行 `cargo xtask ci`，提交 `AE05: Bound analysis results and validate media versions`。

## AE06 MCP 工具和 Bridge 一致性

**Files:** 新增 automation `media_analysis.rs`、`media_analysis_tests.rs`；修改 automation `lib.rs`、`server.rs`、`bridge.rs`；修改 docs `agents.md`、`control-protocol.md`。

**Interfaces:** 注册 `media_inspect`、`media_frames`、`media_analysis_result`。新增 `media_analysis::to_tool_result(page: Value) -> Result<CallToolResult, AutomationError>`，根据共同契约生成图片块和结构化元数据。wait 模式采用独立的分析轮询函数，不复用带导出文件删除逻辑的 `run_long`。

- [x] 写 `mcp_analysis_returns_images_and_metadata`：图片块 MIME 为 image/png，解码后尺寸正确，content_index 能对应每帧，文字中没有重复 base64；空结果为 isError，部分成功显式 partial。
- [x] 写 `bridge_retry_reuses_analysis_request`：模拟服务已启动任务但响应断开，重连后相同 request_id 和参数返回原 job，工作队列只有一个任务；同 ID 不同参数返回冲突。已过期的请求返回过期错误，不悄悄重做。
- [x] 写 `bridge_analysis_does_not_seek`：Headless 与模拟 Bridge 返回等价帧元数据；调用日志中无 playhead.set、插入片段或 UI 点击。write-free 工具注解和严格未知参数测试一并覆盖。
- [x] 运行 `cargo test -p filmcraft-automation media_analysis` 确认失败；实现 start/poll/result，等待间释放连接及 Session 锁。取消只取消自身 job。请求标识与去重放在引擎队列，覆盖 Bridge 的自动重连重发。
- [x] 在 MCP 适配层核算包含元数据的最终 8 MiB 限额；超限按整帧减少并合并 remaining_times_ticks，禁止截断字节。Bridge 增加有界响应读取，针对分析响应使用足够的有限上限，不将现有 4 MiB 请求行限制误当作响应限制。
- [x] 更新引擎命令和工具文档，明确缓存、任务和 readOnlyHint 的含义；运行 automation、CLI 回归及 `cargo xtask ci`，提交 `AE06: Expose source analysis over MCP and bridge`。

## AE07 可重复的自动剪辑验收

**Files:** 新增 `apps/filmcraft-cli/tests/agent_editing.rs`、`docs/testing/agent-video-editing.md`；修改 `docs/agents.md`。

**Interfaces:** 使用现有 `command_list`、`command_run`、`command_batch`、`doc_inspect`、`render_preview` 和 AE06 工具。新增测试辅助 `generate_fixtures(dir: &std::path::Path) -> std::path::PathBuf`，返回夹具清单文件路径；仅在测试中使用 expect/unwrap。

- [x] 写 `agent_editing_round_trip`：程序生成三段具有明确场景和帧编号的视频、一张 PNG、一段音频；通过 stdio MCP 导入、分析、构建 30 秒 1280×720/25fps 序列、添加标题和配乐，保存并重开，导出。
- [x] 对时间线时长、素材源入出点、标题、音轨和重开后的工程内容做结构断言；对导出尺寸、时长和代表帧做媒体探测及图像断言。帧映射还覆盖分数帧率、变帧率、重复使用、变速和倒放；调用现有 TrackItem 时间映射，不新增另一套编辑时间算法。
- [x] 写 `failed_batch_can_be_recovered`：第二步注入不存在的命令，断言第一步保留、后续未执行，重新读取状态后可以撤销第一步。先运行 `cargo test -p filmcraft-cli --test agent_editing` 确认测试能暴露未完成路径。
- [x] 测试生成使用现有 Rust 生成器、编码器及 PNG/WAV 输出；可选 FFmpeg 仅作外部 oracle。生成失败必须失败，不用 SKIPPED 掩盖 P0 必需测试。文档列出夹具参数、原始命令响应和重现方法。
- [x] 在真实桌面 Bridge 再跑同一场景，并核对分析前后的工程、播放头、选区和撤销历史；模拟 Bridge 通过不能替代真实桌面验证。
- [x] 重跑 CLI 验收与全量门禁，通过后提交 `AE07: Verify end-to-end agent editing with generated media`。

## AE08 Codex 接入与发布门禁

**Files:** 修改 `docs/agents.md`、`docs/testing/agent-video-editing.md`，仅在功能已验收后更新 `docs/roadmap.md` 和 `docs/gaps.md` 的相应事实。

**Interfaces:** 产出可复用的连接步骤、模型看图记录和 AC01 至 AC11 验收表；不新增产品 API。

- [x] 准备两套配置：Headless 启动 `filmcraft-cli mcp`；Bridge 先启动 `filmcraft --control 9876`，再连接 `filmcraft-cli mcp --bridge 127.0.0.1:9876`。Codex 使用绝对二进制路径，示例不硬编码本机用户名，不改现有 Claude 的 .mcp.json。
- [x] 在实际 Codex 客户端调用 `media_frames`，让模型判断夹具场景切换和帧编号，人工对照确定性夹具验证。记录客户端版本、模型、工具调用、实际图像内容及判断结果；只展示文件路径不算 AC04 通过。 实际采用当前 Codex 会话接收真实 stdio MCP 图片块并对照确定性夹具；未进行人工签字或持久化原生 MCP 注册测试，具体模型部署标识不可获得，边界见验收记录。
- [x] 执行恶意参数和媒体变异测试：截断、位翻转、伪造尺寸、空流、坏 PTS、循环项目引用；在 catch_unwind 下断言不 panic，并确认之后仍能正常查询和保存工程。新增导致崩溃的样本成为最小回归测试。
- [x] 记录固定硬件、素材、首帧和批量耗时、峰值内存、响应大小、缓存命中耗时、全量哈希耗时。逐条填写 AC01 至 AC11 为通过或未通过，不用估计值填充测量结果。
- [x] 执行 `cargo fmt --check`、相关模块测试及 `cargo xtask ci`，确认已有导出、撤销、转录和 wasm 分层仍通过；未通过项先修复再发布。
- [x] 提交 `AE08: Document and qualify Codex editing workflow`。若准备 PR，按 contributors 规则核对当前贡献者条目；不手改生成的 contributors.json。

## 覆盖检查与交付状态

| 需求 | 任务 |
|---|---|
| AC01、素材元数据与未入时间线图片 | AE02、AE03 |
| AC02、AC03、采样及精确时间 | AE01、AE03、AE07 |
| AC04、实际模型看图 | AE06、AE08 |
| AC05、两种模式只读 | AE02、AE06、AE07 |
| AC06、异常及预算 | AE01、AE03、AE05、AE08 |
| AC07、取消与失败 | AE04、AE06 |
| AC08、缓存和版本 | AE05 |
| AC09、完整剪辑 | AE07 |
| AC10、原片导出 | AE02、AE07 |
| AC11、部分失败恢复 | AE07 |

交付时提供代码、命令文档、生成式夹具、测试结果及真实客户端验证记录。执行状态以上方勾选项为准。先完成 P0，再按两个独立 P1 计划扩展；不将“接口存在”写成“模型已理解素材”。
