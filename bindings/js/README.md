# mineru-refine

[MinerU](https://github.com/opendatalab/MinerU) 解析结果的后处理器(linter / fixer)。

接收 MinerU 的 `content_list`(item 对象数组),修掉解析产生的高频结构问题——伪标题、
跨页断句、跨页拆表、混入正文的页眉页脚、LaTeX / 链接残留——返回**同 schema** 的
content_list,下游零改动。

两条核心承诺:

- **绝不新增一个字**:只做削减与重组,输出的每个内容字符都来自输入,由机器逐步校验,
  违反即自动回滚(不是靠 prompt 约束 LLM)。
- **fail-open**:任何异常 / LLM 不可用 → 原样返回输入(`report.failOpen === true`),
  绝不搞崩上游。

本包是 Rust 核心实现的 napi-rs 原生插件(预编译,无需本地 Rust 工具链),支持
Bun / Node ≥ 18,与 Python / Rust / HTTP 版选项和返回值完全同构。

## 安装

```bash
bun add mineru-refine    # 或 npm i mineru-refine
```

## 用法

```ts
import { refine, renderMarkdown, detectSuspects } from "mineru-refine";

const { items, report } = await refine(contentList, {
  sha256,                      // 可选:源文件 SHA256,提供则启用进程内缓存
  maxIterations,               // 可选:修复循环硬上限,默认随疑点数自适应
  concurrency: 8,              // 可选:并行裁决的疑点数,1 = 严格串行
  imageDir: "/abs/mineru/out", // 可选:MinerU 产物目录,提供则启用跨页拆表的视觉裁决
  fixOcrConfusion: false,      // 可选:opt-in 的 OCR 字符混淆修正层(CE0→CEO 等)
  extraConfusionPairs: [],     // 可选:混淆准入名单补充对,如 ["0D"]
  rewriteGarbledTables: false, // 可选:opt-in 的重度乱码表视觉重转写层(需要 imageDir)
  degradeGarbledTables: false, // 可选:opt-in 的乱码表降级兜底(救不回的表降级为图片)
});

items;    // 清洗后的 content_list(同 schema,未知字段原样透传)
report;   // 审计报告:iterations / opCounts / dismissed / removedSpans
          //          / violations / tokenUsage / failOpen
          //          (开 fixOcrConfusion 后另有 confusionFixes 等,见主 README)
```

删除的每段内容都留痕于 `report.removedSpans`(itemId / 原文 / 原因),逐条可审计。
`fixOcrConfusion: true` 开启混淆修正层(直接替换,LLM 提案 + 机械闸门),
开启后输出契约从"只删不增"变为双契约——详见主 README 的「混淆修正层」一节。
`rewriteGarbledTables: true` 开启重度乱码表的视觉重转写层(机械检测整表认废的表,
Qwen-VL 对照截图逐单元格重转写,全量进 report.tableRewrites)——详见主 README 的
「乱码表重转写层」一节。
`degradeGarbledTables: true` 开启乱码表降级兜底(纯机械,跑在重转写层之后:仍判废且
有 img_path 的表整项降级为 image,report.tableDegraded 计数)——详见主 README 的
「降级兜底」一节。

独立工具函数(都不调 LLM):

```ts
renderMarkdown(items);   // items → full.md 文本(确定性重渲染)
detectSuspects(items);   // 仅探测疑点,返回疑点列表
```

## 环境变量

| 变量 | 必需 | 用途 |
|---|---|---|
| `DEEPSEEK_APIKEY` | 是 | 文本裁决(DeepSeek)。缺失时 refine 直接 fail-open |
| `QWEN_APIKEY` | 视觉裁决需要 | 跨页拆表的 Qwen-VL 裁决;缺失则该类疑点跳过,表格原样保留 |

库本身不读 `.env`,请在宿主程序里设置环境变量(或自行加载 `.env`)。

## 本地构建

```bash
bun install && bun run build   # 产出 mineru-refine.<platform>.node + index.js / index.d.ts
bun run test
```

发布:仓库根 `just publish-js`(发布本机平台子包 + 主包;linux 子包在 linux 机器上跑
同一条命令补发)。

探测器、修复操作集、保真闸门的完整设计文档见
[仓库 README](https://github.com/LcpMarvel/mineru-refine#readme)。

## License

MIT
