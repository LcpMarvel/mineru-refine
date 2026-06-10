# mineru-refine

MinerU 解析结果的 **linter / fixer**。content_list（item 对象数组）进，**同 schema** 出：
修掉 MinerU 解析 PDF 时产生的结构 quirk——伪标题、跨页断句、巨型块、混入正文的页眉页脚、
LaTeX / 链接残留——只做**削减与重组，绝不新增一个字**。消费方读到的仍是"一份 MinerU
结果"，作为透明过滤器接入现有 pipeline，零改动。

由一个 LLM tool-use 循环驱动：确定性探测器找疑点，LLM（DeepSeek v4-pro 裸 API）只负责
"对这个疑点选哪个修复 op，或裁定误报"；**是否合格由机器闸门裁决，不由 LLM 自评**。

## 硬保证

- **保真 `C_out ⊆ C_in`**：内容字符（`text` + `list_items` + `table_caption`，仅计非空白）
  输出是输入的子多重集——不含任何输入里没有的字。每个 op 执行后立即校验，违反即回滚；
  出口对整篇再校验，不过则 fail-open。
- **`table_body` 逐字节不变**（未被 drop / mergeTable 的表）。mergeTable 产物降级为**行级逐字节**：
  每个 `<tr>` 行必须逐字节来自输入行池、行外"外壳"逐字节命中某个输入表外壳——
  即除"把若干输入行按原字节拼进某个输入表"外，任何字节改动都会被闸门回滚。
- **schema 透明**：输出字段集合/类型与 MinerU 一致，未知字段原样透传，内部稳定 ID 出口前剥除。
- **fail-open**：任何异常 / 超时 / LLM 不可用 → 原样返回输入 items + 大声 log，绝不搞崩上游。
- **幂等**：清洗结果再跑一次输出逐字节不变（实测三份真实文档成立）。无残留疑点的文档零 LLM
  调用；含误报疑点的文档会重新裁决为 dismiss——烧 token 但不改内容。提供 `sha256` 可命中缓存直接跳过。
- **可审计**：删掉的每一段内容留痕于 `report.removedSpans`（itemId / 原文 / 原因）。

## 工作原理

```
        ┌─────────────────────────────────────────────────────┐
  in ──▶│  ① 异常探测器（确定性启发式）  →  worklist[suspect]   │
 items  │            ▼                                         │
        │  ② tool-use loop（DeepSeek）：                        │
        │     观察(预载上下文) → LLM 选 op / dismiss            │
        │     → 执行 + 保真闸(违反即回滚) → 重探测              │
        │            ▼   (loop-until-dry + 守卫)               │
        │  ③ 出口闸门：C_out ⊆ C_in ∧ 异常数单调 ∧ 几何可定位   │
        │      pass ─┴─ fail → fail-open（返回原始输入）        │
        └─────────────────────────────────────────────────────┘
                     ▼  { items(同schema), report }
```

控制流由**确定性外层循环**驱动：弹出一个疑点 → 连同上下文交给 LLM → LLM 回一个 op 或
`dismiss` → 执行 → 重探测。不让 LLM 当司机自由乱调——可控、便宜、可单测。

每个 item 在流程内带一个**内部稳定 ID**（`it_0001`），所有 op 参数、worklist、LLM 引用
一律用 ID 而非 array index——一次 merge/split 就会让下标全体错位。ID 是内部字段，出口前剥除。

### 探测器疑点

**可处理（有对应 op）：**

| kind | 启发式 | 修复 |
|---|---|---|
| `pseudo_heading` | 带 `text_level` 但含逗号/句末标点/正文过长 | `demote` / `merge` |
| `cross_page_break` | 相邻块跨页，前块未以句末标点结尾 | `merge` |
| `giant_block` | 单 text 超阈值且含多个疑似小标题编号 | `split` |
| `page_artifact` | 高频重复短文本 / 与已分类页眉页脚同文（≥2 处家具佐证） | `drop` |
| `residual_markup` | markdown 链接、`$...$`、`\frac` 等 LaTeX 残骸 | `strip` |
| `empty_table` | 零内容空壳表（无行/caption/图——MinerU 自行跨页合并后留下的占位，真实数据中"续表"多为此形态） | `drop` |
| `split_table` | 跨页相邻两个**有体**表格（跳过页面家具判相邻） | `mergeTable`（优先视觉裁决，见下） |
| `split_list` | 跨页相邻两个列表 | `mergeList` |

**只标记、无 op（LLM 只能 `dismiss`，计入 report 供观测）：**
孤儿/空 caption（`caption_issue`）。

### op 集（9 个削减/重组 + dismiss）

全部是纯函数 `(items, args) -> items`，自带保真校验，违反即回滚并计入 `report.violations`。

| op | 语义 | bbox / page_idx 派生 |
|---|---|---|
| `merge(idA, idB)` | 相邻两块拼一块，去 MinerU 插入的分隔符 | bbox 并集；page_idx 取首块 |
| `split(id, offset)` | 在 offset 处切两块 | 两子块继承父块 |
| `demote(id)` | 伪标题降为正文（清 `text_level`） | 不变 |
| `promote(id, level)` | 正文升为标题 | 不变 |
| `reorder(idsInOrder)` | 修跨页错序（仅限连续区间的排列） | 各块不变 |
| `drop(id)` | 删页码/页眉/页脚/水印/零内容空壳表（须命中白名单） | —（删除） |
| `strip(id, pattern)` | 去残留符号（pattern 白名单：`md_link` / `latex_dollar` / `latex_block` / `latex_command` / `escaped_dollar` / `html_tag`） | 不变 |
| `mergeTable(idA, idB)` | 跨页拆表合并：B 的 `<tr>` 行**原字节**追加到 A 末行后，caption/footnote 拼接；B 首行与 A 表头逐字节相同（每页重印表头）时去重并留痕 | bbox 并集；page_idx 取首块 |
| `mergeList(idA, idB, joinSeam?)` | 跨页拆列表合并：`list_items` 拼接；`joinSeam` 把 A 尾项与 B 首项缝成一项（断句跨页） | bbox 并集；page_idx 取首块 |
| `dismiss(id, reason)` | 裁定误报，不改文本；重探测不再标记它 | — |

mergeTable **不做列对齐判断，也不做列对齐修复**："是否同一张表"由 LLM 看内容裁决（列数相等
故意不做闸门——rowspan 跨页携带、某页空列被 MinerU 略去都会造成列数合法地不等）；
列参差的行原样保留，绝不发明空单元格去"补齐"——补哪一列是语义猜测，猜错即篡改，
而行级保真闸恰好把这类"修复"挡在门外。错位若存在，那是 MinerU 输入即有的，合并不引入新损伤。

### split_table 的视觉裁决（Qwen-VL）

"是否同一张表"是图里一眼可见、文本里只能猜的事实，所以提供 `loadImage` 时
`split_table` 疑点**优先路由给 Qwen-VL**：把 A/B 两表的 MinerU 裁剪图（content_list 的
`img_path` 本来就指向它们）发给 `qwen-vl-max` 问一个窄问题，结构化回答映射到
`mergeTable` 或 `dismiss`。要点：

- **只输出决策，不产内容字符**——merge 仍走 `applyOpChecked` 行级保真闸，不碰纯削减红线。
- **fail-open 到文本路径**：无图 / 无 key / VLM 不可用 / 判决 op 被闸门拒 → 自动回退
  DeepSeek 文本裁决，绝不阻塞。
- 实测两份真实文档 7 判 7 对（5 真续表 merge + 2 假续表 dismiss，含 rowspan 列数不等、
  文控页同位置异表等困难形态），单次 ~2k token。

几何字段（`bbox` / `page_idx`）的派生规则保证**每个输出 item 仍能回指至少一个源 item**——
下游做高亮定位依赖它们。

### 守卫与终止

- **loop-until-dry**：worklist（有 op、未 dismiss）弹空才到底。
- **dismiss 裁决集**：已裁定的误报重探测时排除，防同一误报反复入列、循环永不收敛。
- **maxIterations** 硬上限，到顶强停；单疑点轮数耗尽 → 强制搁置（计入 dismissed）。
  默认随初始疑点数自适应（`min(max(48, 2N+16), 512)`）——修复会解锁新疑点
  （实测总工作量 ≈ 1.6× 初始数），固定常数对大文档必然截断。
- **防震荡**：merge 产物禁止立刻 split，split 产物对禁止立刻 merge 回去。
- 出口合格判定全部是机器检查：worklist 空 ∧ `C_out ⊆ C_in` ∧ 异常数 ≤ 输入 ∧ 几何可定位；
  任一不满足 → fail-open。

## 使用

### 作为库（收/发内存对象，不读写文件）

```ts
import { refine } from "./src/refine.ts";

const { items, report } = await refine(contentList, {
  sha256,          // 可选；提供则启用进程内缓存
  maxIterations,   // 外层循环硬上限；不传则自适应：min(max(48, 2×疑点数+16), 512)
  concurrency,     // 疑点并行裁决数，默认 8；1 = 严格串行
  loadImage: imageDirLoader(mineruOutputDir), // 可选；启用 split_table 视觉裁决
});
```

缓存 key = `sha256 + refineLogicVersion + model + promptVersion`——只用 SHA256 是错的，
逻辑/prompt/模型一变旧结果会错误命中。

`report` 字段：`iterations` / `opCounts` / `dismissed` / `removedSpans` / `violations` /
`tokenUsage` / `failOpen`。

性能：疑点默认 8 路并行裁决，常见疑点的上下文（±2 邻居 / 跨页整页）预载进首条消息省观察
轮次；DeepSeek 调用对网络错误/429/5xx 自动重试，单疑点故障只搁置自身不毁全局（全程零成功
才 fail-open）。实测 71 页 / 1004 items / 46 疑点：~86s（串行 ~622s）。

### HTTP 服务（跨语言消费方首选）

```bash
bun run server                  # 默认端口 8771，MINERU_REFINE_PORT 可改
curl -X POST localhost:8771/refine -d '{"items":[...], "sha256":"...", "imageDir":"/abs/mineru/out"}'
curl localhost:8771/health
```

`imageDir`（可选）是 MinerU 产物目录的绝对路径，须与本服务共享文件系统；
提供则启用视觉裁决。CLI 同理（stdin 包对象里带 `imageDir` 字段）。

消费方在解析 `content_list.json` 之前调一次，用返回的 `items` 替换即可；
建议在调用侧也兜一层超时/不可用回退（fail-open 双保险）。

### CLI（备选）

```bash
cat content_list.json | bun run cli      # stdin JSON → stdout JSON（仅 items）
```

## LLM 接入（全部裸 API，零 SDK）

**DeepSeek `deepseek-v4-pro`（文本裁决主力）**——裸 `fetch` 打 `POST https://api.deepseek.com/chat/completions`：

- 本库不碰 fs/bash/MCP，SDK 全是死重，且翻译层历史上吞过 tool-call。
- key 取 `.env` 的 `DEEPSEEK_APIKEY`（或环境变量 `RAGENT_DEEPSEEK_APIKEY`），缺则启动即抛。
- `thinking: disabled`：让 `temperature: 0` 生效（可复现）、省 reasoning token，并绕开
  "thinking + tool-call 必须回传 `reasoning_content` 否则 400"的雷。
- `tool_choice: "required"`：强制每轮必调一个工具（op 或 dismiss），天然禁止输出正文。
- `arguments` 是 JSON 字符串，一律先过 `safe-json-repair` 再 parse，兜偶发坏 JSON。
- 省钱：system prompt + 文档 outline 放 messages 前缀且每轮不变，吃 DeepSeek 的
  input cache hit（命中价约为 miss 的 1/120）。

**Qwen `qwen-vl-max`（split_table 视觉裁决）**——裸 `fetch` 打 DashScope OpenAI 兼容端点：

- 环境变量：`QWEN_APIKEY`（必填）、`QWEN_BASE_URL` / `QWEN_VISION_MODEL`（有默认值）。
- 图走 base64 data URL，`temperature: 0`，回复按 JSON 裁决解析（过 `safe-json-repair`）。
- 网络错误 / 429 / 5xx 自动重试；任何失败回退 DeepSeek 文本路径。

## 开发

```bash
bun install
bun test                        # 全程 mock LLM，不打网络
bun run typecheck
bun run m0                      # 冒烟：验真实 DeepSeek 多轮 tool-call（需 key）
bun run m0:vl                   # 冒烟：验真实 Qwen-VL 判表（三对真实表格图，需 key）
```

测试覆盖 eval 六件套：① golden fixtures ② `C_out ⊆ C_in` ③ table_body 逐字节不变
④ 异常数单调 ⑤ 几何可定位 ⑥ 幂等。没有"干净原文"做 ground truth 时，
"保真 + 异常下降 + 幂等"是能拿到的最强代理指标。

### 真实数据工作流

```bash
# .env 需有 MINERU_API_TOKEN
bun run mineru:fetch            # 把 test_data/source/ 下的 PDF/DOC 交 MinerU 官方 API 解析，
                                # 产物落盘 test_data/mineru/<stem>/
                                # （--force 重跑；--batch <id> 复用已完成的 batch）
bun run refine:real             # 对全部真实 content_list 跑 refine（真 LLM），
                                # 输出 test_data/refined/<stem>/，打印疑点前后对比
bun run refine:real <stem>      # 只跑某个文档；REFINE_MAX_ITERATIONS 可调上限
```

`test_data/refined/<stem>/` 是对应 MinerU 产物目录的 **drop-in 替身**：images/、layout.json
等原样镜像（`img_path` 引用不断链），`content_list.json` 替换为清洗版，`full.md` 从清洗后
items 确定性重渲染，另附 `refine_report.json`（审计：ops/dismissed/removedSpans/tokens）。

## 目录结构

```
src/types.ts      # MineruItem / WorkItem / OpCall / RefineReport
src/id.ts         # 内部稳定 ID（出口剥除，绝不进输出 schema）
src/detect.ts     # 确定性异常探测器 → worklist
src/ops/index.ts  # 9 个削减/重组 op + 保真闸 + 回滚
src/invariant.ts  # C_out ⊆ C_in / table_body / 几何校验
src/loop.ts       # 确定性外层循环 + LLM tool-use + 守卫
src/deepseek.ts   # 裸 fetch v4-pro（thinking disabled / tool_choice required / temp 0）
src/qwen_vl.ts    # 裸 fetch qwen-vl-max（split_table 视觉裁决，失败回退文本）
src/markdown.ts   # 清洗后 items → full.md 确定性重渲染
src/refine.ts     # 入口：fail-open + 缓存 + 出口闸门
src/server.ts     # HTTP transport
src/cli.ts        # stdin/stdout transport
```

## 边界（有意不做的）

- **不加字**：OCR 纠错、补图注等内容生成一概不做——纯削减让保真完全可证。
  真有需求时走视觉模型路线，届时启用预留的 `provenance` 通道（逐字登记 AI 新增字符）。
- **不修表格列对齐**：mergeTable 不补空单元格、不重排单元格（见 op 集说明）。
- 不感知任何下游业务模型；不替代 MinerU 的解析，只做其输出的后处理。
