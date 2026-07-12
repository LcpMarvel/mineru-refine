# mineru-refine

> 🌏 English version: [README.md](./README.md)

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
    fix_ocr_confusion=False,            # 可选:opt-in 的 OCR 字符混淆修正层(CE0→CEO 等)
    extra_confusion_pairs=None,         # 可选:混淆准入名单补充对,如 ["0D"]
    rewrite_garbled_tables=False,       # 可选:opt-in 的重度乱码表视觉重转写层(需要 image_dir)
    degrade_garbled_tables=False,       # 可选:opt-in 的乱码表降级兜底(救不回的表降级为图片)
    model_config=None,                  # 可选:配置驱动换模型(见下文「换模型」)
    chat=None,                          # 可选:自定义文本 LLM 回调(逃生口;优先级高于 model_config)
    vision=None,                        # 可选:自定义视觉 LLM 对象(逃生口)
)

result["items"]    # 清洗后的 content_list(同 schema,未知字段原样透传)
result["report"]   # 审计报告:iterations / opCounts / dismissed / removedSpans
                   #          / violations / tokenUsage / failOpen
                   #          (开 fix_ocr_confusion 后另有 confusionFixes 等,见主 README)
```

删除的每段内容都留痕于 `report["removedSpans"]`(itemId / 原文 / 原因),逐条可审计。
`fix_ocr_confusion=True` 开启混淆修正层(直接替换,LLM 提案 + 机械闸门),
开启后输出契约从"只删不增"变为双契约——详见主 README 的「混淆修正层」一节。
`rewrite_garbled_tables=True` 开启重度乱码表的视觉重转写层(机械检测整表认废的表,
Qwen-VL 对照截图逐单元格重转写,全量进 report["tableRewrites"])——详见主 README 的
「乱码表重转写层」一节。
`degrade_garbled_tables=True` 开启乱码表降级兜底(纯机械,跑在重转写层之后:仍判废且
有 img_path 的表整项降级为 image,report["tableDegraded"] 计数)——详见主 README 的
「降级兜底」一节。

独立工具函数(都不调 LLM):

```python
mineru_refine.render_markdown(items)    # items → full.md 文本(确定性重渲染)
mineru_refine.detect_suspects(items)    # 仅探测疑点,返回疑点列表
```

## 换模型（自定义 LLM）

默认文本角色是 DeepSeek、视觉角色是 Qwen-VL。两条机制可把它们指向任意其它 LLM(如
MiniMax);完整说明见[主 README](https://github.com/LcpMarvel/mineru-refine#换模型自定义-llm)。

**1. `model_config`——配置驱动、多厂商（推荐）。** 一个含两个独立角色的 dict(`reasoning`
文本、`vision` 视觉),每个是 `{provider?, model, key?, baseUrl?}`(camelCase 键)。省略某
角色则回落 env 默认。

```python
mineru_refine.refine(
    items,
    image_dir="/abs/mineru/out",
    model_config={
        # MiniMax-M3 是 OpenAI 兼容 + 多模态,一个模型同时充当两个角色
        "reasoning": {"provider": "openai", "model": "MiniMax-M3", "key": key, "baseUrl": "https://api.minimaxi.com/v1"},
        "vision":    {"provider": "openai", "model": "MiniMax-M3", "key": key, "baseUrl": "https://api.minimaxi.com/v1"},
    },
)
```

`provider` 接受 `deepseek` / `aliyun`(`qwen`、`dashscope`) / `openai`(`openai-compatible`、
`custom`) / `anthropic`(`claude`) / `gemini`(`google`) / `ollama` / `groq` / `xai`(`grok`);
省略则从模型名推断。

**2. 自定义回调——逃生口（优先级高于 `model_config`）。** 当 `model_config` 表达不了你的
鉴权/代理/模型逻辑时,注入你自己的实现:

```python
def chat(messages, tools):
    # messages: list[dict](OpenAI 风格), tools: list[dict]
    return {
        "message": {"content": "...", "tool_calls": [...]},  # tool_calls 可选
        "finishReason": "stop",
        "usage": {"prompt_tokens": 0, "completion_tokens": 0},
    }

class Vision:
    def judge_split_table(self, img_a: bytes, img_b: bytes):
        return {"verdict": "merge", "reason": "...", "usage": {}}   # verdict: "merge" | "dismiss"
    def transcribe_table(self, img: bytes, cells_render: str):      # 可选;仅 rewrite_garbled_tables 用
        return {"cells": [{"row": 0, "col": 0, "text": "..."}], "usage": {}}

mineru_refine.refine(items, image_dir="/abs/mineru/out", chat=chat, vision=Vision())
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
