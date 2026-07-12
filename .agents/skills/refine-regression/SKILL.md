---
name: refine-regression
description: 整体跑一遍真实测试用例（refine_real，真 LLM），并对比前后产物——content_list.json、refine_report.json、full.md——输出回归报告。当用户想"整体跑一下测试用例并对比前后数据"、验证清洗效果有没有回归时使用。
model: opus
---

# refine 回归测试：整跑 + 前后对比

把 `test_data/mineru/<stem>`（MinerU 原始产物）整体过一遍 refine（真 LLM），输出到
`test_data/refined/<stem>`，并做两层对比：

1. **输入 vs 本次输出**（mineru → refined）：这次清洗到底改了什么。
2. **上次输出 vs 本次输出**（refined_prev → refined）：代码改动带来的回归/改进。

`test_data/` 不进 git，"上次"只能靠文件系统快照（`test_data/refined_prev/`）。

## 步骤

### 1. 前置检查

- `.env` 里要有 `QWEN_APIKEY`（refine_real 通过 dotenvy 加载；split_table / 视觉层都要用）。
  缺了就停下来告诉用户，不要空跑。
- `test_data/mineru/` 必须非空；为空提示先跑 `just mineru-fetch`。

### 2. 快速门禁（mock 测试）

先跑 `just test`（全程 mock，不打网络）。红了就停：先修单测，别烧真 LLM 的钱。

### 3. 快照上一次结果

```bash
rm -rf test_data/refined_prev
[ -d test_data/refined ] && cp -R test_data/refined test_data/refined_prev
```

没有 `test_data/refined/`（首跑）就跳过，后面只做"输入 vs 输出"对比。

### 4. 整跑

```bash
just refine-real 2>&1 | tee /tmp/refine_real.log
```

- 真 LLM，每个文档可能要几分钟——用 `run_in_background` 跑，期间 tail 日志看进度。
- 用户只想跑某一个文档时：`just refine-real <stem>`（refine_real 只会重写该文档的输出目录，
  其余文档保留上次结果，对比时也只看这个 stem）。
- 可选环境变量（用户明确要求时才加）：`REFINE_MAX_ITERATIONS=N`、
  `REFINE_FIX_CONFUSION=1`（OCR 混淆层）、`REFINE_REWRITE_GARBLED=1`（乱码表重转写层）。
- 日志本身就含每个文档的输入/输出疑点、耗时、ops、删除片段、混淆/重转写明细——这是
  报告的重要素材，别丢。

### 5. 汇总对比

```bash
python3 .Codex/skills/refine-regression/summarize.py [stem]
```

逐文档打印：items 数量（输入/本次/上次）、refine_report 关键指标及上次对照
（iterations / opCounts / dismissed / violations / failOpen / removedSpans / tokens）、
full.md 与 content_list.json 的 diff 量级。

**只对量级非零的"上次→本次"差异深入看 diff**，full.md 很大，别整篇倒进上下文：

```bash
git diff --no-index test_data/refined_prev/<stem>/full.md test_data/refined/<stem>/full.md
```

逐 hunk 判断是改进（修掉了之前没修的）还是回归（之前修好的又坏了）。
content_list.json 的 diff 同理，必要时配合 `refine_report.json` 里的 removedSpans /
confusionFixes / tableRewrites 解释每处变化的来源。

注意：MinerU 原版 full.md 与其 content_list 本就有少量出入；refined 的 full.md 是从清洗后
items 确定性重渲染的，对比"输入→输出"时以 content_list 为准，full.md 的格式性差异
（如渲染风格）不算回归。

### 6. 报告

输出一份 markdown 总结，按文档分节：

- 总览表：每个文档的 items 变化、耗时、token 消耗、violations / failOpen（**这两项相比
  上次不该上升，升了就是红灯**）。
- 上次→本次有差异的文档：逐条列出 full.md / content_list 的实质变化，标注「改进」「回归」
  「中性（非确定性抖动）」。真 LLM 输出有随机性，孤立的小抖动要点出来但别误报为回归。
- 输入→输出：本次清洗做了什么（ops 分布、删除片段、各层落地/拒绝数）。

最后给结论：本次代码状态相比上次快照是否安全。
