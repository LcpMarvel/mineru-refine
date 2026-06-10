# mineru-refine — 规格说明（SPEC v0.2）

> 工作名 `mineru-refine`，可改。
> 一个**独立应用**：对 MinerU 的解析结果做二次处理，输出"更接近原文结构"的同 schema 结果。
> 由一个 **LLM tool-use 循环**驱动：LLM 只观察内容、调用确定性 function 改结构，
> "是否合格"由**机器闸门**裁决，不由 LLM 自评。
>
> **技术栈**：Bun + TypeScript。LLM = DeepSeek `deepseek-v4-pro` **裸 API**（不用 SDK）。JSON 修复 = `safe-json-repair`。
> **已决策**：D1 允许加字 / D2 DeepSeek 原生 / D3 统一吃对象（见 §3）。

---

## 0. 一句话定位

**MinerU 输出的 linter / fixer。** content_list（item 对象数组）进，同 schema 出。
消费方（docfuse 及任何吃 MinerU 的人）**无感知**——它读到的仍是"一份 MinerU 结果"，
不知道中间有人动过。

---

## 1. 目标 / 非目标

### 目标
- 修 MinerU 面向 PDF 视觉解析产生的**结构 quirk**：伪 HEADING、跨页块被打断、
  巨型块塞多章节、页码/页眉混入正文、超链接/LaTeX 残留等。**只做削减/重组，不加字**（D4=(c)）。
- 输出与 MinerU **同 schema**，作为透明过滤器接入现有 pipeline。
- 用 DeepSeek v4-pro（便宜）跑，成本远低于 Claude。
- 每次运行可**机器验证**：`C_out ⊆ C_in`（无新增字符，未篡改可证）；异常数下降；幂等。

### 非目标
- **不做内容生成 / 改写 / 摘要 / 加字**（OCR 纠错、补图注本期不做，D4=(c)）。
- 不感知 docfuse 的业务模型（`ContentBlock`、章节树、制度2.0 等一概不依赖）。
- 不替代 MinerU 已有的解析；只在其输出上做后处理。
- 跨页表格/列表合并本期**只标记不处理**（D5）。

---

## 2. 契约（最重要，先钉死）

| 项 | 约定 |
|---|---|
| **输入** | content_list（MinerU item 对象数组，**内存对象**，非文件路径）+ 可选 `markdown` 字符串、图片访问器（只读，供上下文判断） |
| **输出** | `{ items, provenance, report }`：`items` 与输入**同 schema**；`provenance` 当前为空（D4=(c) 无加字）；`report` 是统计 |
| **schema 透明性** | `items` 字段集合/类型/取值与 MinerU 一致，**不掺任何非标字段**（内部稳定 ID 出口前剥除）；消费方读 `items` 即可，零改动 |
| **失败行为** | **fail-open**：任何异常 / 超时 / LLM 不可用 → **原样返回输入 items** + 大声 log。绝不搞崩上游 |
| **缓存** | key = `sha256(源文件) + refineLogicVersion + model + promptVersion`。**只用 SHA256 是错的**——逻辑/prompt/模型一变旧结果会错误命中。命中直接返回，跳过 LLM loop |
| **幂等** | 对已清洗过的输入再跑一次，应收敛为 no-op（异常清单已空） |

> ⚠️ 透明性是对**文本语义 + schema** 的承诺，不是对**几何字段**（`bbox` / `page_idx`）的承诺。
> 见 §6 几何派生规则——下游 `anchor_inject` 的高亮定位依赖这两个字段。

---

## 3. 决策记录

- **D1｜允许加字** ⛔ **撤回**（被 D4 取代）。v0.1 设想的 `fixOcr`/`synthesizeCaption` 不做。
- **D2｜DeepSeek 裸 API** ✅。`POST https://api.deepseek.com/chat/completions`、`model=deepseek-v4-pro`、
  `Authorization: Bearer`，**不用任何 SDK**（Bun fetch）。key=`RAGENT_DEEPSEEK_APIKEY`。thinking 默认 **disabled**（详见 §11）。
- **D3｜统一吃对象** ✅。core lib 的 API 收/发**内存对象数组**，不读写文件。
  跨进程/跨语言传输（docfuse 是 Python）由薄 transport 层用 JSON 包一层（§12）。
- **D4｜加字策略** ✅ **(c) 先上纯削减**。不加字，op 集只做削减/重组（§8 削减集）。
  好处：保真**完全可证**（§5 即 `C_out ⊆ C_in`，无 `C_add`），最干净。
  加字（OCR 纠错/补图注）作为后续 feature，真有需求时再走视觉模型（Qwen-VL）路线，届时重开 provenance（§5a 暂留备用）。
- **D5｜表格/列表跨页** ✅ **探测器只标记、不处理**。MinerU 高频把跨页表格拆成两个 `table`、跨页列表拆断；
  探测器照常标进 worklist 供观测/统计，但**无对应 op**，LLM 对这类疑点只能 `dismiss`。`mergeTable` 等留作后续。

---

## 4. 数据模型（MinerU content_list item，真实字段）

```jsonc
{
  "type": "text" | "header" | "table" | "list" | "page_number" | "image",
  "text": "正文文本（text/header）",
  "text_level": 1,                       // 仅 text 且为标题时存在
  "table_body": "<table>...</table>",     // 仅 table，HTML
  "table_caption": ["表1 ..."],            // 仅 table，数组
  "list_items": ["第一项", "第二项"],       // 仅 list，数组
  "img_path": "images/xxx.jpg",            // 仅 image
  "page_idx": 0,                            // 所在页（0-based）
  "bbox": [x0, y0, x1, y1]                  // 页面内坐标
}
```

TS 类型 `MineruItem` 直接镜像此结构。输出 item 必须保持同样的字段形态。

### 4a. 内部稳定 ID（关键，防索引错位）

**绝不用 array index 跨 op 寻址**——一个 `merge`/`split`/`drop`/`reorder` 就会让后续所有下标错位，
LLM 手里的旧 index 会指向错的块。处理流程内每个 item 带一个**内部稳定 ID**：

- 入口给每个 item 分配 `id`（如 `it_0001`）。`merge` 产一个新 ID；`split` 产两个新 ID；
  `demote/promote/strip/fixOcr` 继承原 ID；`reorder` 不变；`drop` 移除。
- worklist、provenance、所有 op 参数、LLM 看到的引用**一律用 ID**，不用 index。
- `id` 是**内部字段**，**出口返回前剥除**，保 §2 schema 透明。
- `fixOcr` 在同一 item 内多处改字时，offset 右到左应用或每次重算，防自身偏移。

---

## 5. 保真不变式（文本）— 本应用的立身之本

"内容字符"= `text` + `list_items` 拼接 + `table_caption` 拼接，**仅计非空白字符**
（空白符——空格/换行/制表——归入可削减白名单，merge 吃掉块尾换行不算违规）。
`table_body` **不纳入字符比对**：没有 op 改它，改为更强的约束——**未被 drop 的 item，其 `table_body` 逐字节相等**。`img_path` 不计。

设输入内容字符多重集 `C_in`、输出 `C_out`，**当前模式（D4=(c) 纯削减）硬断言**：

> **`C_out ⊆ C_in`** —— 输出不得包含任何输入里没有的非空白内容字符。只许削减/重组，不许新增。

- 所有 op（merge/split/reorder/demote/promote/drop/strip）：`C_out ⊆ C_in`。
- 任何 op 执行后立即校验；**违反则回滚该 op**，记一条 violation。
- 出口处对整篇再校验；违反 → fail-open，返回原始 items。

### 5a. provenance（sidecar）— 当前停用，备用

D4=(c) 纯削减下**没有 agent 新增字符**，provenance 为空。结构保留备用：若将来重开加字（视觉路线），
断言升级为 `C_out \ C_in ⊆ C_add`，新增字符逐一登记下表，**不进 items**（保 schema 透明），供审计/复核定位"哪些字是 AI 补的"：

```ts
type ProvenanceEntry = {
  itemId: string;             // 内部稳定 ID（§4a），非 array 下标
  field: "text" | "table_caption" | "list_items";
  charStart: number; charEnd: number;
  origin: "agent";
  op: string;
  confidence: number;
  note?: string;
};
```

> 当前可审计的是**削减**侧：被 drop/strip 删掉的内容记入 `report.removedSpans`（§14）。

---

## 6. 几何派生规则（bbox / page_idx）

下游 `anchor_inject` 用 `page_idx` 找页面容器、`bbox[1]`(y0) 定垂直高亮位置。原则：
**每个输出 item 仍能回指至少一个源 item 的 bbox**。

| op | bbox | page_idx |
|---|---|---|
| `merge(i,j)` | 并集 union | 取首块 |
| `split(i,off)` | 两子块都继承父块 bbox（二期按 y 比例切） | 同父块 |
| `demote/promote(i)` | 不变 | 不变 |
| `reorder` | 各块不变 | 各块不变 |
| `drop(i)` | —（删除） | — |
| `strip(i,…)` | 不变 | 不变 |

> eval 软检查：每个输出 item bbox 非空且落在其 page_idx 对应页范围内。

---

## 7. 架构

```
        ┌─────────────────────────────────────────────────────┐
        │  mineru-refine  (Bun + TS)                           │
  in ──▶│  ① 异常探测器(确定性)  →  worklist[suspect]          │
  items │            ▼                                         │
        │  ② tool-use loop (DeepSeek):                         │
        │     observe(带上下文) → LLM 选 op → apply+保真闸 → 重探测 │
        │            │  (loop-until-dry + 守卫)                 │
        │            ▼                                         │
        │  ③ 出口闸门: 保真不变式 + provenance 覆盖 + 异常单调 + 几何 │
        │      pass ─┴─ fail → fail-open(返回原始 in)           │
        └─────────────────────────────────────────────────────┘
                     ▼  { items(同schema), provenance, report }
```

LLM 仅负责 ② 里"选哪个 op、填什么参数"。①③ 全是确定性代码。
"合格"= ③ 机器闸门通过，**不是** LLM 自评。

**控制流（钉死）**：**确定性外层循环驱动**——弹出一个 worklist 疑点 → 把它（及上下文）交 LLM →
LLM 回一个 op 或 `dismiss` → 执行 → 重探测。**不是**让 LLM 当司机自由乱调。
理由：可控、便宜、可单测。每个 op 后**重探测会重算 index**，所以全程用稳定 ID（§4a）不用 index。

---

## 8. 工具集（LLM 可调用的 function）

### 观察类（只读）
- `outline()` → 所有 header 的 index/level/text 摘要（章节骨架）
- `getItems(start, end)` → 区间 item 完整内容（含相邻上下文）
- `whyFlagged(i)` → 探测器为何标记第 i 块
- `peekPage(i)` → 第 i 块所在页**及上下页**内容（跨页判断必需）

### 裁决类（不改文本）
- `dismiss(id, reason)` → 判定该疑点为误报，加入已裁决集，重探测不再标记它（§10 防永不终止）

### 变更类·纯削减/重组（D4=(c)：全集仅此 7 个，`C_out ⊆ C_in`，自带保真校验，违反即回滚）

**参数一律用稳定 ID（§4a），不用 array index。**

| op | 语义 |
|---|---|
| `merge(idA, idB)` | 两块拼成一块，去 MinerU 插入的分隔符 |
| `split(id, offset)` | 在 offset 处切成两块 |
| `demote(id)` | 伪标题降为 text（清 text_level） |
| `promote(id, level)` | text 升为 header |
| `reorder(idsInOrder)` | 修跨页错序 |
| `drop(id)` | 删页码/页眉/页脚/水印（须命中白名单类型） |
| `strip(id, pattern)` | 去超链接残留/LaTeX 残片（须命中白名单 pattern） |

> 加字 op（fixOcr / synthesizeCaption）**本期不做**（D4=(c)）。后续若开，需走视觉模型并重启 §5a provenance。

---

## 9. 异常探测器（确定性启发式 → worklist）

廉价规则，产出"疑点"非"结论"：

**可处理（有对应 op）：**
- 伪 HEADING：`type=text & text_level` 存在，但文本含逗号/分号、或句末标点收尾、或去编号后正文过长 → `demote`/`merge`
- 跨页断句：相邻块跨 `page_idx`，前块不以句末标点结尾、后块不以编号/标题特征开头 → `merge`
- 巨型块：单 `text` 超阈值且含多个疑似小标题编号 → `split`
- 混入正文的页眉页脚：高频重复短文本、页首/页尾 bbox、与已分类 header/footer/page_number 同文（≥2 处家具佐证，可为多条家具拼接；不受重复页数阈值限制）→ `drop`
- 残留符号：markdown 链接 `[..](..)` / LaTeX `$...$`、`\frac`、裸 `\命令{}` 残骸 / 孤立 `\$` 转义 → `strip`

**只标记、无 op（D5；LLM 对这类只能 `dismiss`，但计入 report 供观测）：**
- 跨页表格被拆成两个 `table` item
- 跨页列表被拆断
- 孤儿/错配 caption、空 caption 的 table/image
- 疑似 OCR 错字（D4=(c) 不修）

每条 = `{ kind, itemId, evidence, hasOp: boolean }`，进 worklist。判定逻辑可移植 docfuse `mineru_api.py` 的现有规则（不依赖其模块）。

---

## 10. 终止与守卫

- **loop-until-dry**：worklist 弹空 → 全量重探测 → 仍为空才到底。
- **误报裁决集（防永不终止）**：LLM 判某疑点是误报 → 调 `dismiss(id, reason)` → 加入已裁决集；
  重探测**必须排除已 dismiss 的疑点**，否则同一误报反复入列、loop 永不收敛。
- **maxIterations**：硬上限，到顶强停 + log。
- **防震荡**：记录已执行 op 序列；禁止刚做过的逆操作（merge↔split 同处等）。
- **无进展检测**：一轮 doc 未变 / 异常数未降 → 停。
- **合格判定（机器）**：worklist 空（剩余仅无 op 的标记类 / 已 dismiss）∧ 保真不变式 `C_out ⊆ C_in` ∧ 异常数 ≤ 输入 ∧ 几何可定位。
  任一不满足且已到 maxIterations → fail-open 返回原始 items。

---

## 11. LLM 接入（DeepSeek 裸 API / Bun / TS）

> 以下经 2026-06 官方文档核对（tool_calls / create-chat-completion / thinking_mode / json_mode / pricing / multi_round）。

**裸 API，不用任何 SDK**（用 Bun 的 `fetch`）：
- Endpoint：`POST https://api.deepseek.com/chat/completions`
- Header：`Authorization: Bearer <key>`、`Content-Type: application/json`
- Model：`deepseek-v4-pro`（上下文 1M，最大输出 384K）
- 凭据：`RAGENT_DEEPSEEK_APIKEY`（`~/.ragent_profile`），现场映射，不硬编码。
- **不上 Claude Agent SDK / 不上 openai SDK**：本 app 不碰 fs/bash/MCP，SDK 全是死重；裸 API 绕开一切翻译层（含历史上吞 tool-call 的 Anthropic shim）。

**thinking 模式：默认 `disabled`。**
- v4-pro 默认 thinking enabled，会返回 `reasoning_content` 并计 reasoning token（按输出价 6元/M 计费）。
- 我们关掉它（`"thinking": {"type": "disabled"}`），换取：① `temperature:0` 生效→可复现；② 省 reasoning token；
  ③ **绕开下面那个 400 雷**。本任务窄+有机器闸门，先不需要 reasoning；质量不够再开。
- ⚠️ **400 雷（若将来开 thinking）**：thinking 开启 + tool calls 时，**后续每轮请求必须把上一轮 assistant 的 `reasoning_content` 一并回传，否则 400**。开启 thinking 必须改造 message 拼接。

**请求要点**：
- `tools`：函数定义数组（§8 的 op 全转成 function schema）。
- `tool_choice: "required"`：**强制模型必调一个工具**，天然落实"禁止输出正文文本"（要么 op、要么 `dismiss`）。
- `temperature: 0`（thinking disabled 下生效）。
- **不用** `response_format: json_object`（它与 tool calls 不保证兼容，且偶发空 content）；我们走 tool calls 拿结构。
- `arguments` 字段是 **JSON 字符串** → 一律先过 `safe-json-repair` 的 `repair()` 再 `JSON.parse`，兜 DeepSeek 偶发坏 JSON / 多余闭合符，避免整轮空转。

**多轮 / 计费**：
- API **无状态**，客户端每轮带全量 history（含 assistant 的 `tool_calls` 消息 + `role:"tool"` 结果消息）。
- 省钱关键：**system prompt + 文档 outline 等稳定内容放在 messages 前缀且每轮不变**，吃 input **cache hit（0.025元/M，相对 miss 3元/M 便宜 120×）**。`usage.prompt_cache_hit_tokens` 可观测命中率。

**system prompt 要点**：
- 你**只能调用工具**；不确定就先 `getItems`/`peekPage` 看清楚再决定。
- 跨页合并前必须 `peekPage` 确认上下页连续。
- 拿不准就调 `dismiss`（宁可漏修，不可错改/误删真标题）。

**M0 冒烟前置**：裸 fetch 打 v4-pro，验**多轮** tool-call loop：`tool_choice:required` 强制调用 → 解析 `arguments`（过 safe-json-repair）→ 回传 `role:"tool"` 结果 → 第二轮仍能正确续调。确认 `tool_call`/`tool_result` 往返不丢、参数不截断。**绿了才往上盖楼。**

---

## 12. 集成点（docfuse 侧）

docfuse 是 Python，本 app 是 Bun/TS → 跨语言。core lib 吃对象（D3），跨进程用 JSON 包一层：

- **首选**：本 app 起一个 Bun HTTP 小服务，`POST /refine` 收 content_list JSON、回 `{items,...}`。
  docfuse `mineru_api.py::_parse_extracted_dir` 解析 `content_list.json` **之前**调一次（拿 items 替换）。改一处，之后无感知。
- **备选**：bun CLI，stdin 收 JSON、stdout 回 JSON，docfuse subprocess 调用。
- fail-open 同时在 transport 层兜：服务超时/不可用 → docfuse 用原始 MinerU 结果继续。

---

## 13. Eval（独立 app 改文档 → 回归网硬要求）

1. **golden fixtures**：一批 `原始 content_list → 期望清洗结果`，每次断言。
2. **保真不变式**（§5）：`C_out ⊆ C_in` 运行时闸门 + 测试断言（无新增字符）。
3. **table_body 不变**：未被 drop 的 item，`table_body` 逐字节相等。
4. **异常数单调**：输出（有 op 的）异常数 ≤ 输入，否则报警。
5. **几何可定位**（§6）：每个输出 item bbox 非空且在页范围内。
6. **幂等**：对清洗结果再跑一次为 no-op。

> 通常**没有**"干净原文"做 ground truth，所以"保真（C_out⊆C_in）+ 异常下降 + 幂等"是能拿到的最强代理指标。
> 削减侧的"删了什么"由 `report.removedSpans` 留痕供人审计。

---

## 14. 技术栈与项目骨架

```
mineru-refine/
├── package.json            # bun; deps: safe-json-repair（无 SDK，裸 fetch）
├── src/
│   ├── types.ts            # MineruItem / WorkItem / RefineResult / RefineReport
│   ├── id.ts               # 内部稳定 ID 分配/继承（§4a）
│   ├── detect.ts           # 异常探测器 → worklist（标 hasOp）
│   ├── ops/                # 7 个削减/重组 op：纯函数 (items, args) -> items
│   ├── invariant.ts        # 保真不变式 C_out ⊆ C_in + 几何校验 + 回滚
│   ├── loop.ts             # 确定性外层循环 + 守卫 + loop-until-dry + dismiss 集
│   ├── deepseek.ts         # 裸 fetch 调 v4-pro + safe-json-repair 包裹 arguments
│   ├── refine.ts           # 入口：refine(items, opts) -> RefineResult  (吃对象)
│   └── server.ts           # 可选 HTTP transport
└── test/                   # golden fixtures + eval 六件套
```

核心 API（D3 吃对象）：

```ts
export async function refine(
  items: MineruItem[],
  opts?: { markdown?: string; sha256?: string; maxIterations?: number }
): Promise<{ items: MineruItem[]; provenance: ProvenanceEntry[]; report: RefineReport }>;

type RefineReport = {
  iterations: number;
  opCounts: Record<string, number>;     // 各 op 执行次数
  dismissed: number;
  removedSpans: { itemId: string; text: string; reason: string }[]; // 审计：被 drop/strip 删掉的内容
  violations: number;                    // 保真闸回滚次数
  tokenUsage: { prompt: number; completion: number };
  failOpen: boolean;                     // 是否因失败而透传
};
```

> `removedSpans` 让"删了什么"也可审计，与 provenance 的"加了什么"对称，建立信任。

---

## 15. 里程碑

- **M0**｜DeepSeek v4-pro 裸 API 多轮 tool-call 冒烟（§11，`tool_choice:required` + safe-json-repair + 回传 `role:"tool"`）。**阻断后续。**
- **M1**｜types + 稳定 ID + `refine()` 骨架 + fail-open + 缓存（无 LLM，纯透传跑通）。
- **M2**｜异常探测器 + worklist（标 hasOp）。
- **M3**｜7 个削减/重组 op + 保真不变式闸门（脚本喂固定 op 序列验 replay+回滚，不接 LLM）。
- **M4**｜接 DeepSeek tool-use loop + 守卫 + dismiss 集 + loop-until-dry。
- **M5**｜eval 六件套 + golden fixtures。
- **M6**｜transport（HTTP/CLI）+ docfuse 接缝，端到端验证无感知 + 高亮不漂。

> 加字 op（原 v0.2 M4）随 D4 撤回；将来重开另起里程碑。

---

## 附：与 docfuse 现有 `_SANITIZE_PASSES` 的关系

docfuse 的确定性 pass 链（伪 heading 降级 / 跨页列表合并 / 巨型块拆分）是本 app 的**思想原型**。
本 app 把"需要语义判断/加字"的部分交给 LLM，"有稳定 pattern"的部分仍走确定性探测器。
两者解耦，docfuse 不依赖本 app 存在；迁移后 docfuse 侧 pass 链可逐步瘦身但不强制。
```
