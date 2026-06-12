# TODO — 基于 test_data 三份文档精修结果的残留问题分析

> 数据来源：`test_data/mineru/`（原始）vs `test_data/refined/`（精修后），2026-06-12 分析。
> 三份文档：MN-JZY-001 战略管理规范（71 页）、MN-ZBZ-003 管理评审程序（9 页）、
> MN-ZBZ-047 组织绩效管理规范（17 页）。
>
> 已完成（2026-06-12，core 0.9.0 / confusion c3）：
> ① 赘字/衍字删除（extra_char 探测器 + deleteChar op，全走 LLM 裁决，无机械兜底）；
> ② confusionObservations 闭环回灌（一轮二次候选，三道闸门照旧）；
> ③ 中文形近字准入名单扩充（校较/酒源/军率）+ 全文频率投票（加白排误 + 拉丁 token 少数派投票）。
>
> 已完成（2026-06-12，core 0.10.0 / garbled g1）：
> ④ 重度乱码表视觉重转写层（`rewriteGarbledTables`，opt-in，见下「1.」原案与实现差异）。
>
> 已完成（2026-06-12，bugfix）：
> ⑤ 图注/图脚注静默丢失修复——`render_markdown`（markdown.rs）与 caption 探测器
>    （detect.rs）查的字段名是 `img_caption`/`img_footnote`，但 MinerU 真实字段是
>    `image_caption`/`image_footnote`，永远取到 null。后果：重渲染的 full.md **静默丢掉
>    全部图注**（JZY-001 实测 17→2）；caption_issue 对图片误判。`markdown_test.rs` 的
>    fixture 也写成 `img_caption`，与代码"错得一致"互相掩护，所以测试一直绿。已三处一并
>    改回真实字段名，重跑后 8 张图图注全部正确渲染、caption_issue 41→33。

## 1. 重度乱码表格的视觉重转写 ✅（core 0.10.0，`rewriteGarbledTables`）

实现为独立 opt-in 层（`garbled.rs`），跑在出口闸门后、混淆层前：

- [x] 机械检测：内嵌 6 万常用词词典（jieba top60k，`data/cn_words.txt`），对单元格汉字段
      做正向最大匹配算词覆盖率。标定（三份真实文档）：乱码表 0.46，最差正常表 0.61，
      阈值 0.55，全语料零误报。原案的「observation 密集」信号不需要——覆盖率单信号已分干净，
      且让本层与混淆层解耦
- [x] 复用 `imageDir`/`LoadImage` + Qwen-VL（`VisionClient::transcribe_table`），
      对照 img_path 截图逐单元格重转写
- [x] 三道机械闸门 + 全量留痕：①资格（空格/纯数值/短编号/词覆盖率正常的格不许动——
      实测视觉模型在 33 列宽表上会行列错位张冠李戴，这道闸门拦下全部错位提案）；
      ②结构（行列命中/无标签/长度上限/提案非纯数值/长度量级可比）；
      ③整表回归（重转写后覆盖率必须严格升高，否则整表回退）。
      每格进 `report.tableRewrites`（before 即撤销凭据）与 provenance（origin=garbled_table）
- [x] Midhuel→Michael 词级错误顺带解决（实测 ZBZ-047：25 格落地，覆盖率 0.46→0.77，
      含 Michael×2；冒烟入口 `cargo run --example garbled_smoke --features bin`）

## 2. 单元格内 LaTeX 残留 ✅（机械清洗 pass 第 6 件：`mech:cell_latex`）

`strip:latex_dollar` 只扫**独立文本** item，单元格内的 `$...$` 不碰。JZY-001 精修后
表格里仍躺着 `$\lambda _ { m i }$`、`$\lambda _ { m a x }$` 这类被 MinerU 误转的 LaTeX
片段——默认配置（两个 opt-in 层都关）下肉眼可见的脏点，且不依赖 LLM/VL，纯机械可清。

与原案差异：没走 strip op 扩展——cell 受「table_body 逐字节不变」保真闸约束，op 体系
动不了；落在机械清洗 pass（mechanical.rs，基线快照前），这也正好兑现"纯机械可清"。

- [x] cell 内 `$...$` 伪 LaTeX 包装剥除：已知符号命令换 Unicode（`\lambda`→`λ`、
      `\times`→`×`，原 strip 直接丢命令会把 λ 丢掉），样式包装命令（\mathsf 等）删除，
      花括号/公式内空白删除
- [x] 真公式不动：命中任何未知命令（\frac/\sum/\begin…）整段保留；裸 `$100$`
      （无命令无花括号）不动——可能是真美元金额；`\$` 转义定界符不碰（先于 unescape 跑）
- [x] 自校验 + 留痕：span 级字符预算校验（产物字符 ⊆ 原 span 字符 + 命令映射符号）+
      表级期望值严格比对，不过即整表回退；原 span 进 removedSpans
      （reason=`mech:cell_latex→替换文`，即撤销凭据）
- [x] 实测三份文档：JZY-001 恰命中两格 → `λ_mi`/`λ_max`，其余零误报零回退

## 3. 跨页段落粘连 / 漏标标题残留（探测器调优）✅（missed_heading 子项信号）

JZY-001 精修后仍有约 16 处疑似跨页断句，抽查分两类：

- 真断句：item471「--收割战 ‖ 防御战的核心」
- 漏 promote 的小节标题：「战略管理之"看趋势"」以 text 身份反复出现在页首

复盘结论（对照 origin.pdf 逐处复核）与原案有出入：

- **「真断句」抽查系误判，merge 侧不需要调**。item471「---收割战」是 p39 五连
  项目符列表（---防御战/---正面战/---游击战/---侧翼战/---收割战，OCR 吃掉部分短横）
  的末项，下一页「防御战的核心」是图示小标题——merge 了反而错。宽松口径重扫精修后
  全部 13 处跨页候选，全是「列表项相邻」「冒号引出列表/图表」形态，探测器静默全部正确。
- **「战略管理之看趋势」页眉泄漏已被家具同文佐证 drop 解决**（本轮重跑后无残留），
  不是 promote 缺口。
- **真正的召回缺口**：整组编号兄弟都被 MinerU 漏标时（「2.1确定竞争对手：」
  「2.2竞争情报收集：」…），原有「±1 相邻兄弟是标题」信号永不触发——
  JZY-001 原始数据 15 处全因此漏网，规模恰对上「约 16」。

- [x] missed_heading 新增**子项证据**信号：下一个内容块的编号以本块编号为真前缀
      （同数制，如 2.1 → 2.1.1）即标。表面闸照旧（≤30 内容字、无逗号/句末标点）。
      实测：JZY-001 命中 13 处（其余 2 处由兄弟信号互补覆盖），另两份文档零新增命中
- [x] 回归用例落地 detect_test.rs：正例（同页子项、跨页隔家具子项）+ 负例
      （非前缀编号、数制不匹配、收割战列表尾、冒号引出去符列表）
- [x] 端到端重跑 JZY-001：promote 9→18，「2.1确定竞争对手：」「2.2竞争情报收集：」
      「1.1定期评估」等抽查 11 处全部落地 level=2，子项形态残留清零，violations 0

## 4. 观察（已知，暂不单独立项）

- **mergeTable 合表 ≠ 修内容**：ZBZ-003 序号 1-10 输入信息表跨页合并成功，但续表本身
  OCR 烂——`3）亻`（拆字）、`2 3)`（并格错位）、`有效 性`（空格断字）原样保留。合表只接
  结构，单元格内乱码归 `rewriteGarbledTables`（opt-in）/「2.」管，默认配置不治。
- **跨运行非确定性**：mergeTable 走 Qwen-VL 视觉裁决，同一输入两次跑出 100 / 102 items
  不同结果（本轮把序号 10 续表 + 一条页眉清掉了，更优）。VL 裁决本质非确定，暂接受；
  若要稳定可考虑对 mergeTable 判定加缓存或降温。

## 不做（明确排除）

- **漏字补全**（华大科技「00085」缺一位）：要加字且无法从上下文确定加什么，
  只进 report 当质量信号
- **占位符 xxxxxX 归一**：源文档本来的脱敏占位内容，改了反而违背保真
- **页眉/页码清理**：已正确分类为 `header`/`page_number`，下游过滤即可
