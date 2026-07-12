# mineru-refine

> 🌏 中文文档：[README.zh-CN.md](./README.zh-CN.md)

A post-processor (linter / fixer) for [MinerU](https://github.com/opendatalab/MinerU) parse results.

It takes MinerU's `content_list` (an array of item objects), fixes the high-frequency
structural problems that parsing introduces — pseudo-headings, cross-page sentence
breaks, cross-page split tables, headers/footers mixed into the body, LaTeX / link
residue — and returns a content_list with the **same schema**, so downstream consumers
need zero changes.

Two core promises:

- **Never adds a single character**: it only removes and reorganizes; every content
  character in the output comes from the input, verified step by step by machine, and
  any violation is rolled back automatically (not enforced by prompting the LLM).
- **fail-open**: any exception / LLM unavailability → the input is returned unchanged
  (`report["failOpen"] == True`), never breaking the upstream pipeline.

This package is a native PyO3 binding of the Rust core implementation; its options and
return values are structurally identical to the JS / Rust / HTTP versions.

## Install

```bash
pip install mineru-refine
```

Requires Python ≥ 3.9.

## Usage

```python
import json
import mineru_refine

items = json.load(open("content_list.json"))

result = mineru_refine.refine(
    items,                              # content_list (list[dict])
    sha256="...",                       # optional: source-file SHA256; enables the in-process cache
    max_iterations=None,                # optional: hard cap on the fix loop; defaults to adaptive with suspect count
    concurrency=8,                      # optional: number of suspects judged in parallel; 1 = strictly serial
    image_dir="/abs/mineru/out",        # optional: MinerU output dir; enables vision judging for split tables
    fix_ocr_confusion=False,            # optional: opt-in OCR character-confusion fix layer (CE0→CEO, etc.)
    extra_confusion_pairs=None,         # optional: extra allowlist pairs for confusion, e.g. ["0D"]
    rewrite_garbled_tables=False,       # optional: opt-in vision re-transcription layer for garbled tables (needs image_dir)
    degrade_garbled_tables=False,       # optional: opt-in fallback that demotes unrecoverable garbled tables to images
    model_config=None,                  # optional: config-driven model swap (see "Swapping models" below)
    chat=None,                          # optional: custom text-LLM callback (escape hatch; takes priority over model_config)
    vision=None,                        # optional: custom vision-LLM object (escape hatch)
)

result["items"]    # cleaned content_list (same schema; unknown fields passed through verbatim)
result["report"]   # audit report: iterations / opCounts / dismissed / removedSpans
                   #               / violations / tokenUsage / failOpen
                   #               (with fix_ocr_confusion on, also confusionFixes etc.; see the main README)
```

Every removed span is recorded in `report["removedSpans"]` (itemId / original text /
reason), auditable line by line. `fix_ocr_confusion=True` turns on the confusion-fix
layer (direct replacement, LLM proposal + mechanical gates); once on, the output
contract changes from "remove-only" to a dual contract — see the "Confusion-fix layer"
section of the main README. `rewrite_garbled_tables=True` turns on vision
re-transcription for heavily garbled tables (mechanical detection selects tables judged
whole-table junk, Qwen-VL re-transcribes cell by cell against the crop, all recorded in
`report["tableRewrites"]`) — see the "Garbled-table re-transcription layer" section of
the main README. `degrade_garbled_tables=True` turns on the garbled-table fallback
(purely mechanical, runs after the re-transcription layer: tables still judged junk that
have an `img_path` are demoted to `image`, counted in `report["tableDegraded"]`) — see
the "Fallback demotion" section of the main README.

Standalone helper functions (none call the LLM):

```python
mineru_refine.render_markdown(items)    # items → full.md text (deterministic re-render)
mineru_refine.detect_suspects(items)    # detect suspects only, returns the suspect list
```

## Swapping models (custom LLMs)

By default the text role is DeepSeek and the vision role is Qwen-VL. Two mechanisms let you
point them at any other LLM (e.g. MiniMax); see the [main README](https://github.com/LcpMarvel/mineru-refine#swapping-models-custom-llms)
for the full explanation.

**1. `model_config` — config-driven, multi-vendor (recommended).** A dict with two
independent roles (`reasoning` for text, `vision` for vision); each is
`{provider?, model, key?, baseUrl?}` (camelCase keys). Omit a role to fall back to the env
default.

```python
mineru_refine.refine(
    items,
    image_dir="/abs/mineru/out",
    model_config={
        # MiniMax-M3 is OpenAI-compatible and multimodal, so one model serves both roles
        "reasoning": {"provider": "openai", "model": "MiniMax-M3", "key": key, "baseUrl": "https://api.minimaxi.com/v1"},
        "vision":    {"provider": "openai", "model": "MiniMax-M3", "key": key, "baseUrl": "https://api.minimaxi.com/v1"},
    },
)
```

`provider` accepts `deepseek` / `aliyun` (`qwen`, `dashscope`) / `openai`
(`openai-compatible`, `custom`) / `anthropic` (`claude`) / `gemini` (`google`) / `ollama` /
`groq` / `xai` (`grok`); omit it to infer from the model name.

**2. Custom callbacks — the escape hatch (take priority over `model_config`).** Pass your
own implementations when you need auth/proxy/model logic `model_config` can't express:

```python
def chat(messages, tools):
    # messages: list[dict] (OpenAI-style), tools: list[dict]
    return {
        "message": {"content": "...", "tool_calls": [...]},  # tool_calls optional
        "finishReason": "stop",
        "usage": {"prompt_tokens": 0, "completion_tokens": 0},
    }

class Vision:
    def judge_split_table(self, img_a: bytes, img_b: bytes):
        return {"verdict": "merge", "reason": "...", "usage": {}}   # verdict: "merge" | "dismiss"
    def transcribe_table(self, img: bytes, cells_render: str):      # optional; only for rewrite_garbled_tables
        return {"cells": [{"row": 0, "col": 0, "text": "..."}], "usage": {}}

mineru_refine.refine(items, image_dir="/abs/mineru/out", chat=chat, vision=Vision())
```

## Environment variables

| Variable | Required | Purpose |
|---|---|---|
| `DEEPSEEK_APIKEY` | Yes | Text judging (DeepSeek). If missing, refine goes straight to fail-open |
| `QWEN_APIKEY` | For vision judging | Qwen-VL judging of cross-page split tables; if missing, those suspects are skipped and the tables kept as-is |

The library itself does not read `.env`; set the environment variables in the host
program (or load `.env` yourself).

## Building locally

```bash
just py-dev        # repo root: build the wheel and install it into bindings/python/.venv
just publish-py    # publish to PyPI: current-platform wheel + sdist (needs MATURIN_PYPI_TOKEN)
```

The full design docs for the detector, the fix operation set, and the fidelity gates are
in the [repository README](https://github.com/LcpMarvel/mineru-refine#readme).

## License

MIT
