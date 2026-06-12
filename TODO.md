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

## 2. 跨页段落粘连 / 漏标标题残留（探测器调优）

JZY-001 精修后仍有约 16 处疑似跨页断句，抽查分两类：

- 真断句：item471「--收割战 ‖ 防御战的核心」
- 漏 promote 的小节标题：「战略管理之"看趋势"」以 text 身份反复出现在页首

- [ ] 把这 16 处做成回归用例，调 merge/promote 探测器的召回阈值

## 不做（明确排除）

- **漏字补全**（华大科技「00085」缺一位）：要加字且无法从上下文确定加什么，
  只进 report 当质量信号
- **占位符 xxxxxX 归一**：源文档本来的脱敏占位内容，改了反而违背保真
- **页眉/页码清理**：已正确分类为 `header`/`page_number`，下游过滤即可
