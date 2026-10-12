# MCP 自动剪辑 P1 验收步骤

适用范围：AP01–AP04 素材理解、AR01–AR03 编辑可靠性。P0 原有接口继续可用。
实现分支：`codex/agent-video-editing-p1`。以下命令通过 MCP `command_run` 或原生控制通道执行；
新增时间与 revision 使用十进制字符串，Tick 每秒 254016000000。

## 1. 构建和自动化回归

```sh
cargo test -p filmcraft-engine media_analysis
cargo test -p filmcraft-engine edit_plan
cargo test -p filmcraft-automation edit_plan
cargo test -p filmcraft-cli --test agent_editing
cargo xtask ci
```

Bridge 测试需要本机回环端口权限。真实 stdio 测试生成原创素材到 `target/fixtures/agent-editing/`，
输出 MCP 交互记录和预览测量结果；不需要下载外部素材、Whisper 权重或调用云端模型。

## 2. 素材分析

1. 导入一段包含可辨认图形、镜头变化、音频的素材，记录工程、选区、revision 和历史。
2. `media_inspect` 获取 item 与 source_version；`media_preview_clip` 请求明确的源时间区间。
   不声明视频能力时应得到实际 PNG image blocks；每张含请求时间和实际源时间，不能把视频链接当成已看见画面。
3. 在已另行确认支持视频输入的客户端中声明 `video_capability:"verified"`：读取返回的受控
   `filmcraft://analysis/...` MP4 资源。默认最多 10 秒，单次最多 30 秒、1920 像素边长、12 fps、24 MiB。
   验证区间、比例、速度、音轨及文件实际可解码。编码成功不等于模型已接收视频。
4. `media_shots` 查询切点候选；检查源 Tick、代表帧时间、覆盖/缺失区间及 candidate_only 标识。
   闪光可能成为候选；不应出现引擎自称理解场景的语义标签。
5. `media_transcript` 查询已存转录的重叠词及时间。无转录时应明确 unavailable 并给出显式生成入口；
   不自动下载模型或推断词句。`media.analysis.audio` 查询静音区间和响度；无节奏分析时明确 unavailable。
6. 用实际生成的帧证据 ID 调用 `media.analysis.summary.set`，提供 source_version、区间、模型标识、摘要、标签。
   伪造证据、过期证据或版本不匹配必须拒绝。`media.analysis.summary.search` 返回来源、证据和原始区间；
   无匹配应返回空集。改写原片后旧摘要不应作为有效匹配返回。
7. 再次比较工程、选区、revision、历史：上述分析均不改变编辑状态。

## 3. 编辑预检和整体提交

1. `edit.plan.session {}` 获取 session_id 与精确 revision；读取显式 clip、track、caption 等目标。
2. 组装计划，最多 100 步、1 MiB：

   ```json
   {
     "session_id": "从 edit.plan.session 获取",
     "request_id": "本会话内唯一的请求 ID",
     "expected_revision": "当前 revision",
     "source_versions": [{"item": 5, "source_version": "从素材分析获取"}],
     "steps": [{
       "id": "timeline.trim",
       "params": {"clip": 12, "edge": "out", "delta": "-254016000000"},
       "reason": "缩短镜头尾部一秒"
     }]
   }
   ```

   数字仅为示例，必须替换为当前工程对象。首批只允许显式目标的 trim、move、clear、captions.setText、graphics.set。
   图形和字幕自身没有源媒体时可使用空 source_versions；受影响的媒体和联动片段需要内容版本。
3. `edit.plan.validate` 返回 job；用 `edit.plan.result {job,session_id}` 轮询。
   成功应为 committed:false，有每步影响对象和工程差异摘要，工程完全未变。
4. 对同一计划使用新的 request_id 调用 `edit.plan.apply`；成功应为 committed:true，revision 增加一次，
   历史只增加 `Apply Editing Plan` 一项。撤销恢复整份工程，重做恢复提交结果。
5. MCP 可在新请求中省略 session_id/request_id，由服务补齐；加 `wait:true` 可等待最终结果。
   重试时必须携带首次返回的两个标识和原参数，不能重新生成身份。

## 4. 冲突、失败与恢复

- 预检差异：将 A 覆盖到 B 中部或完全覆盖 B，报告必须显示 B 被截短、分裂后新增的 ID 或被删除的 ID；复制操作须列出新副本。
- 波纹修剪和插入：报告应包含实际受影响的后续片段、字幕和标记的前后时间（沿用原命令语义）。请求超出可用修剪范围时，以报告中的实际边界验收。
- 字幕修改及共享图形：报告应列出字幕和所有被同步的实例；内容变化标为 `content_changed`。仅预检时工程及历史保持不变。
- 大方案：每步差异共用 256 条 / 224 KiB 配额，整案差异另有相同配额；省略数量必须明确。文字最多 256 个 Unicode 字符并标注截断；效果/关键帧只报告内容变化，不返回完整属性树。存在省略时缩小方案再预检。
- 预检后用户编辑：旧 revision 的 apply 必须拒绝，并保留用户修改。
- 候选结果完成、提交前替换源文件或工程：拒绝提交。取消作业也不得留下部分工程变更。
- 第二步指向不存在的对象：第一步也不能落地；内部测试注入中途 panic，验证相同不变性。
- 修改共享图形实例：其他实例按原有语义同步，并包含在同一次撤销内。
- 模拟提交成功但响应丢失：Bridge 重连发送相同身份和参数，只返回原 job/result，不增加 revision/历史。
- 相同 ID 改任意参数（即使步骤数量相同）：返回 REQUEST_ID_CONFLICT。
- 淘汰普通结果页后重试已提交请求：仍能取得原结果。提交记录满 1024 条或 8 MiB 时拒绝新事务，旧记录保留。
- 重启服务后提交/轮询旧 session_id：返回 SESSION_EXPIRED，先重读工程，不能盲目重放。
- 已提交工程经现有恢复快照恢复；去重回执不跨崩溃持久化，不承诺跨重启 exactly-once。
- 对同样的失败步骤调用旧 command_batch：此前成功的步骤保留，验证旧接口语义没有改变。

## 5. 验收边界

- 当前 Codex 会话未原生挂载 FilmCraft MCP；已验证 stdio 输出与 PNG 图片转交，不能据此宣称原生视频/音频输入已通过模型验收。
- 本机没有 Whisper 权重，真实 Whisper 推理未验收；显式能力错误、已有转录查询及音频分析有自动化覆盖。
- 镜头检测按名义帧率采样并记录实际 PTS，极短 VFR 镜头可能漏检。
- 摘要与保留的帧证据同寿命；缓存淘汰后需重新取证。索引最多 1000 条/item、4096 条/session、32 MiB，单次搜索最多涉及 256 个源。
- 文件事务需要宿主提供低成本变化标记，最终最多检查 256 个文件/目录；不支持该能力的宿主明确拒绝。
  元数据检查可检测普通文件变化，但不构成操作系统文件锁。
