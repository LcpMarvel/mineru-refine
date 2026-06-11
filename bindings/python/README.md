# mineru-refine (Python)

MinerU 解析结果的 linter/fixer——Rust core 的 PyO3 绑定。content_list 进、同 schema 出，
只做削减与重组，绝不新增一个字（机器闸门保证 `C_out ⊆ C_in`）。详见仓库根 README。

```bash
pip install mineru-refine
```

```python
import mineru_refine

result = mineru_refine.refine(
    items,                              # content_list（list[dict]）
    sha256="...",                       # 可选：启用进程内缓存
    max_iterations=None,                # 可选：外层循环硬上限，默认自适应
    concurrency=8,                      # 可选：疑点并行裁决数
    image_dir="/abs/mineru/out",        # 可选：MinerU 产物目录，提供则启用 split_table 视觉裁决
)
result["items"]    # 清洗后的 content_list（同 schema）
result["report"]   # iterations / opCounts / dismissed / removedSpans / violations / tokenUsage / failOpen

mineru_refine.render_markdown(items)    # items → full.md 文本
mineru_refine.detect_suspects(items)    # 仅探测疑点，不打 LLM
```

环境变量：`DEEPSEEK_APIKEY`（必需）、`QWEN_APIKEY`（视觉裁决需要）。
fail-open：LLM 不可用/任何异常 → 原样返回输入（`report["failOpen"] == True`），绝不搞崩上游。

## 本地构建

```bash
just py-dev        # 仓库根：构建 wheel 并装进 bindings/python/.venv
just publish-py    # 发布 PyPI：当前平台 wheel + sdist（需 MATURIN_PYPI_TOKEN）
```

核心逻辑、保真闸、探测器的设计文档见[仓库根 README](https://github.com/LcpMarvel/mineru-refine#readme)。
