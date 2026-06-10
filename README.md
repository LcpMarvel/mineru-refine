# mineru-refine

MinerU 输出的 **linter / fixer**。content_list（item 对象数组）进，同 schema 出。
LLM（DeepSeek v4-pro 裸 API）只负责"选哪个 op"，是否合格由机器闸门裁决。
完整设计见 [SPEC.md](./SPEC.md)。

## 保证

- **保真**：`C_out ⊆ C_in`——输出不含任何输入里没有的非空白内容字符（每 op 校验+回滚，出口再校验）。
- **table_body 逐字节不变**（未被 drop 的表）。
- **fail-open**：任何异常 / LLM 不可用 → 原样返回输入，绝不搞崩上游。
- **幂等**：清洗过的结果再跑一次输出逐字节不变（实测三份真实文档均成立）。无残留疑点的文档
  零 LLM 调用；含误报疑点（如各章节平行重复的真标题）的文档会被重新裁决为 dismiss——
  烧 token 但不改内容。同进程内提供 `sha256` 可直接命中缓存跳过。
- 删掉的内容全部留痕于 `report.removedSpans`，可审计。

## 用法

```bash
# .env 里需有 DEEPSEEK_APIKEY（refine 用）和 MINERU_API_TOKEN（拉真实解析产物用）

bun run m0                      # M0 冒烟：验 DeepSeek 多轮 tool-call 地基
bun test                        # 63 个测试（ops/探测器/不变式/loop/eval 六件套，全程 mock LLM）
bun run typecheck
```

### 真实数据工作流

```bash
bun run mineru:fetch            # 把 test_data/source/ 下的 PDF/DOC 交给 MinerU 官方 API 解析，
                                # 产物落盘 test_data/mineru/<stem>/content_list.json
                                # （--force 重跑；--batch <id> 复用已完成的 batch）
bun run refine:real             # 对全部真实 content_list 跑 refine（真 LLM），
                                # 输出 test_data/refined/<stem>/，打印疑点前后对比
bun run refine:real <stem>      # 只跑某个文档；REFINE_MAX_ITERATIONS 可调上限
```

`test_data/refined/<stem>/` 是对应 MinerU 产物目录的 **drop-in 替身**：images/、
layout.json 等原样镜像（content_list 里的 `img_path` 引用不断链），`content_list.json`
被替换为清洗版，`full.md` 从清洗后 items 确定性重渲染（与清洗版保持一致），
另附 `refine_report.json`（审计：ops/dismissed/removedSpans/tokens）。

```bash
```

### 作为库（D3：吃内存对象）

```ts
import { refine } from "./src/refine.ts";
const { items, provenance, report } = await refine(contentList, { sha256, maxIterations, concurrency });
```

性能：疑点默认 8 路并行裁决（`concurrency: 1` 可退回严格串行），常见疑点的上下文
（±2 邻居 / 跨页整页）预载进首条消息省观察轮次；DeepSeek 调用对网络错误/429/5xx
自动重试，单疑点故障只搁置自身不毁全局（全程零成功才 fail-open）。
实测 71 页 / 1004 items / 46 疑点：~86s（串行版 ~622s）。

### HTTP transport（docfuse 接入首选，SPEC §12）

```bash
bun run server                  # 默认端口 8771，MINERU_REFINE_PORT 可改
curl -X POST localhost:8771/refine -d '{"items":[...], "sha256":"..."}'
```

docfuse 侧在 `mineru_api.py::_parse_extracted_dir` 解析 `content_list.json` 之前调一次，
用返回的 `items` 替换即可；服务超时/不可用时用原始结果继续（fail-open 双保险）。

### CLI transport（备选）

```bash
cat content_list.json | bun run cli      # stdin JSON → stdout JSON
```

## 结构

```
src/types.ts      # MineruItem / WorkItem / OpCall / RefineReport
src/id.ts         # 内部稳定 ID（出口剥除，绝不进输出 schema）
src/detect.ts     # 确定性异常探测器 → worklist
src/ops/index.ts  # 7 个削减/重组 op + 保真闸 + 回滚
src/invariant.ts  # C_out ⊆ C_in / table_body / 几何校验
src/loop.ts       # 确定性外层循环 + LLM tool-use + 守卫（防震荡/dismiss 集/硬上限）
src/deepseek.ts   # 裸 fetch v4-pro（thinking disabled / tool_choice required / temp 0）
src/refine.ts     # 入口：fail-open + 缓存 + 出口闸门
src/server.ts     # HTTP transport
src/cli.ts        # stdin/stdout transport
```
