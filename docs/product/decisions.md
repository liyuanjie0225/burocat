# KuroCat 产品决策记录（Decision Log）

> 只记「已定的方向性决策」，不重复路线图 / PRD 的内容。每条含背景、决策、依据；后续决策倒序追加在最上。
> 关联文档：[agent-era-roadmap.md](./agent-era-roadmap.md) · [KuroCat_PRD_V5.2_digital-coworker.md](./KuroCat_PRD_V5.2_digital-coworker.md)

---

## 2026-11-26 · 「等确认」只信真事件：钩子已装就不再猜（D9）

**背景**：PermissionRequest 实弹验证 2/2 触发当天，胶囊仍对无卡片的会话显示「等确认 32s」。根因：另一 guarded 会话提交 prompt 后进入长思考（首个工具前静默 >20s），前端「静默即疑似等确认」的兜底猜测**无条件生效**，把思考时长当成了卡片等待时长在走秒。

### D9 · 静默猜测只在钩子缺席时兜底；钩子已装则只信任真实事件

- 猜测（`agentTaskProbablyWaiting`）的存在理由是「真卡片可能报不上来」（自动安装关闭 / 安装失败）。钩子已装时真卡片必然自报 PermissionRequest，猜测失去正当场景，只剩误报（长思考、被 Esc 中断且不发钩子的回合）
- 前端以模块态 `hooksInstalled` 关闸：`get_agent_hook_info` 播种，后端 `sync` 在安装状态每次实际变化时以 **`kuro-agent-hooks`** 事件推送（设置开关、dev/prod exe 切换实时生效，无需重载宠物窗口）
- 胶囊与具身演出共用同一判定，同步翻转；guarded 20s / 普通 45s 阈值仅在钩子缺席时使用，代码保留

---

## 2026-11-26 · 思考态：区分「模型思考」与「工具在跑」（D8）

**背景**：R2 拟宠化把所有 running 任务一律当 working（埋头敲键盘），模型生成间隙（prompt 后首次工具前、工具调用之间）与工具执行看起来完全一样，缺少「思考的感觉」。上游 companion 协议已有 thinking 状态可参照。

### D8 · 思考态 = 静默 ≥6s 且无进行中工具；PreToolUse 以 tool_start 上报

- 判定规则收敛为一条（`agentStatus.ts` 的 `agentTaskThinking`，pill 与演出共用）：**距上次事件 ≥6s、`pendingTool` 为空、任务仍活跃**。6s 以内的工具间间隙保持敲击节奏
- 仅靠静默判定会把长命令（Bash 构建，高频场景）误判为思考，因此 **`ZCODE_EVENTS` 增加 `PreToolUse`**，映射为独立事件 `tool_start`：任务新增 `pendingTool` 字段，工具在跑就继续敲击、永不入思考态；`tool_start` 不计步数、不改 `last_tool`（步数仍只数完成调用）
- 表演复用好奇凝视（瞳孔放大/竖耳/歪头 + 💭 气泡 + 胶囊 💭 前缀），零新动画；与等待态（抬头看你）姿态区分。升级「托腮沉思」姿态留作后续可选
- 90s 哈欠、3min 淡出、用户优先仲裁、隐私优先全部不变，思考态只与之叠加

---

## 2026-11-26 · 用户工具栈修正优先级（D5–D7，推翻 D2）

**背景**：liyuanjie 明确反馈：豆包、Claude Code 都不是常用软件，本机 coding agent 只有 **ZCode 和 Cursor**，未来可能装 **Codex CLI**。路线图 A3 是市场层面假设（Claude Code 保有量大），不能覆盖「作者本人即第一用户」的自用优先阶段。路线图 §4 预设规则「若 A3 不成立，R1 优先级下降、R2/R6 上升」就此触发。

### D5 · R1a-lite（Claude Code 适配器）挂起 —— 推翻 D2

- 自用价值为零（不用 Claude Code）
- Claude Code 用户的接入层已由上游 companion 覆盖；传播侧若要给他们完整演出，走 D3 的可选桥接（2–3 天），不必自己写适配器
- 待真实需求出现再重启

### D6 · R6 MCP 表达工具包升为 v5.2 剩余工作第一优先

ZCode 与 Cursor 都支持 MCP，一条路径同时服务自用的两大工具：ZCode 补上「agent 主动驱动猫表达」，Cursor 获得唯一的接入路径（无 hooks，只有 MCP + 终端命令）。估时 ~0.5 周。

### D7 · Codex 适配调研推迟到实际安装后

现在不空转；届时补做「Agent hooks/MCP 能力矩阵」（路线图 §4 遗留项）以 Codex 为先，Cursor 的 MCP 路径同批验证。Claude Code 部分直接引用上游实测结论。

---

## 2026-11-26 · 上游 v5.1.2 撞车后的对齐策略（D1–D4）

**背景**：合并上游 v5.1.2（merge 8a8bb20）时发现 upstream 已内置简化版 AI Companion（`src/agentCompanion.ts` + `mcp.rs` 的 `POST /hook` 桥），与路线图 Now 期的 R1（多 Agent 接入）/ R2（拟宠化）部分撞车。分析见路线图 §5。liyuanjie 确认「上游很多功能我不需要」后，定下四条决策：

### D1 · 上游同步策略：从「全面跟随」改为「选择性同步」

- watch upstream releases，先判断变更性质再决定动作
- 纯 website / 营销类 release → **跳过**（v5.1.2 的 63 个变更文件中 ~90% 是网站内容）
- desktop `src` 有实质改动 → 全量合并，或仅 cherry-pick bugfix

### D2 · R1a-lite 升为 v5.2 下一步（替代原 R1a）

自家管线的 Claude Code 适配器。关键发现：上游 v5.1.2 注册的 Claude Code hooks 事件（UserPromptSubmit / PreToolUse / PostToolUseFailure / PermissionRequest / Stop + `CLAUDE_SESSION_ID`）与 `hook_payload_to_event` 已解析的事件**完全一致**——R1a 只差 `agent_hooks.rs` 增加 `~/.claude/settings.json` 注入目标（复用 read-merge-atomic 模式）。

**影响**：工期估算 2 周 → **3–5 天**。

### D3 · companion 桥接降级为可选路径

上游 AI Companion 的 `kuro-agent-event` → embodiment 桥接只作为可选路径，服务豆包等上游已覆盖客户端的用户；自家管线（D2 的 R1a-lite）为主路径。

### D4 · 上游 PR 策略

- 小修小补（bugfix、i18n 补齐）可以 PR 回上游，做开源好公民
- R2 拟宠化深度演出**不送上游**——这是 fork 的差异化命脉（Codex pets / agentpet 只有状态 widget，KuroCat 独有具身化演出）
- 择机与上游作者 qianyiai 沟通分工边界

---

## 待决事项（Open Items）

| 事项 | 状态 | 备注 |
|------|------|------|
| `feat/agent-status-zcode`（8 提交：R2 全量 + v5.1.2 合并 + 文档）推送 origin 备份 | **等确认** | push 属外发动作，liyuanjie 说推再推 |
| PRD 的 3 条阻塞开发待验证项（Claude Code Notification payload 字段 / Windows 配置路径 / SubagentStop 处理） | 待真机验证 | 见 PRD 对应章节 |
