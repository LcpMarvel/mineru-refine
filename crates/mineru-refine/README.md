# mineru-refine

> 🌏 中文文档：[README.zh-CN.md](./README.zh-CN.md)

A post-processor (linter / fixer) for [MinerU](https://github.com/opendatalab/MinerU)
parse results — this crate is the core implementation; the Python (PyO3) and JS
(napi-rs) bindings are thin wrappers around it.

It takes MinerU's `content_list` (an array of item objects), fixes the high-frequency
structural problems that parsing introduces — pseudo-headings, cross-page sentence
breaks, cross-page split tables, headers/footers mixed into the body, LaTeX / link
residue — and returns a content_list with the **same schema**. It **only removes and
reorganizes, never adds a single character**: every content character in the output
comes from the input, verified step by step by machine, and any violation is rolled back
automatically; any exception / LLM unavailability returns the input unchanged
(fail-open), and no panic ever leaks out. The full design docs for the detector, the fix
operation set, and the gates are in the
[repository README](https://github.com/LcpMarvel/mineru-refine#readme).

```bash
cargo add mineru-refine
```

## Usage

```rust
use mineru_refine::{refine, RefineOptions};

let result = refine(items, RefineOptions {
    sha256: Some(sha),                        // optional: source-file SHA256; enables the in-process cache
    max_iterations: None,                     // optional: hard cap on the fix loop; defaults to adaptive
    concurrency: Some(8),                     // optional: number of suspects judged in parallel; 1 = strictly serial
    image_dir: Some("/abs/mineru/out".into()),// optional: MinerU output dir → enables vision judging for split tables
    fix_ocr_confusion: false,                 // optional: opt-in OCR character-confusion fix layer
    extra_confusion_pairs: vec![],            // optional: extra allowlist pairs for confusion, e.g. ["0D"]
    rewrite_garbled_tables: false,            // optional: opt-in vision re-transcription for garbled tables (needs image_dir)
    // model_config: config-driven model swap — see "Swapping models" below
    ..Default::default()
}).await;
// Never returns Err and never leaks a panic: fail-open is built in — check result.report.fail_open
result.items;    // cleaned content_list (same schema)
result.report;   // audit: iterations / op_counts / dismissed / removed_spans
                 //        / violations / token_usage / fail_open
```

`items` is a `Vec<MineruItem>` — `MineruItem` is an order-preserving JSON object
(`serde_json::Map`) underneath: unknown fields pass through verbatim, key order is
preserved, and serde converts directly to/from `content_list.json`.

Environment variables: `DEEPSEEK_APIKEY` (text judging, required; if missing, refine goes
straight to fail-open) and `QWEN_APIKEY` (needed for vision judging; if missing,
cross-page split-table suspects are skipped). The library itself does not read `.env`;
set it in the host program (the CLI / server binaries auto-load `.env` from the current
directory).

Standalone helper functions (none call the LLM):

```rust
mineru_refine::detect_items(&items);     // detector: returns the suspect list
mineru_refine::render_markdown(&items);  // items → full.md deterministic re-render
```

For tests / embedding you can inject a fake LLM: `RefineOptions`'s `chat` / `vision` /
`load_image` / `log` accept `Arc<dyn ChatClient>` / `Arc<dyn VisionClient>` /
`Arc<dyn LoadImage>` / `Logger` respectively; the default implementations are bare
reqwest clients for DeepSeek / Qwen-VL.

## Swapping models (custom LLMs)

By default the text role is DeepSeek and the vision role is Qwen-VL. Two mechanisms let you
point them at any other LLM; see the
[main README](https://github.com/LcpMarvel/mineru-refine#swapping-models-custom-llms) for the
full explanation.

**1. `model_config` — config-driven, multi-vendor (recommended).** Built on the
[genai](https://github.com/jeremychone/rust-genai) crate; sets the text (`reasoning`) and/or
vision (`vision`) roles to any of DeepSeek / Aliyun (Qwen) / OpenAI / Anthropic / Gemini /
Ollama / Groq / xAI / any OpenAI-compatible endpoint. Omit a role to fall back to the env
default.

```rust
use mineru_refine::{refine, ModelConfig, ProviderConfig, RefineOptions};

// MiniMax-M3 is OpenAI-compatible and multimodal, so one model serves both roles
let minimax = ProviderConfig {
    provider: Some("openai".into()),
    model: "MiniMax-M3".into(),
    key: Some(key.clone()),
    base_url: Some("https://api.minimaxi.com/v1".into()),
};
let result = refine(items, RefineOptions {
    image_dir: Some("/abs/mineru/out".into()),
    model_config: Some(ModelConfig {
        reasoning: Some(minimax.clone()),
        vision: Some(minimax),
    }),
    ..Default::default()
}).await;
```

`provider` accepts `deepseek` / `aliyun` (`qwen`, `dashscope`) / `openai`
(`openai-compatible`, `custom`) / `anthropic` (`claude`) / `gemini` (`google`) / `ollama` /
`groq` / `xai` (`grok`); omit it to infer from the model name. Reasoning models that emit
`<think>…</think>` blocks are handled — the block is stripped automatically.

**2. Custom callbacks — the escape hatch (take priority over `model_config`).** Inject your
own `chat: Arc<dyn ChatClient>` / `vision: Arc<dyn VisionClient>` (the same injection points
used for test mocks) when you need auth/proxy/model logic `model_config` can't express.

## CLI / HTTP server (`--features bin`)

```bash
cargo install mineru-refine --features bin

cat content_list.json | mineru-refine                  # stdin JSON → stdout JSON
mineru-refine-server                                    # POST /refine + GET /health, port 8771
```

## Development

```bash
cargo test -p mineru-refine     # LLM fully mocked, no network
just --list                     # repo root: real-data workflow / smoke / publish
```

## License

MIT
