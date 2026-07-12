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
  (`report.failOpen === true`), never breaking the upstream pipeline.

This package is a native napi-rs addon of the Rust core implementation (prebuilt, no
local Rust toolchain required); it supports Bun / Node ≥ 18, and its options and return
values are structurally identical to the Python / Rust / HTTP versions.

## Install

```bash
bun add mineru-refine    # or: npm i mineru-refine
```

## Usage

```ts
import { refine, renderMarkdown, detectSuspects } from "mineru-refine";

const { items, report } = await refine(contentList, {
  sha256,                      // optional: source-file SHA256; enables the in-process cache
  maxIterations,               // optional: hard cap on the fix loop; defaults to adaptive with suspect count
  concurrency: 8,              // optional: number of suspects judged in parallel; 1 = strictly serial
  imageDir: "/abs/mineru/out", // optional: MinerU output dir; enables vision judging for split tables
  fixOcrConfusion: false,      // optional: opt-in OCR character-confusion fix layer (CE0→CEO, etc.)
  extraConfusionPairs: [],     // optional: extra allowlist pairs for confusion, e.g. ["0D"]
  rewriteGarbledTables: false, // optional: opt-in vision re-transcription layer for garbled tables (needs imageDir)
  degradeGarbledTables: false, // optional: opt-in fallback that demotes unrecoverable garbled tables to images
  modelConfig,                 // optional: config-driven model swap (see "Swapping models" below)
});

items;    // cleaned content_list (same schema; unknown fields passed through verbatim)
report;   // audit report: iterations / opCounts / dismissed / removedSpans
          //               / violations / tokenUsage / failOpen
          //               (with fixOcrConfusion on, also confusionFixes etc.; see the main README)
```

Every removed span is recorded in `report.removedSpans` (itemId / original text /
reason), auditable line by line. `fixOcrConfusion: true` turns on the confusion-fix
layer (direct replacement, LLM proposal + mechanical gates); once on, the output
contract changes from "remove-only" to a dual contract — see the "Confusion-fix layer"
section of the main README. `rewriteGarbledTables: true` turns on vision
re-transcription for heavily garbled tables (mechanical detection selects tables judged
whole-table junk, Qwen-VL re-transcribes cell by cell against the crop, all recorded in
`report.tableRewrites`) — see the "Garbled-table re-transcription layer" section of the
main README. `degradeGarbledTables: true` turns on the garbled-table fallback (purely
mechanical, runs after the re-transcription layer: tables still judged junk that have an
`img_path` are demoted to `image`, counted in `report.tableDegraded`) — see the
"Fallback demotion" section of the main README.

Standalone helper functions (none call the LLM):

```ts
renderMarkdown(items);   // items → full.md text (deterministic re-render)
detectSuspects(items);   // detect suspects only, returns the suspect list
```

## Swapping models (custom LLMs)

By default the text role is DeepSeek and the vision role is Qwen-VL. Two mechanisms let you
point them at any other LLM (e.g. MiniMax); see the [main README](https://github.com/LcpMarvel/mineru-refine#swapping-models-custom-llms)
for the full explanation.

**1. `modelConfig` — config-driven, multi-vendor (recommended).** An object with two
independent roles (`reasoning` for text, `vision` for vision); each is
`{ provider?, model, key?, baseUrl? }`. Omit a role to fall back to the env default.

```ts
await refine(contentList, {
  imageDir: "/abs/mineru/out",
  modelConfig: {
    // MiniMax-M3 is OpenAI-compatible and multimodal, so one model serves both roles
    reasoning: { provider: "openai", model: "MiniMax-M3", key: process.env.MINIMAX_APIKEY, baseUrl: "https://api.minimaxi.com/v1" },
    vision:    { provider: "openai", model: "MiniMax-M3", key: process.env.MINIMAX_APIKEY, baseUrl: "https://api.minimaxi.com/v1" },
  },
});
```

`provider` accepts `deepseek` / `aliyun` (`qwen`, `dashscope`) / `openai`
(`openai-compatible`, `custom`) / `anthropic` (`claude`) / `gemini` (`google`) / `ollama` /
`groq` / `xai` (`grok`); omit it to infer from the model name.

**2. Custom callbacks — the escape hatch (take priority over `modelConfig`).** The callbacks
are the 4th–6th positional arguments of `refine(items, opts?, onProgress?, chat?, visionJudge?, visionTranscribe?)`:

```ts
const chat = async (messages, tools) =>
  // messages: OpenAI-style array, tools: array
  ({ message: { content: "...", tool_calls: [] }, finishReason: "stop", usage: { prompt_tokens: 0, completion_tokens: 0 } });

const visionJudge = async (imgA: Buffer, imgB: Buffer) =>
  ({ verdict: "merge", reason: "..." });            // verdict: "merge" | "dismiss"

const visionTranscribe = async (img: Buffer, cellsRender: string) =>  // optional; only for rewriteGarbledTables
  ({ cells: [{ row: 0, col: 0, text: "..." }] });

await refine(contentList, { imageDir: "/abs/mineru/out" }, undefined, chat, visionJudge, visionTranscribe);
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
bun install && bun run build   # produces mineru-refine.<platform>.node + index.js / index.d.ts
bun run test
```

Publishing: from the repo root run `just publish-js` (publishes the current-platform
subpackage + the main package; the linux subpackages are published from a linux machine
by running the same command there).

The full design docs for the detector, the fix operation set, and the fidelity gates are
in the [repository README](https://github.com/LcpMarvel/mineru-refine#readme).

## License

MIT
