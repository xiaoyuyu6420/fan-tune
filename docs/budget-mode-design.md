# Budget 模式设计草案 —— "恒温器"式风扇控制

> 目标：把风扇控制从「温度 → 转速」的静态映射，改成「温度预算 → 闭环调节」。
> 一句话原则：**温度不是要压低的东西，是要花的预算；预算内的温度免费，越线才付噪音。**

## 1. 问题定义

现有所有模式（Curve/Manual/Auto）本质是**开环映射**：

```
温度 --(曲线查表)--> 占空比
```

两个天生缺陷：
1. **不管结果**：83 °C 映射 90% 就写 90%，至于当时 55% 是否就够，它不知道也不关心；
2. **不抗漂移**：室温升降、滤网积灰、风扇老化后散热效率变了，曲线照旧执行，实际结果全变。

预算模式改成**闭环**：

```
温度预算 --(误差反馈)--> 占空比调节 --> 实测温度 --> 回到比较
```

稳态温度不再是曲线的副产品，而是**设计目标**：用刚好够的转速，把每个热源维持在它的预算之内。

## 2. 核心概念

| 概念 | 含义 | 配置字段 |
|---|---|---|
| `budget_c` | 越线阈值。温度高于它 = 必须加风 | 新增 |
| 合格带 | `[budget_c - hysteresis_c, budget_c]`，温度落在这里 = 什么都不做 | 复用 `hysteresis_c` 作为带宽 |
| 越线加风 | `temp > budget_c` → 目标 100%，由 `ramp_up_pct_s` 决定爬升速度 | 复用 |
| 富余退风 | `temp < budget_c - band` → 目标 0%，由 `ramp_down_pct_s` 决定下降速度 | 复用 |
| 紧急顶格 | `temp > budget_c + emergency_margin`（默认 8 °C）→ 绕过限速立即 100% | 新增常量 |

**为什么用双阈值（bang-bang + deadband）而不是 PID**：风扇系统有大滞后 + 非线性，PID 调参困难且容易振荡。双阈值 + 变化率限制在工程上更稳，语义也和苹果的解法一致——**让温度停在预算里，而不是追一个精确值**。

## 3. 决策层的接口（最小改动点）

`algo.rs::channel_target()` 的链条是：

```
组合源 → EMA → [决策] → 迟滞 → 限速 → spin-up → clamp
```

**只替换 [决策] 一段**：

```rust
// 现有
let curve_duty = eval_curve(&c.points, eff_temp);

// 新增：budget 决策产出"目标占空比"，下面的迟滞/限速/spin-up 原样复用
let target_duty = match c.mode {
    FanMode::Curve  => eval_curve(&c.points, eff_temp),
    FanMode::Budget => budget_decide(state, c, temp),   // ← 只有这一个新函数
    _ => unreachable!(),  // Manual/Auto 由 caller 分发，不进来
};

fn budget_decide(state: &ChannelState, c: &FanChannelConfig, temp: f32) -> f32 {
    let budget = c.budget_c;
    let band   = c.hysteresis_c.max(0.0);          // 带宽
    let hard   = budget + EMERGENCY_MARGIN_C;      // 8 °C
    if temp > hard          { 100.0 }              // 紧急：绕过限速（见 §4）
    else if temp > budget   { 100.0 }              // 越线：让 ramp_up 限速爬
    else if temp < budget - band { 0.0 }           // 富余：让 ramp_down 限速退
    else                    { state.last_duty }    // 带内：保持现状
}
```

关键性质：
- **输出是"趋向"而非"档位"**：给的 0/100 只是方向，真实速度由 `ramp_up_pct_s` / `ramp_down_pct_s` 控制——这两个字段从"曲线模式的防抖参数"升级为"预算模式的响应速度旋钮"，语义反而更直观；
- **迟滞链条需要特判**：`FanMode::Budget` 下，双阈值本身已构成保持带，曲线迟滞的"下降等 anchor-h"逻辑要跳过（否则两层保持带叠加，退风会卡住）；
- **spin-up 逻辑照旧**：从静止起步的踢一脚对预算模式同样必要。

## 4. 紧急路径（安全边界）

闭环是温和的，"越线 + 8 °C"必须有不受限速约束的暴力兜底：

- 复用 `ServerGuard` 的模式（latch + 迟滞释放）：越过 `budget + 8` 触发，回落到 `budget + 3` 释放；
- 触发期间 `duty = 100` 且**跳过 ramp 限速**（这是与普通越线的唯一区别）；
- 与现有 `SOURCE_FAIL → SAFE_DUTY 70%` 并存，互不替代：源失效走旧的，源有效但温度失控走这个。

## 5. 与 profile 的关系

| Profile | 对 Curve 的作用 | 对 Budget 的作用 |
|---|---|---|
| Gaming | 曲线按 +5 °C 评估 | 预算收紧 5 °C（更早介入），语义统一 |
| Server | 限速 + 迟滞 + EMA 封顶 | 天然适用（预算模式本来就稳）；仅保留 guard |

## 6. 配置文件（fantune.json）

```json
{
  "enabled": true,
  "name": "Fan #0 (机箱/冷排A)",
  "label": "前部进气",
  "mode": "Budget",          // 新增枚举值
  "budget_c": 62.0,          // 新增：预算阈值
  "hysteresis_c": 4.0,       // 复用为"合格带宽"
  "ramp_up_pct_s": 8.0,      // 越线后的爬升速度
  "ramp_down_pct_s": 1.5,    // 富余后的退风速度（慢！避免忽快忽慢）
  "source": "nvme:0",
  "sources": ["gpu:0", "nvme:0"],
  "combine": "Max"
}
```

兼容性纪律（照抄 config.rs 既有做法）：
- `mode: "Budget"` + `budget_c` 都是新增，旧配置不受影响；
- `budget_c` 的 serde default 设 0（0 = 未配置，engine 遇到则回退为 Curve 行为或返回 None，二选一，见 §9 决策点）；
- 必须加 legacy 加载测试（有 `legacy_config_loads_with_defaults` 的先例）。

## 7. UI 接入点（顺带收口的重复代码）

`ui.rs` 里 mode 分发散在多处（`582`、`839`、`1755`、`2857` 附近），GUI 自己还带一套控制循环。**建议先抽一个公共函数**：

```rust
// 建议：把"算目标占空比"从 GUI 和 daemon 各自的内联逻辑中抽出来
fn mode_target(engine: &mut Engine, i: usize, c: &FanChannelConfig, ...) -> Option<f32>
```

否则 Budget 会在 GUI/daemon 各实现一次——这正是 README 里"两个文件都要看"的重复实现问题的扩大版。

单选按钮（`ui.rs:2857`）加一项：`预算`；模式旁显示 `预算 62°C` 的输入框（NumberDrag）。

## 8. 分阶段实施

### 阶段 1：基础恒温器（本次）
- `FanMode::Budget` + `budget_decide()` + 紧急路径；
- 迟滞链条特判、限速复用；
- 配置字段 `budget_c` + legacy 测试；
- UI：单选 + 预算输入框；
- **验收**：给定一个稳定负载，温度收敛到 `[budget - band, budget]` 区间内不振荡，转速停在某一定值。

### 阶段 2：学习下探（"自适应最低转速"）
- 状态新增 `learned_floor: f32`；
- 温度在带内稳定 ≥ N 分钟 → 试探性 `-2%`；若温度越线 → 回退到试探前值并记为 `learned_floor`，试探间隔指数退避（最长 30 分钟一次）；
- 效果：系统自动收敛到"刚好够"的转速；**换季、积灰后自动重新学习**——这是曲线永远做不到的。
- **验收**：人为加负载（堵滤网/升温）后，学习值自动上移；撤掉后自动下探。

### 阶段 3：per-source 预算（多热源约束系统）
- 配置升级为每源独立预算（`{"key": "gpu:0", "budget_c": 85}`），通道误差取"余量最小者"：
  `error = max_i(temp_i - budget_i)`，最接近越线的源主导；
- 这是"动态追踪多个热源"的完整形态；
- **验收**：两个源预算不同时，接近预算的那个单独拉动转速，另一个不参与。

## 9. 待决策点

1. **`budget_c` 未配置时的行为**：回退 Curve（保守）vs 通道报错（显式）——倾向回退 Curve + UI 红字提示；
2. **预算默认值**：不给默认（用户必填）vs 按源前缀给预设表（`gpu:* → 85`, `nvme:* → 70`, `nct:* → 90`）——倾向 UI 提供预设按钮但不自动填；
3. **先在哪条通道试点**：建议拿"前部进气"或"机箱扇"开刀，GPU 冷排通道暂留 Curve 对照；
4. **阶段 2 是否同期做**：预算模式跑通后，学习下探是自然延伸，但会引入新的持续性状态（要写盘吗？建议只存内存，重启重学）。

## 10. 测试清单（algo.rs 纪律）

- `budget_holds_inside_band`：带内输入 → duty 不变；
- `budget_ramps_up_when_over`：越线 → 按 ramp_up 速率爬升；
- `budget_ramps_down_when_cool`：富余 → 按 ramp_down 速率退；
- `budget_no_oscillation_at_boundary`：温度在预算线上 ±1 °C 抖动 → duty 不振荡；
- `budget_emergency_bypasses_ramp`：超紧急线 → 立即 100（不等 ramp）；
- `budget_multi_sources_use_combine`：多源按 combine 组合后比较预算；
- `budget_skips_curve_hysteresis`：带内保持生效且不与曲线迟滞叠加；
- `config.rs`: `legacy_config_loads_with_defaults` 扩展（无 `budget_c` 的旧文件照常加载）。
