# mineru-refine（Claude Code plugin）

**PDF/DOC/PPT/图片 进，干净 Markdown 出。** 一个 skill 包办全链路：

1. **解析** —— 把文件交 [MinerU](https://mineru.net) 官方 API，得到 `content_list.json` + images；
2. **清洗** —— 用 [`mineru-refine`](https://github.com/LcpMarvel/mineru-refine) 后处理，修掉伪标题、
   跨页断句、跨页拆表、表内续行、页面家具、残留符号、OCR 形近字、乱码表……
   全程**机器校验保真**（输出每个字符都来自输入）、**fail-open**（出错原样返回不搞崩）。

产出一个 **drop-in 替身目录**：`images/`、`layout.json` 原样镜像，`content_list.json` 换成清洗版，
`full.md` 确定性重渲染，外加 `refine_report.json`（做了什么、删了什么、花了多少 token，逐条可审计）。

本 plugin 默认把三层 opt-in 清洗（OCR 形近字修正 / 乱码表视觉重转写 / 乱码表降级兜底）**全开**，
追求最干净的产物；代价是输出契约从"只删不增"转为双契约（所有替换全量留痕、可程序化撤销）。

## 安装

marketplace 清单在仓库根 `.claude-plugin/marketplace.json`（plugin 本体在 `plugin/` 子目录）。
在 Claude Code 里：

```
/plugin marketplace add LcpMarvel/mineru-refine
/plugin install mineru-refine@mineru-refine
```

`LcpMarvel/mineru-refine` 是 GitHub 仓库 shorthand，等价于完整 URL
`https://github.com/LcpMarvel/mineru-refine`。本地开发可改用本地路径：
`/plugin marketplace add /abs/path/to/mineru-refine`（仓库根，不是 `plugin/`）。
改了 SKILL/脚本后用 `/reload-plugins` 热加载。

## 用法

装好后，直接对 Claude Code 说：

> 清洗这个 PDF：/abs/path/to/报告.pdf

skill（`mineru-prime`）会引导你完成首次 key 配置，然后跑完解析 + 清洗，把产物放进
`mineru-refine-out/refined/`，并汇报疑点前后对比与审计摘要。

## 需要三个 key

首次运行时 skill 会向你索取并写入工作目录 `.env`（持久化，下次免输）：

| key | 必需性 | 申请 / 用途 |
|---|---|---|
| `MINERU_API_TOKEN` | 解析必需 | https://mineru.net —— 官方 API 解析配额 |
| `DEEPSEEK_APIKEY` | 清洗必需 | https://platform.deepseek.com —— refine 文本裁决；缺则 fail-open |
| `QWEN_APIKEY` | 强烈建议 | DashScope —— 跨页拆表视觉裁决 + 乱码表视觉重转写；缺则这两类搁置 |

> 私有化部署：`DEEPSEEK_BASE_URL` / `QWEN_BASE_URL` / 各 `*_MODEL` 均可在 `.env` 覆盖，
> 指向自建的 OpenAI 兼容端点。详见 mineru-refine 主仓 README。

## 依赖

- **[bun](https://bun.sh)** —— 运行时（skill 会检测，缺则引导安装）。
- **`unzip`** —— 解 MinerU 产物 zip（macOS/Linux 自带）。
- `mineru-refine` npm 原生绑定 —— skill 首次运行自动 `bun install`（含预编译二进制，无需 Rust 工具链）。

## 结构

```
plugin/
├── .claude-plugin/
│   ├── plugin.json               # plugin manifest
│   └── marketplace.json          # 自带 marketplace（指向本目录）
├── bin/                          # 启用时自动进 PATH，skill 直接当命令调
│   ├── mineru-prime-fetch        #   → scripts/mineru_fetch.ts
│   └── mineru-prime-refine       #   → scripts/refine.ts（首次自动 bun install）
├── skills/mineru-prime/SKILL.md  # 编排流程
└── scripts/
    ├── mineru_fetch.ts           # MinerU 官方 API 解析（单文件 → 产物目录）
    ├── refine.ts                 # 调 mineru-refine 清洗 → drop-in 替身目录 + 报告
    └── package.json              # 声明 mineru-refine 依赖
```
