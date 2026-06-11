# mineru-refine (JS/TS)

MinerU 解析结果的 linter/fixer——Rust core 的 napi-rs 原生绑定，Bun / Node ≥18 直接 import。
content_list 进、同 schema 出，只做削减与重组，绝不新增一个字（机器闸门保证 `C_out ⊆ C_in`）。
详见仓库根 README。

```bash
bun add mineru-refine    # 或 npm i mineru-refine
```

```ts
import { refine, renderMarkdown, detectSuspects } from "mineru-refine";

const { items, report } = await refine(contentList, {
  sha256,                      // 可选：启用进程内缓存
  maxIterations,               // 可选：外层循环硬上限，默认自适应
  concurrency: 8,              // 可选：疑点并行裁决数
  imageDir: "/abs/mineru/out", // 可选：MinerU 产物目录，提供则启用 split_table 视觉裁决
});

renderMarkdown(items);   // items → full.md 文本
detectSuspects(items);   // 仅探测疑点，不打 LLM
```

环境变量：`DEEPSEEK_APIKEY`（必需）、`QWEN_APIKEY`（视觉裁决需要）。
fail-open：LLM 不可用/任何异常 → 原样返回输入（`report.failOpen === true`），绝不搞崩上游。

## 本地构建

```bash
bun install && bun run build   # 产出 mineru-refine.<platform>.node + index.js/index.d.ts
bun run test
```

发布：仓库根 `just publish-js`（发布本机平台子包 + 主包；linux 子包在 linux 机器上跑同一条命令补发）。
核心逻辑、保真闸、探测器的设计文档见[仓库根 README](https://github.com/LcpMarvel/mineru-refine#readme)。
