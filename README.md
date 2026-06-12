# mineru-refine

[MinerU](https://github.com/opendatalab/MinerU) 解析结果的后处理器(linter / fixer)。

MinerU 把 PDF 解析成 `content_list.json`——一个 item 对象数组,每个 item 是一段正文、
一个标题、一张表格或一张图。解析质量很好,但结构上有一类高频问题:

- **伪标题**——一句普通正文被误标成标题
- **漏标标题**——同级编号兄弟都是标题，它却被标成正文
- **跨页断句**——一句话被页边界切成两个 item
- **跨页拆表**——同一张表被切成多页的多个表
- **表内续行**——一条记录的长单元格被切成多个只有一列有字的 `<tr>`
- **页面家具**——页眉、页脚、页码混进正文
- **残留符号**——markdown 链接、LaTeX 命令、`\$`/`\*` 转义等解析残骸
- **巨型块**——多个小节被糊成一个超长 item
- **段尾粘连**——跨页合并把「[相关文件]」这类独立结构块吸进上一段结尾
- **表格噪声**——全空 `<tr>`、单元格内 OCR 空格（含被空格打断的 URL）、伪 LaTeX 包装
  （`$\text{...}$` 套着普通文字，已知符号命令换成 Unicode；`\frac` 等真公式不动）

其中无歧义的表格噪声由**机械清洗 pass**（确定性代码、自带校验、不打 LLM）直接处理，
其余疑点交 LLM 裁决。

mineru-refine 接收 content_list,修掉这些问题,返回**同 schema** 的 content_list。
下游读到的仍然是"一份 MinerU 结果"——作为透明过滤器接进现有 pipeline,消费方零改动。

两条核心承诺:

1. **绝不新增一个字**:只做削减与重组(合并、拆分、删除、降级),输出的每个内容字符都
   来自输入,且由机器在每一步校验——不是靠 prompt 约束 LLM,而是违反即自动回滚。
2. **绝不搞崩上游(fail-open)**:任何异常、超时、LLM 不可用,都原样返回输入 items
   并大声记 log,`report.failOpen` 标记为 `true`。

修复决策由 LLM 做("这个疑似伪标题该降级,还是误报?"),但 LLM 只负责在预定义的修复
操作里**选一个**——执行、校验、终止全部由确定性代码控制,**是否合格由机器闸门裁决,
不由 LLM 自评**。

## 安装

核心是一份 Rust 实现,各语言绑定直接 import 同一份代码,选项与返回值完全同构:

| 语言 | 安装 | 形态 |
|------|------|------|
| **Python** | `pip install mineru-refine` | PyO3 原生扩展([文档](bindings/python/)) |
| **JS/TS** | `bun add mineru-refine` / `npm i mineru-refine` | napi-rs 原生插件,Bun / Node ≥18([文档](bindings/js/)) |
| **Rust** | `cargo add mineru-refine` | core crate([文档](crates/mineru-refine/)) |
| **任意语言** | `cargo install mineru-refine --features bin` | HTTP server / CLI(见下) |

## 快速上手

需要 LLM API key(见[环境变量](#llm-接入与环境变量)):`DEEPSEEK_APIKEY` 必需,
`QWEN_APIKEY` 在启用表格视觉裁决时需要。

**Python:**

```python
import json
import mineru_refine

items = json.load(open("content_list.json"))
result = mineru_refine.refine(items, image_dir="/abs/path/to/mineru/output")
result["items"]    # 清洗后的 content_list,schema 与输入一致
result["report"]   # 审计报告:做了什么、删了什么、花了多少 token
```

**JS/TS:**

```ts
import { refine } from "mineru-refine";

const { items, report } = await refine(contentList, {
  imageDir: "/abs/path/to/mineru/output",
});
```

**Rust:**

```rust
use mineru_refine::{refine, RefineOptions};

let result = refine(items, RefineOptions {
    image_dir: Some("/abs/path/to/mineru/output".into()),
    ..Default::default()
}).await;
// 永不 Err、panic 不外漏:fail-open 内置,看 result.report.fail_open
```

**HTTP(任意语言):**

```bash
cargo install mineru-refine --features bin
mineru-refine-server   # 默认端口 8771,MINERU_REFINE_PORT 可改

curl -X POST localhost:8771/refine \
  -d '{"items":[...], "imageDir":"/abs/path/to/mineru/output"}'
curl localhost:8771/health
```

`imageDir` 是 MinerU 产物目录(含 `images/` 的那个目录),可选:提供则启用跨页拆表的
视觉裁决(用表格裁剪图判断"是不是同一张表"),不提供则该类问题整体跳过、表格原样保留。
HTTP 模式下该目录须与 server 共享文件系统。

建议消费方在读 `content_list.json` 之后、消费之前调一次,用返回的 `items` 替换原数组;
调用侧再兜一层超时回退,与内置 fail-open 构成双保险。

## 选项与返回值

各语言只是命名风格不同(Python 蛇形、JS 驼峰),语义相同:

| 选项 | 默认 | 语义 |
|---|---|---|
| `sha256` | 无 | 源文件 SHA256;提供则启用进程内缓存。缓存 key 同时包含逻辑版本、模型、prompt 版本——这些一变,旧结果自动失效,不会错误命中 |
| `maxIterations` | 自适应 | 修复循环硬上限。默认随疑点数自适应(`clamp(2N+16, 48, 512)`),到顶强停 |
| `concurrency` | 8 | 并行裁决的疑点数;`1` = 严格串行 |
| `imageDir` | 无 | MinerU 产物目录;提供则启用跨页拆表的视觉裁决(视觉是该类问题的唯一裁决路径) |
| `fixOcrConfusion` | `false` | **opt-in** 的 OCR 字符混淆修正层(CE0→CEO、入=n→λ=n、竟争→竞争……),覆盖正文与表格单元格。开启后输出契约从"只删不增"变为双契约,见下文[混淆修正层](#混淆修正层opt-in) |
| `extraConfusionPairs` | `[]` | 混淆准入名单的用户补充对,每项恰好 2 个不同字符(如 `"0D"` 表示 0↔D 互换可直接落地)。非法配置立即 fail-open,不静默吞 |
| `rewriteGarbledTables` | `false` | **opt-in** 的重度乱码表视觉重转写层(代格→代码、数据来酒→数据来源、Midhuel→Michael……)。机械检测器选定整表认废的表格,Qwen-VL 对照 `img_path` 截图逐单元格重转写。需要 `imageDir`(缺则 fail-open),见下文[乱码表重转写层](#乱码表重转写层opt-in) |

返回值 `{ items, report, provenance }`:

| 字段 | 含义 |
|---|---|
| `items` | 清洗后的 content_list,字段集合/类型与 MinerU 一致,未知字段原样透传 |
| `report.iterations` | 修复循环实际轮数 |
| `report.opCounts` | 各修复操作的执行次数 |
| `report.dismissed` | 被裁定为误报(或被搁置)的疑点数 |
| `report.removedSpans` | 删除留痕:每段被删内容的 itemId / 原文 / 原因,逐条可审计 |
| `report.violations` | 保真闸回滚次数(修复产物违反保真被自动撤销) |
| `report.tokenUsage` | LLM token 消耗 |
| `report.failOpen` | 是否触发 fail-open;`true` 时 `items` 即原始输入 |
| `report.confusionFixes` | 混淆层落地的每条替换(itemId / 字段 / 偏移 / 前后字符 / 准入来源 / LLM 依据)。仅 `fixOcrConfusion` 开启且有替换时出现 |
| `report.confusionRejected` | 被闸门拒绝的混淆提案数(结构非法 / 密度超标 / 二次裁决否决) |
| `report.confusionObservations` | LLM 裁决时顺带观察到的表外 OCR 质量问题,只记录、从未被应用,可作下游质量信号 |
| `report.tableRewrites` | 重转写层落地的每条整格替换(itemId / 行列号 / before / after / 新串字符区间)。`before` 即撤销凭据,写回该区间可程序化还原。仅 `rewriteGarbledTables` 开启且有替换时出现 |
| `report.tableRewriteRejected` | 被闸门拒绝的重转写提案数(结构非法 / 行列不存在 / 整表覆盖率回归不过) |
| `provenance` | 默认恒为空(纯削减不加字);混淆层/重转写层开启时逐条登记其替换(origin=`ocr_confusion` / `garbled_table`) |

## 硬保证

- **保真**:输出的内容字符(`text` + `list_items` + `table_caption`,仅计非空白)是输入
  的子多重集——记作 `C_out ⊆ C_in`,即不含任何输入里没有的字。每个修复操作执行后立即
  校验,违反即回滚;出口对整篇再校验一次,不过则 fail-open。
- **表格逐字节不变**:未被处理的表,`table_body` 逐字节等于输入。跨页合并的表降级为
  **行级逐字节**:每个 `<tr>` 行必须逐字节来自输入的行池,行外"外壳"逐字节命中某个
  输入表外壳——除"把若干输入行按原字节拼进某个输入表"之外,任何字节改动都会被闸门回滚。
- **schema 透明**:输出字段集合/类型与 MinerU 一致,未知字段原样透传;内部使用的稳定 ID
  在出口前剥除,绝不进输出。
- **fail-open**:任何异常 / 超时 / LLM 不可用 → 原样返回输入 + 大声 log,绝不搞崩上游。
- **幂等**:清洗结果再跑一次,输出逐字节不变(实测三份真实文档成立)。无疑点的文档零 LLM
  调用;提供 `sha256` 可命中缓存直接跳过。
- **可审计**:删掉的每一段内容都留痕于 `report.removedSpans`(itemId / 原文 / 原因)。

以上保证在默认配置下全部成立。显式开启 `fixOcrConfusion` 后,"保真"与
"表格逐字节不变"两条变为下述双契约,其余保证不变;显式开启 `rewriteGarbledTables` 后,
被机械检测器判废的个别表格另有"整格替换 + 全量留痕"的独立契约,见[乱码表重转写层](#乱码表重转写层opt-in)。

## 混淆修正层(opt-in)

OCR 高频形近误认(`CE0`→`CEO`、`0A系统`→`OA`、`入=n`→`λ=n`、`竟争`→`竞争`、
`B1.36%`→`81.36%`)伤检索且无法用削减修复——这是**替换**。默认关闭;
显式传 `fixOcrConfusion: true` 才运行,跑在核心清洗与全部出口闸门**之后**,
是一个独立后处理层。

开启后的输出契约(双契约,均机器可验证/可追溯):

1. **核心层**照旧:只删不增(`C_out ⊆ C_in` 对核心阶段成立);
2. **混淆层**:所有修改都是稀疏的一换一定点替换,每条要么属于内置混淆等价类
   (`0↔O`、`1↔l↔I↔|`、`8↔B`、`入↔人↔λ`、`竟↔竞` 等,可经 `extraConfusionPairs` 补充),
   要么通过了独立的对抗式二次裁决;全量进 `report.confusionFixes` 与 `provenance`,
   可审计、可程序化撤销。

权力结构:**LLM 只有提案权,没有写入权**。每条提案过三道机械闸门——
恰好 1 字符、单字段替换密度上限(混淆是稀疏的,超标整字段拒绝)、准入名单
(表内直落 / 表外二次裁决)。
层内 LLM 故障只搁置对应批次(漏修不误修),层级异常只丢弃本层、核心产物原样返回。

**表格**:`table_body` 经词法切分后只有 td/th 单元格内的文本可成为候选——
HTML 标签骨架(`colspan=1` 的 `1`)在构造上就不可能被替换,实体(`&amp;`)当黑盒跳过,
即"标签骨架逐字节不变,单元格文本仅有准入名单内的稀疏一换一替换"。表格候选用
行列结构化上下文裁决(表标题/表头/所在行),并多一道每表聚合密度闸门:单格各自合规
但整表提案过多 = 乱码表特征,整表拒绝——乱码表的归宿是整表裁决,不是逐字"修复"。

**全文频率投票**:候选字与邻字构成的高频词全文一致出现(≥5 次)且无任何类内变体写法
→ 大概率真术语,加白跳过送审(压误报、省调用);拉丁 token 的少数派写法
(`OGSTM`×2 vs `OGSMT`×20,单字差或相邻换位)生成定点候选,LLM 确认且命中多数派写法的
免二次裁决直落(`source=frequency_vote`——差异本身就是全文实证)。

**observations 闭环**:LLM 裁决时顺带报告的「X 应为 Y」表外观察,解析出单字替换后
生成定点候选做第二轮裁决(三道闸门照旧),回收已花掉的 token。最多一轮回灌,
第二轮的 observations 只记录不再回灌(防循环);频率加白的术语(「烟感」×5)不回灌。

`fixOcrConfusion` 与 `extraConfusionPairs` 均进缓存 key,开关不同的调用绝不互相污染缓存。

## 乱码表重转写层(opt-in)

个别表格会被 OCR **整体认废**(实测某表 13+ 处乱码:代格/目择值/数据来酒/合格军/
Midhuel……),逐字符混淆修正救不动——但它的 `img_path` 截图完全清晰可读。这类表的
归宿是对照图像**逐单元格重转写**。默认关闭;显式传 `rewriteGarbledTables: true` 才运行
(需要 `imageDir`,缺则按配置错误 fail-open),跑在全部出口闸门之后、混淆层之前。

权力结构:**目标选定 100% 由机械检测器定,LLM 无提名权**。检测器对单元格文本的汉字段
做正向最大匹配(内嵌 6 万常用词词典),算"被词典词覆盖的字符比例"——乱码词的特征是
常用字的非词组合(代格/目择/来酒),覆盖率塌方;正常表即便满是专名(股票代码/公司名)
也明显更高。阈值按真实文档标定:乱码表 0.46,最差正常表 0.61,取 0.55 判废。

判废的表连同当前单元格内容送 Qwen-VL 对照截图,视觉模型只有**单元格级提案权**,
落地过三道机械闸门:

1. **资格**:原格必须有"乱码已毁"的证据——空格、纯数值格、短编号格(`G1.4`)、
   词覆盖率正常的格一律不许动。实测视觉模型在 33 列宽表上会**行列错位**,
   把别格内容张冠李戴过来(`79.41%`→`84.1%`、长句→`Michael`),这道闸门拦下全部此类提案;
2. **结构**:行列号必须命中现存单元格、不得引入标签/控制字符、长度有上限、
   提案不得是纯数值、长度量级与原格可比(均为错位特征),同格重复提案只认第一条;
3. **整表回归**:重转写后的词典覆盖率必须**严格高于**重转写前——视觉模型在"修复"
   以外做的任何事都会被这道闸门按住,整表回退。

HTML 标签骨架在构造上不可触碰(替换只发生在单元格内层区间);每条替换进
`report.tableRewrites`(`before` 字段即撤销凭据)与 `provenance`(origin=`garbled_table`),
可审计、可程序化撤销。取不到图 / 视觉故障 / 超大表只搁置对应表(漏修不误修),
层级异常只丢弃本层。顺带地,整格重转写天然覆盖 `Midhuel→Michael` 这类**词级**错误——
它们超出混淆层的单字符契约,在视觉重审语境里是自然产物。

`rewriteGarbledTables` 进缓存 key(含重转写 prompt 版本),开关不同的调用绝不互相污染缓存。

## 工作原理

```
        ┌─────────────────────────────────────────────────────┐
  in ──▶│  ① 异常探测器(确定性启发式)  →  疑点队列              │
 items  │            ▼                                         │
        │  ② tool-use 循环(DeepSeek):                          │
        │     预载上下文 → LLM 选修复操作 / 判误报              │
        │     → 执行 + 保真闸(违反即回滚) → 重新探测            │
        │            ▼   (队列弹空才结束 + 多重守卫)            │
        │  ③ 出口闸门:保真 ∧ 疑点数不增 ∧ 几何可定位            │
        │      pass ─┴─ fail → fail-open(返回原始输入)         │
        └─────────────────────────────────────────────────────┘
                     ▼  { items(同schema), report }
```

控制流由**确定性外层循环**驱动:从队列弹出一个疑点 → 连同上下文交给 LLM → LLM 回一个
修复操作或"误报"裁定 → 执行 → 重新探测。不让 LLM 自由驱动流程——可控、便宜、可单测。

每个 item 在流程内带一个**内部稳定 ID**(如 `it_0001`),所有操作参数、队列、LLM 引用
一律用 ID 而非数组下标——一次合并/拆分就会让下标全体错位。ID 是内部字段,出口前剥除。

### 探测器:能发现哪些问题

**可处理(有对应修复操作):**

| 疑点类型 | 启发式 | 修复 |
|---|---|---|
| `pseudo_heading` 伪标题 | 带 `text_level` 但含逗号/句末标点/正文过长 | `demote` / `merge` |
| `cross_page_break` 跨页断句 | 相邻块跨页,前块未以句末标点结尾 | `merge` |
| `giant_block` 巨型块 | 单 text 超阈值且含多个疑似小节编号 | `split` |
| `page_artifact` 页面家具 | 高频重复短文本,或与已识别页眉页脚同文(≥2 处佐证) | `drop` |
| `residual_markup` 残留符号 | markdown 链接、`$...$`、`\frac` 等 LaTeX 残骸 | `strip` |
| `empty_table` 空壳表 | 零内容表(无行/caption/图)——MinerU 跨页合并后留下的占位 | `drop` |
| `split_table` 跨页拆表 | 跨页的两个有体表格,中间仅页面家具。支持三页以上的链式拆表(每轮合一对,逐段咬合) | `mergeTable`(**仅视觉裁决**,见下) |
| `split_list` 跨页拆列表 | 跨页相邻的两个列表 | `mergeList` |
| `missed_heading` 漏标标题 | 同级编号兄弟是标题而本块是正文,且编号相邻 | `promote` |
| `trailing_marker` 段尾粘连节标记 | 段尾粘了「[相关文件]」类独立结构块(跨页 merge 吸入) | `split` |
| `separated_caption` caption 错序 | caption 样短文本与表格之间隔着一个标题块 | `reorder` |
| `extra_char` 赘字/衍字 | 功能词叠字(的的/地地/是是/了了,合法叠词除外)、孤立偏旁部首(「3)亻」) | `deleteChar` |

**只标记、无修复操作**(LLM 只能判误报,计入 report 供观测):孤儿/空 caption(`caption_issue`)。

### 修复操作集(10 个削减/重组 + dismiss)

全部是纯函数 `(items, args) -> items`,自带保真校验,违反即回滚并计入 `report.violations`。

| 操作 | 语义 | bbox / page_idx 派生 |
|---|---|---|
| `merge(idA, idB)` | 相邻两块拼一块,去掉 MinerU 插入的分隔符 | bbox 并集;page_idx 取首块 |
| `split(id, offset)` | 在 offset 处切成两块 | 两子块继承父块 |
| `demote(id)` | 伪标题降为正文(清 `text_level`) | 不变 |
| `promote(id, level)` | 正文升为标题 | 不变 |
| `reorder(idsInOrder)` | 修跨页错序(仅限连续区间内的排列) | 各块不变 |
| `drop(id)` | 删页码/页眉/页脚/水印/空壳表(须命中白名单类型) | —(删除) |
| `strip(id, pattern)` | 去残留符号。pattern 白名单:`md_link` / `latex_dollar` / `latex_block` / `latex_command` / `escaped_dollar` / `html_tag` | 不变 |
| `deleteChar(id, offset)` | 删单个 OCR 衍字。白名单严格:与紧邻字符重复的功能词叠字(的/地/是/了)或孤立偏旁部首;的的确确/地地道道/是是非非受构造性保护 | 不变 |
| `mergeTable(idA, idB)` | 跨页拆表合并:B 的 `<tr>` 行**原字节**追加到 A 末行后,caption/footnote 拼接;B 首行与 A 表头逐字节相同时(每页重印表头)去重并留痕 | bbox 并集;page_idx 取首块 |
| `mergeList(idA, idB, joinSeam?)` | 跨页拆列表合并:`list_items` 拼接;`joinSeam` 把 A 尾项与 B 首项缝成一项(断句跨页) | bbox 并集;page_idx 取首块 |
| `dismiss(id, reason)` | 裁定误报,不改文本;重新探测时不再标记它 | — |

`mergeTable` **不做列对齐判断,也不做列对齐修复**:"是否同一张表"由模型看内容裁决
(故意不把"列数相等"做成闸门——rowspan 跨页携带、某页空列被 MinerU 略去,都会造成列数
合法地不等);列参差的行原样保留,绝不发明空单元格去"补齐"——补哪一列是语义猜测,猜错
即篡改,而行级保真闸恰好把这类"修复"挡在门外。错位若存在,那是 MinerU 输入即有的,
合并不引入新损伤。

几何字段(`bbox` / `page_idx`)的派生规则保证**每个输出 item 仍能回指至少一个源 item**——
下游做高亮定位依赖它们。

### 跨页拆表的视觉裁决(Qwen-VL,唯一路径)

"两个表是不是同一张表"是图里一眼可见、文本里只能猜的事实,所以 `split_table` 疑点
**只走视觉裁决**:把两个表的 MinerU 裁剪图(content_list 的 `img_path` 本来就指向它们)
发给 `qwen-vl-max` 问一个窄问题,结构化回答映射到 `mergeTable` 或 `dismiss`。
不提供文本兜底路径——首末行摘要不足以核对表格行的真实归属,错合比漏合更糟。要点:

- 视觉模型**只输出决策,不产内容字符**——合并仍走行级保真闸,不碰纯削减红线。
- 未提供 `imageDir` → `split_table` 整体跳过,表格原样保留。
- 无图 / 无 key / 视觉模型不可用 / 判决被闸门拒 → 搁置该疑点(不阻塞其余修复)。
- 实测两份真实文档 7 判 7 对(5 真续表合并 + 2 假续表判误报,含 rowspan 列数不等、
  文控页同位置异表等困难形态),单次约 2k token。

### 守卫与终止

- **队列弹空才结束**:有修复操作、未被判误报的疑点全部处理完,循环才到底。
- **误报裁决集**:已判误报的疑点在重新探测时排除,防止同一误报反复入列、循环不收敛。
- **硬上限**:`maxIterations` 到顶强停;单疑点轮数耗尽 → 强制搁置(计入 dismissed)。
  默认上限随初始疑点数自适应——修复会解锁新疑点(实测总工作量约为初始数的 1.6 倍),
  固定常数对大文档必然截断。
- **防震荡**:合并产物禁止立刻拆分,拆分产物对禁止立刻合并回去。
- **矛盾决策守卫**:同一条回复同时调 dismiss 和变更操作 → 整体驳回,把矛盾点回灌给
  LLM 强制重裁(实测 LLM 会把「应 drop」的分析写进 dismiss 理由又并行调 drop)。
  每个变更操作都带一句话依据进审计日志。
- **联合裁决**:强关联疑点归并为一次裁决——同级编号兄弟组的 `missed_heading` 一起判
  (防止逐个裁决忽升忽不升),同文 `page_artifact` 一起判(同一页眉文本要删全删、
  要留全留)。
- 出口合格判定全部是机器检查:队列空 ∧ 保真 ∧ 疑点数 ≤ 输入 ∧ 几何可定位;
  任一不满足 → fail-open。

## LLM 接入与环境变量

LLM 全部走裸 HTTP(`reqwest`),零 SDK 依赖。

| 变量 | 必需 | 用途 |
|---|---|---|
| `DEEPSEEK_APIKEY` | 是 | 文本裁决主力。也接受 `RAGENT_DEEPSEEK_APIKEY`。缺失时 refine 直接 fail-open |
| `DEEPSEEK_BASE_URL` | 否 | 默认 `https://api.deepseek.com`;可指向私有化部署的 OpenAI 兼容端点 |
| `DEEPSEEK_MODEL` | 否 | 默认 `deepseek-v4-pro`;换模型时进程内缓存自动按模型名隔离 |
| `QWEN_APIKEY` | 视觉裁决需要 | 跨页拆表的 Qwen-VL 裁决;缺失则该类疑点搁置 |
| `QWEN_BASE_URL` | 否 | 默认 DashScope OpenAI 兼容端点 |
| `QWEN_VISION_MODEL` | 否 | 默认 `qwen-vl-max` |
| `MINERU_REFINE_PORT` | 否 | HTTP server 端口,默认 8771 |

**完全私有化部署**:文本与视觉两条链路的端点、模型名都可覆盖,文档不出内网。用 vLLM /
SGLang 等 OpenAI 兼容框架自建服务,把 `DEEPSEEK_BASE_URL` / `QWEN_BASE_URL` 指过去即可。
要求:文本端点须支持 **tool-call**(`tool_choice: "required"`),视觉端点须支持多图输入。
裁决质量未在私有模型上基准测试过——保真闸与 fail-open 仍然兜底(改坏会被回滚,最差原样
返回),但误报率/修复率可能与默认模型不同,建议先拿几份真实文档对比 `report`。

CLI 与 HTTP server 启动时自动加载当前目录的 `.env`;作为库调用时请自行设置环境变量
(或在宿主程序里加载 `.env`)。

实现要点(影响成本与可复现性):

- **DeepSeek 文本裁决**:`temperature: 0` + 关闭 thinking(可复现、省 reasoning token);
  `tool_choice: "required"` 强制每轮必选一个操作,天然禁止输出正文;tool-call 参数先过
  JSON 修复再解析,兜偶发坏 JSON。system prompt 与文档 outline 放消息前缀且每轮不变,
  命中 DeepSeek input cache(命中价约为未命中的 1/120)。
- **Qwen-VL 视觉裁决**:图走 base64 data URL,`temperature: 0`,回复按结构化 JSON 解析。
- **容错**:网络错误 / 429 / 5xx 自动重试;单疑点故障只搁置自身不毁全局
  (整轮零成功才 fail-open)。
- **性能**:疑点默认 8 路并行裁决;常见疑点的上下文(±2 邻居、跨页整页)预载进首条消息,
  省去额外的观察轮次。

### 成本参考

三份真实文档的实测消耗(`report.tokenUsage`),按 DeepSeek-V4-Pro 现行价格
(2026-06 起:输入缓存命中 ¥0.025 / 未命中 ¥3、输出 ¥6,均为每百万 token)估算:

| 文档 | 裁决轮数 | prompt | completion | 估算花费 |
|---|---|---|---|---|
| 战略管理规范(大,content_list 334 KB) | 66 | 195 万 | 2.7 万 | ¥0.5 ~ 1.2(全不命中缓存封顶 ¥6) |
| 组织绩效管理规范 | 8 | 7.7 万 | 0.1 万 | < ¥0.25 |
| 管理评审程序 | 7 | 6.7 万 | 0.1 万 | < ¥0.25 |

循环按命中 input cache 设计(system prompt 与 outline 是稳定前缀),多轮迭代里绝大部分
prompt token 走命中价,实际花费通常远低于全未命中的上限。Qwen-VL 表格裁决单次约 2k
token,可忽略。无疑点的文档零 LLM 调用,花费为零。

## CLI

```bash
cargo install mineru-refine --features bin
cat content_list.json | mineru-refine > refined.json
# stdin 也可以是包对象:{ "items": [...], "sha256"?, "maxIterations"?, "imageDir"? }
```

## 开发

```bash
just test         # cargo test:全程 mock LLM,不打网络(160+ 个测试)
just check        # clippy -D warnings + fmt --check
just smoke-vl     # 冒烟:真实 Qwen-VL 判表(三对真实表格图,需 key)
just js-build     # JS 绑定本地构建(napi)
just py-dev       # Python 绑定构建并装进 .venv
```

测试覆盖六类性质:① golden fixtures ② 保真(`C_out ⊆ C_in`)③ table_body 逐字节不变
④ 疑点数单调下降 ⑤ 几何可定位 ⑥ 幂等。没有"干净原文"做 ground truth 时,
"保真 + 疑点下降 + 幂等"是能拿到的最强代理指标。

### 真实数据工作流

```bash
# .env 需有 MINERU_API_TOKEN
just mineru-fetch               # 把 test_data/source/ 下的 PDF/DOC 交 MinerU 官方 API 解析,
                                # 产物落盘 test_data/mineru/<stem>/
                                # (--force 重跑;--batch <id> 复用已完成的 batch)
just refine-real                # 对全部真实 content_list 跑 refine(真 LLM),
                                # 输出 test_data/refined/<stem>/,打印疑点前后对比
just refine-real <stem>         # 只跑某个文档;REFINE_MAX_ITERATIONS 可调上限
```

`test_data/refined/<stem>/` 是对应 MinerU 产物目录的 **drop-in 替身**:images/、layout.json
等原样镜像(`img_path` 引用不断链),`content_list.json` 替换为清洗版,`full.md` 从清洗后
items 确定性重渲染,另附 `refine_report.json`(审计:ops/dismissed/removedSpans/tokens)。

## 目录结构

```
crates/mineru-refine/            # Rust core
  src/types.rs                   #   MineruItem(保序 JSON 对象)/ WorkItem / OpCall / RefineReport
  src/id.rs                      #   内部稳定 ID(出口剥除,绝不进输出 schema)
  src/detect.rs                  #   确定性异常探测器 → 疑点队列
  src/mechanical.rs              #   机械清洗 pass(表格噪声,确定性、不打 LLM)
  src/ops.rs                     #   10 个削减/重组操作 + 保真闸 + 回滚
  src/extrachar.rs               #   赘字/衍字白名单(deleteChar 的准入)
  src/invariant.rs               #   保真 / table_body / 几何校验
  src/confusion.rs               #   混淆修正层(opt-in,fixOcrConfusion)
  src/garbled.rs                 #   乱码表重转写层(opt-in,rewriteGarbledTables)
  src/agent_loop.rs              #   确定性外层循环 + LLM tool-use + 守卫
  src/llm.rs                     #   裸 reqwest:DeepSeek + Qwen-VL(trait 注入,测试 mock)
  src/markdown.rs                #   清洗后 items → full.md 确定性重渲染
  src/refine.rs                  #   入口:fail-open + 缓存 + 出口闸门
  src/bin/{cli,server}.rs        #   stdin/stdout 与 HTTP transport
  examples/{qwen_smoke,refine_real}.rs   # 真实数据工作流
  tests/                         #   六类性质测试 + 守卫/绑定回归(mock LLM)
bindings/python/                 # PyO3 → pip install mineru-refine
bindings/js/                     # napi-rs → bun add mineru-refine
scripts/mineru_fetch.ts          # MinerU 官方 API 拉取测试产物(Bun)
```

## 边界(有意不做的)

- **不加字**:OCR 纠错、补图注等内容生成一概不做——纯削减让保真完全可证。
  返回值预留了 `provenance` 通道(逐字登记 AI 新增字符),供未来扩展。
- **不修表格列对齐**:合并表格时不补空单元格、不重排单元格(见修复操作集说明)。
- 不感知任何下游业务模型;不替代 MinerU 的解析,只做其输出的后处理。

## License

MIT
