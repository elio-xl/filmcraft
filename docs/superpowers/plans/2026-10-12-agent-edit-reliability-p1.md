# FilmCraft 自动剪辑执行可靠性 P1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让智能体对可撤销的工程编辑进行预检、带版本的整体提交、整体撤销和安全重试。

**Architecture:** 在现有命令系统之外增加显式 opt-in 的编辑方案入口，限定可参与事务的命令。事务在隔离的工程状态中执行，检查项目与素材版本后一次提交；文件导出和网络等外部副作用继续走独立作业。

**Tech Stack:** 现有 Rust Session、Arc<Project>、撤销历史、serde_json、P0 素材版本与请求标识。

**Spec:** [需求文档](../../requirements/agent-video-editing.md)，依赖 [P0 计划](2026-10-12-agent-video-editing.md) 的稳定命令、版本和验证流程，不依赖 P1 原生视频输入。

## Global Constraints

- 不改变 `command_batch` 的现有逐条执行、逐条撤销行为。
- 精确时间继续使用 Tick；新增 JSON 的时间与大整数 revision 使用十进制字符串。
- 事务仅包含有明确隔离实现的工程编辑；禁止文件读写、导出、模型下载、后台任务启动及 UI 操作进入事务。
- 不把创建临时 Session 后直接调用任意现有命令视为天然隔离，必须阻止其访问外部服务和共享可变状态。
- 不使用 panic/unwrap 处理正常错误；每次提交执行 `cargo xtask ci`；源媒体不被改写。

## Review Focus

1. 用户在预检后编辑工程：AR02 必须拒绝过期 revision，保留用户修改。
2. 修改过的原片仍使用旧选片方案：AR01、AR02 检查期望内容版本，不能仅比较路径。
3. 命令内部产生隐藏的文件或后台副作用：AR01 采用默认拒绝的事务允许表并测试嵌套命令。
4. 提交成功但响应丢失：AR03 相同请求不能再次编辑；不同参数复用同 ID 必须报冲突。
5. 第三步失败或 panic：AR02 不留下前两步的工程、历史、事件或 revision 变化。

## 文件与接口

新增 `crates/engine/src/edit_plan/mod.rs`、`validation.rs`、`transaction.rs`、`dedup.rs`、`tests.rs`；修改 engine `commands.rs`、`lib.rs`。新增 `crates/automation/src/edit_plan.rs` 和 `edit_plan_tests.rs`，修改 automation 注册和文档。

新增命令 `edit.plan.validate`、`edit.plan.apply`，函数分别为 `edit_plan::validate(session: &mut Session, params: &Value) -> crate::Result<Value>` 和 `edit_plan::apply(session: &mut Session, params: &Value) -> crate::Result<Value>`，MCP 可通过 command_run 使用。两个入口均返回 `{job, request_id}`，版本哈希及隔离计算进入有界后台队列；新增 `edit.plan.result` 对应 `edit_plan::result(session: &Session, params: &Value) -> crate::Result<Value>` 读取结果，现有 jobs.list/cancel 支持这些任务。提交仍回到 Session 的短临界区执行。

公共输入：`request_id`、`expected_revision`、`source_versions` 和 `steps: [{id, params, reason}]`。每份计划最多 100 步、JSON 最多 1 MiB；reason 最多 512 字符。validate 返回每步校验结果、受影响实体和工程差异摘要；apply 返回是否提交、revision 前后值和单次撤销标签。

首批允许表固定为 `timeline.trim`（显式 clip）、`timeline.move`（显式 moves 中的 clip、track、time）、`edit.clear`（显式 clips）、`captions.setText`（显式 caption）和 `graphics.set`（显式 clip、layer、time，禁止 merge/begin）。禁止按当前选区或播放头补默认目标。事务适配器将字符串时间精确解析为现有命令所用整数；裁剪 delta 允许合法负值，源区间仍不得为负或倒置。AE07 中的导入、保存、导出保持独立。

## AR01 方案解析和预检

**Files:** 新增 `edit_plan/mod.rs`、`validation.rs`、`tests.rs`；修改命令注册及 `docs/control-protocol.md`。

**Interfaces:** `validation::validate(session: &Session, params: &Value) -> crate::Result<Value>`；内部 `validation::allowed_command(id: &str) -> bool` 默认 false。允许表记录确切 command ID、所需显式目标键、可变状态和隔离执行器，不能仅按前缀放行。

- [ ] 将上述五个确切命令 ID 和必需目标字段加入固定允许表测试。写 `edit_plan_rejects_external_effects`，对 file.exportMedia、file.import、下载、任务启动及嵌套批量调用断言拒绝。
- [ ] 写 `edit_plan_requires_explicit_targets` 和 `edit_plan_rejects_stale_sources`：无目标、缺失对象、旧内容版本、零或负区间及超预算步骤均返回带步骤序号的错误。预检前后工程、选区、历史和 revision 完全相同。
- [ ] 运行 `cargo test -p filmcraft-engine edit_plan::tests` 确认失败；实现严格解析和只读预检，对源版本验证采用 P0 的受限哈希任务，不把长 IO 放在 UI 线程中。validate 的输出不是锁定凭证，apply 必须再次检查版本。
- [ ] 更新允许表、限制及错误契约文档；重跑测试和 `cargo xtask ci`，提交 `AR01: Validate bounded editing plans`。

## AR02 隔离事务与整体撤销

**Files:** 新增 `edit_plan/transaction.rs`；修改 Session、edit_plan 测试，必要时为允许表命令提取纯编辑执行器，保持原有单命令行为。

**Interfaces:** `transaction::apply(session: &mut Session, params: &Value) -> crate::Result<Value>`。预检后对工程快照执行纯编辑步骤；提交阶段在同一 Session 临界区检查 expected_revision、替换工程并写入一次撤销记录。

- [ ] 写 `plan_commits_one_undo_step`：三步成功编辑只增加一个历史条目，撤销后工程恢复；重做得到同一工程结果。写 `plan_failure_leaves_live_state_unchanged`，中间错误和注入 panic 均不改变工程、revision、事件、历史及选区。
- [ ] 写 `concurrent_user_edit_wins`：预检后用户修改，apply 拒绝旧 revision。写 `source_replaced_before_apply_is_rejected`：提交前重新验证源版本，变化时不提交；在最终短临界区复核验证过的源句柄和项目引用，变化时重新验证而非继续应用。
- [ ] 运行 `cargo test -p filmcraft-engine edit_plan::tests` 确认失败；提取允许操作的纯函数并在沙盒编辑状态执行，禁止访问真实 Services、后台队列和全局事件。项目更换、关闭或取消时丢弃候选结果。
- [ ] 提交一次工程变更事件和一次撤销记录，按现有恢复日志约定持久化；禁止直接改 undo 向量绕开恢复机制。新增图形修改沿用现有实例同步语义，并在测试中覆盖共享实例。
- [ ] 运行现有编辑、撤销、恢复日志回归及 `cargo xtask ci`，提交 `AR02: Apply editing plans as recoverable transactions`。

## AR03 去重和端到端失败恢复

**Files:** 新增 `edit_plan/dedup.rs`、automation `edit_plan.rs`、`edit_plan_tests.rs`；修改文档及 P0 全流程测试。

**Interfaces:** `dedup::lookup(session: &Session, request_id: &str, canonical_params: &Value) -> crate::Result<Option<Value>>`；`dedup::record(session: &mut Session, request_id: &str, canonical_params: &Value, result: &Value) -> crate::Result<()>`。以完整规范化参数哈希比较请求，不能只比较步骤数量。

- [ ] 写 `lost_reply_does_not_apply_twice`：模拟提交成功后网络断开，相同 ID 和参数重试返回原结果，revision 和历史不再次增加。写 `request_id_conflict_is_rejected`：相同 ID 不同参数被拒绝。
- [ ] 写 `dedup_capacity_does_not_forget_commits`：每会话最多 1024 个已提交请求、总计最多 8 MiB；达到容量时拒绝新的事务，不能淘汰记录后误重放旧请求。任务尚在执行时，同一请求返回原任务状态。
- [ ] 运行 `cargo test -p filmcraft-engine edit_plan::tests` 和 `cargo test -p filmcraft-automation edit_plan` 确认失败；在同一提交临界区写入成功结果和去重记录，记录失败不得留下“工程已提交但请求未知”的状态。
- [ ] 去重仅承诺当前会话；服务重启后旧会话标识失效，旧请求明确返回 SESSION_EXPIRED，客户端重新读取工程再决定后续动作。不得把会话内缓存宣传为跨崩溃 exactly-once。
- [ ] 将 AE07 剪辑流程中的合适编辑片段迁移到可选事务调用，对比事务和旧 command_batch 的失败行为；重跑真实 Bridge 重连验证及 `cargo xtask ci`，提交 `AR03: Deduplicate transactional editing requests`。

## 阶段验收

AR01 → AR02 → AR03 顺序执行。交付预检报告、允许命令表、版本冲突及断线恢复记录。只有纯工程编辑具有整体撤销语义，导出与外部文件副作用保持独立。事务增加后，P0 的既有接口和完整剪辑验收仍须通过。
