# mineru-refine

[MinerU](https://github.com/opendatalab/MinerU) 解析结果的后处理器(linter / fixer)。

接收 MinerU 的 `content_list`(item 对象数组),修掉解析产生的高频结构问题——伪标题、
跨页断句、跨页拆表、混入正文的页眉页脚、LaTeX / 链接残留——返回**同 schema** 的
content_list,下游零改动。

两条核心承诺:

- **绝不新增一个字**:只做削减与重组,输出的每个内容字符都来自输入,由机器逐步校验,
  违反即自动回滚(不是靠 prompt 约束 LLM)。
- **fail-open**:任何异常 / LLM 不可用 → 原样返回输入(`report["failOpen"] == True`),
  绝不搞崩上游。

本包是 Rust 核心实现的 PyO3 原生绑定,与 JS / Rust / HTTP 版选项和返回值完全同构。

## 安装

```bash
pip install mineru-refine
```

需要 Python ≥ 3.9。

## 用法

```python
import json
import mineru_refine

items = json.load(open("content_list.json"))

result = mineru_refine.refine(
    items,                              # content_list(list[dict])
    sha256="...",                       # 可选:源文件 SHA256,提供则启用进程内缓存
    max_iterations=None,                # 可选:修复循环硬上限,默认随疑点数自适应
    concurrency=8,                      # 可选:并行裁决的疑点数,1 = 严格串行
    image_dir="/abs/mineru/out",        # 可选:MinerU 产物目录,提供则启用跨页拆表的视觉裁决
)

result["items"]    # 清洗后的 content_list(同 schema,未知字段原样透传)
result["report"]   # 审计报告:iterations / opCounts / dismissed / removedSpans
                   #          / violations / tokenUsage / failOpen
```

删除的每段内容都留痕于 `report["removedSpans"]`(itemId / 原文 / 原因),逐条可审计。

独立工具函数(都不调 LLM):

```python
mineru_refine.render_markdown(items)    # items → full.md 文本(确定性重渲染)
mineru_refine.detect_suspects(items)    # 仅探测疑点,返回疑点列表
```

## 环境变量

| 变量 | 必需 | 用途 |
|---|---|---|
| `DEEPSEEK_APIKEY` | 是 | 文本裁决(DeepSeek)。缺失时 refine 直接 fail-open |
| `QWEN_APIKEY` | 视觉裁决需要 | 跨页拆表的 Qwen-VL 裁决;缺失则该类疑点跳过,表格原样保留 |

库本身不读 `.env`,请在宿主程序里设置环境变量(或自行加载 `.env`)。

## 本地构建

```bash
just py-dev        # 仓库根:构建 wheel 并装进 bindings/python/.venv
just publish-py    # 发布 PyPI:当前平台 wheel + sdist(需 MATURIN_PYPI_TOKEN)
```

探测器、修复操作集、保真闸门的完整设计文档见
[仓库 README](https://github.com/LcpMarvel/mineru-refine#readme)。

## License

MIT
