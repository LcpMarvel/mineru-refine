---
name: mineru-prime
description: PDF/DOC/PPT/图片 进、干净 Markdown 出——比原生 MinerU 产物更可用。先用 MinerU 官方 API 解析成 content_list，再用 mineru-refine 后处理清洗（伪标题、跨页断句、跨页拆表、乱码表、OCR 形近字、页面家具…），全程机器校验保真、fail-open 不搞崩。当用户说"清洗这个 PDF"、"把这份文档转成干净 markdown"、"解析并清洗"、"用 mineru-prime/mineru-refine 处理"时使用。需要三个 key：MINERU_API_TOKEN / DEEPSEEK_APIKEY / QWEN_APIKEY。
model: opus
---

# PDF → 干净 Markdown：MinerU 解析 + mineru-refine 清洗

全链路两段：**① MinerU 官方 API 把文件解析成 `content_list.json` + images** →
**② mineru-refine 后处理清洗，产出 drop-in 替身目录**（清洗版 content_list + 重渲染 full.md + 审计报告）。

两段都靠仓库脚本驱动，运行时统一 **bun**。本 plugin 默认把三层 opt-in 清洗
（OCR 形近字修正 / 乱码表视觉重转写 / 乱码表降级兜底）**全开**——最大限度清洗，
输出契约从"只删不增"转为双契约（有逾点全部留痕、可审计、可程序化撤销）。

## 前置：两个命令 + 工作目录

- **命令**：plugin 的 `bin/` 已自动进 PATH，直接当命令调，**不用关心脚本在哪、不依赖 `$CLAUDE_PLUGIN_ROOT`**：
  - `mineru-prime-fetch <输入文件> <产物目录>` —— MinerU 解析
  - `mineru-prime-refine <产物目录> [清洗输出目录]` —— refine 清洗（首次运行自动装依赖）

  若极少数环境里这两个命令不在 PATH（`command -v mineru-prime-refine` 为空），
  回退用 `$CLAUDE_PLUGIN_ROOT/bin/mineru-prime-refine`，再不行用
  `find ~/.claude/plugins -name mineru-prime-refine 2>/dev/null` 定位。
- **工作目录** `$WORKDIR`：放 `.env` 和产物的地方。默认用户当前目录下的 `mineru-refine-out/`
  （不存在就建）；用户指定了输出位置就用用户的。**两个命令都从 `$WORKDIR` 运行**
  （`cd "$WORKDIR" && mineru-prime-...`），因为 bun 从 cwd 自动加载 `.env` 里的 key。

## 步骤

### 1. 确认输入文件

- 用户给一个待处理文件路径。支持 `pdf / doc / docx / ppt / pptx / png / jpg / jpeg / html`。
- 路径不存在或类型不支持 → 停下来问用户，不要瞎跑。

### 2. 环境检查 + 三个 key（写 .env 持久化）

- **bun**：`command -v bun`。没有就停下来引导安装（`curl -fsSL https://bun.sh/install | bash`），
  装好再继续——别用别的运行时凑合。
- **key**：检查 `$WORKDIR/.env` 是否已含以下三个。**缺哪个就向用户要哪个**，
  然后写进 `$WORKDIR/.env`（持久化，下次免输）。已存在的不要覆盖、不要打印 key 值。

  | key | 必需性 | 用途 |
  |---|---|---|
  | `MINERU_API_TOKEN` | 解析必需 | MinerU 官方 API 解析（https://mineru.net 申请）|
  | `DEEPSEEK_APIKEY` | 清洗必需 | refine 文本裁决主力；缺则 refine 直接 fail-open |
  | `QWEN_APIKEY` | 清洗强烈建议 | 跨页拆表视觉裁决 + 乱码表视觉重转写；缺则这两类整体搁置 |

  三个都给齐再往下；用户坚持不给 `QWEN_APIKEY` 就照常跑，但提醒他视觉相关清洗会被搁置。

### 3. 解析（MinerU）

```bash
cd "$WORKDIR" && mineru-prime-fetch "<输入文件绝对路径>" "$WORKDIR/mineru"
```

- 这一步会上传文件、轮询 MinerU（大文件可能几分钟，脚本内置 20 分钟超时），下载 zip 解包，
  归一化出 `$WORKDIR/mineru/content_list.json` + `images/`。
- 脚本**末行是一行 JSON**（`{ok, stem, outDir, items}`）——读它确认产物与 item 数。
- 失败（HTTP 错 / 解析 failed / 超时）会抛异常，原样把错误告诉用户，别吞。

### 4. 清洗（mineru-refine，三层全开）

```bash
cd "$WORKDIR" && mineru-prime-refine "$WORKDIR/mineru" "$WORKDIR/refined"
```

首次运行会自动 `bun install` 把 `mineru-refine` 原生绑定装进 plugin 的 `scripts/node_modules`（幂等，之后秒过）。

- 产出 drop-in 替身目录 `$WORKDIR/refined/`：`images/`、`layout.json` 原样镜像
  （`img_path` 不断链），`content_list.json` 是清洗版，`full.md` 确定性重渲染，
  另有 `refine_report.json` 审计报告。
- 脚本**末行是一行 JSON 摘要**——读它拿前后疑点数、ops、留痕数、token、failOpen 等。
- `failOpen=true` 意味着触发了兜底（异常/key 缺失/LLM 不可用），`refined` 内容即原始输入——
  **要明确告诉用户没真正清洗到**，并查 `refine_report.json` 找原因（多半是 `DEEPSEEK_APIKEY` 问题）。

### 5. 汇报

给用户一段简明小结，基于第 4 步的摘要 JSON：

- **疑点 前 → 后**（`suspectsBefore → suspectsAfter`）：清洗整体效果。
- **各修复操作次数**（`opCounts`）+ **降级/删除留痕条数**（`removedSpans`）。
- **三层 opt-in 落地量**：`confusionFixes`（OCR 形近字替换）/ `tableRewrites`（乱码表整格重转写）
  / `tableDegraded`（救不回降级为图片的表数）。
- **token 花费**（`tokenUsage`）与 **是否 failOpen**。
- **产物位置**：`$WORKDIR/refined/`——指明 `full.md` 是干净 markdown 成品，
  `content_list.json` 可直接 drop-in 替换原 MinerU 结果喂下游，`refine_report.json` 可逐条审计。

不要轮询、不要后台挂着；跑完即汇报。

## 硬约束（照搬 mineru-refine 的承诺，别违背）

- **fail-open 优先**：任何异常都应让脚本大声抛、原样把错误转达用户，**绝不静默吞**或伪造成功。
- **不替用户编内容**：full.md / content_list 全部来自 refine 的产物，不要手工增删其中文字。
- **key 不外泄**：不在对话或日志里回显任何 key 值；写 `.env` 即可。
