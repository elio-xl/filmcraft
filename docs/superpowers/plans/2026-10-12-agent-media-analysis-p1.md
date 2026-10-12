# FilmCraft 低清视频与素材分析 P1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在已验收的图片分析基础上增加低清短视频、镜头列表、源转录与可追溯的语义检索。

**Architecture:** 复用 P0 分析队列、素材版本、原片读取和 MCP 结果适配。短视频是独立分析产物，镜头和转录均使用源时间；模型生成的摘要作为带来源的可丢弃索引保存，不混入确定性的媒体事实。

**Tech Stack:** 现有 Rust 编解码、export、scene detector、speech、serde、rmcp，以及 P0 的哈希和缓存能力。

**Spec:** [需求文档](../../requirements/agent-video-editing.md)，前置为 [P0 计划](2026-10-12-agent-video-editing.md) AC01 至 AC11 验收完成。

## Global Constraints

- 继承 P0 的纯 Rust、Never crash、时间字符串、版本、只读、队列、许可证和每提交 `cargo xtask ci` 约束。
- 视频默认 10 秒，单次最长 30 秒，长边默认 960、最高 12 fps，单文件最大 24 MiB；保持原速，不给低帧率原片补帧。
- 视频能力未知或不支持时回退图片；文件返回成功不能替代模型实际消费验证。
- 不自动下载语音模型，不自动调用云模型，不将低帧率分析副本挂载为剪辑代理。

## Review Focus

1. 23.976fps 降帧率后音画错位：AP01 用时间标记音轨核对误差不超过一个输出视频帧。
2. 高熵内容超过 24 MiB：AP01 在编码过程中终止并清理部分输出，不能编码完无限大文件才检查。
3. 转录存在但无语音模型：AP03 仍应能读取已存内容，不能错误禁用查询。
4. 闪光被误认为镜头切换：AP02 明确检测结果是候选边界，测试闪光与硬切的差异。
5. 摘要来自旧版素材或未知区间：AP04 拒绝检索返回过期或越界记录。

## 文件与公共接口

新增 engine `media_analysis/preview.rs`、`shots.rs`、`transcripts.rs`、`summaries.rs` 及各自测试；修改同目录 `mod.rs`、`jobs.rs`、`cache.rs` 和 automation `media_analysis.rs`、`server.rs`、`media_analysis_tests.rs`。工程命令注册仍通过 P0 `commands()`。

新增入口分别为 `preview::start(session: &mut Session, params: &Value) -> crate::Result<Value>`、`shots::start(session: &mut Session, params: &Value) -> crate::Result<Value>`、`transcripts::start(session: &mut Session, params: &Value) -> crate::Result<Value>`、`summaries::set(session: &mut Session, params: &Value) -> crate::Result<Value>`、`summaries::search(session: &mut Session, params: &Value) -> crate::Result<Value>`。需要读取源内容及校验版本的操作均进入队列，返回 `{job, request_id}`，复用 `media.analysis.result`。

## AP01 低清短视频与回退

**Files:** 新增 `preview.rs`、`preview_tests.rs`；修改分析任务与缓存、automation 适配及 `docs/testing/agent-video-editing.md`。

**Interfaces:** 引擎 `media.analysis.preview` 对应 MCP `media_preview_clip`，参数含 `item, expected_version, start_ticks, end_ticks, max_side=960, max_fps=12, audio=true`；结果含产物 ID、源区间、输出时间基、帧率、音轨状态、大小、MIME 与受控资源标识。

- [ ] 写 `preview_preserves_duration_and_sync`，分数帧率源含时间标记音轨，断言原速、正确源偏移、音画差不超过一输出帧；写 `preview_limits_are_enforced`，30 秒允许、超过拒绝、达到 24 MiB 时返回预算错误且无残缺产物。
- [ ] 运行 `cargo test -p filmcraft-engine preview_tests` 确认失败；使用现有导出管线和只供任务使用的临时工程快照编码 H.264 MP4，不改变用户工程。生成分辨率保持比例，受 960 默认值和 P0 1920 长边上限约束，帧率取源帧率与请求上限较小值。
- [ ] 增加按产物 ID 读取的受限资源接口，禁止任意路径读取；有效期随缓存管理，取消和磁盘空间不足清理临时文件。视频缓存总额 512 MiB，最多 16 份；使用中产物不淘汰。
- [ ] 客户端文档默认采用 P0 图片回退；仅对已验证的视频客户端启用视频路径。写 `video_unsupported_falls_back_to_frames`，对未知能力输入返回同源区间的连续帧调用方案，并实际执行图片请求。
- [ ] 运行 engine/automation 对应测试、真实客户端 AC12、`cargo xtask ci`，提交 `AP01: Add bounded analysis video previews`。

## AP02 只读镜头列表

**Files:** 新增 `shots.rs`、`shots_tests.rs`；复用 `crates/render/src/scene.rs`，修改 MCP 工具注册。

**Interfaces:** 引擎 `media.analysis.shots` 对应 `media_shots`。接受 `item, expected_version, start_ticks, end_ticks, sensitivity=50, min_shot_frames=6`；返回候选镜头 `{start_ticks,end_ticks,representative_ticks}`、检测参数和覆盖范围。

- [ ] 写 `source_shots_do_not_edit_project`，输入包含两次明确硬切，断言候选区间覆盖请求范围、顺序无重叠、工程历史不变。写 `flash_does_not_claim_semantic_scene_change`，保持敏感度和最短镜头参数可追溯，结果不包含未经模型判断的场景名称。
- [ ] 运行 `cargo test -p filmcraft-engine shots_tests` 确认失败；提取现有检测算法的只读使用路径，不能调用默认会 applyCuts 的时间线命令。按实际源展示时间输出边界，代表帧取区间中点实际命中的帧。
- [ ] 限制单次检测源区间为 10 分钟、最多 1000 个候选镜头；超限返回未覆盖源区间，任务可取消。对坏帧返回部分结果及缺失区间，不跨缺口推断无切换。
- [ ] 运行镜头和现有场景检测回归、`cargo xtask ci`，提交 `AP02: Expose read-only source shot candidates`。

## AP03 源转录和音频分析

**Files:** 新增 `transcripts.rs`、`transcripts_tests.rs`；修改分析注册与 MCP 工具；复用 `crates/engine/src/transcript.rs`、`crates/audio-dsp/src/lib.rs` 暴露的已有能力。

**Interfaces:** `media.analysis.transcript` 对应 `media_transcript`，按源区间读取已存词级转录。另注册 `media.analysis.audio`，通过 command_run 查询静音区间及可用的响度；节奏未实现时明确 unavailable，不用音量峰值冒充节拍。

- [ ] 写 `stored_transcript_works_without_whisper`，无 whisper 构建仍可读已存词句；无转录时返回 unavailable 及可执行的生成/导入命令。写 `subclip_words_use_root_time`，返回母素材时间、保留与区间相交的完整词边界并标明范围交叠。
- [ ] 写 `audio_only_media_can_be_analysed`，纯音频可获得确定性生成的静音区间；静音检测输入含阈值 `threshold_db=-40`、最短持续 `min_duration_ticks="127008000000"`，边界保持采样级精度。
- [ ] 运行 `cargo test -p filmcraft-engine transcripts_tests` 确认失败；所有长音频扫描进入队列，每次最长 10 分钟，受限分块读取；生成转录仍沿用现有可用性检查。把同步识别移入可取消任务时保留既有词级数据契约。
- [ ] 运行默认无 whisper 测试和安装相应模型后的可选集成测试，明确两者结果；执行 `cargo xtask ci`，提交 `AP03: Query source transcripts and audio analysis`。

## AP04 可追溯摘要与主题检索

**Files:** 新增 `summaries.rs`、`summaries_tests.rs`；修改缓存、命令文档及 MCP 适配。

**Interfaces:** `media.analysis.summary.set` 接受 `item, source_version, start_ticks, end_ticks, summary, tags, evidence_frame_ids, model_label`；`media.analysis.summary.search` 接受 `query, limit=20`。复用 command_run，不另加通用模型代理。摘要明确标记 `origin="model"`。

- [ ] 写 `summary_requires_current_evidence`：版本不匹配、区间越界、未属于该素材版本的证据帧均被拒绝；图片内出现指令时仅作为素材内容，不能触发文件操作或改变工具权限。
- [ ] 写 `search_omits_stale_summaries`，替换源内容后旧记录不再参与检索；每次结果上限 100，单条摘要最多 4096 字符、最多 32 个标签，每素材最多 1000 条。
- [ ] 运行 `cargo test -p filmcraft-engine summaries_tests` 确认失败；P1 使用规范化文字和标签匹配，固定排序规则为命中标签数、文本匹配、源时间，暂不引入向量数据库。没有匹配时返回空结果，不生成猜测摘要。
- [ ] 缓存保存证据与生成来源，清理及失效继承 P0；更新端到端场景并运行 `cargo xtask ci`，提交 `AP04: Index grounded media summaries for selection`。

## 阶段验收

AP01、AP02、AP03 可在 P0 公共接口稳定后分别实施，AP04 依赖 P0 帧证据，可结合 AP02/AP03 结果增强。共享注册文件按顺序合并。完成低清视频 AC12，并对每项新增命令执行异常参数与只读测试，才标记该阶段交付；模型音频/视频消费结果需单独记录，不能由编码成功推定。
