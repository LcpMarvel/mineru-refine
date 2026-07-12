# mineru-refine

> 🌏 中文文档：[README.zh-CN.md](./README.zh-CN.md)

A post-processor (linter / fixer) for [MinerU](https://github.com/opendatalab/MinerU) parse results.

MinerU parses a PDF into `content_list.json` — an array of item objects, each item a
paragraph of body text, a heading, a table, or an image. The parse quality is good, but
there is a class of high-frequency structural problems:

- **Pseudo-headings** — an ordinary line of body text mislabeled as a heading
- **Missed headings** — same-level numbered siblings are all headings, yet this one is labeled body text
- **Cross-page sentence breaks** — one sentence cut into two items by a page boundary
- **Cross-page split tables** — one table cut into multiple tables across pages
- **In-table continuation rows** — a record's long cell cut into several `<tr>` rows where only one column has text
- **Page furniture** — headers, footers, and page numbers mixed into the body
- **Residual markup** — parse debris like markdown links, LaTeX commands, `\$`/`\*` escapes
- **Giant blocks** — several sections glued into one extra-long item
- **Trailing adhesion** — a cross-page merge sucks a standalone structural block like "[Related documents]" onto the end of the previous paragraph
- **Table noise** — all-empty `<tr>`, OCR spaces inside cells (including URLs broken by spaces), pseudo-LaTeX wrapping (`$\text{...}$` around plain text; known symbol commands are converted to Unicode, while real formulas like `\frac` are left untouched)
- **Minority spellings of look-alike characters** — `SWOT`×24 coexisting with `SW0T`×6 in the same document (0↔O look-alike misread); a document-wide frequency vote rewrites the minority
- **Section headings swallowed into a table caption** — MinerU stuffs "4.6 Application of core organizational performance" into an adjacent table's `table_caption`, which after rendering looks like a missed heading promotion but is really a structural misplacement
- **Header/footer furniture swallowed into a table caption** — MinerU stuffs a running header "Appendix 3: …" or a footer "Prepared by: Zhang Wei" into the `table_caption` of a cross-page table fragment, `mergeTable` faithfully preserves it, and it renders as residue (identified via same-text furniture corroboration and removed)

Of these, unambiguous table noise and the frequency vote are handled directly by the
**mechanical cleaning pass** (deterministic code, self-verifying, no LLM); the rest go to
the LLM to judge.

mineru-refine takes the content_list, fixes these problems, and returns a content_list
with the **same schema**. Downstream still reads "a MinerU result" — plug it into an
existing pipeline as a transparent filter, with zero changes on the consumer side.

Two core promises:

1. **Never adds a single character**: it only reduces and reorganizes (merge, split,
   delete, demote); every content character in the output comes from the input, and each
   step is verified by machine — not by prompting the LLM, but by rolling back
   automatically on any violation. In the default configuration the only character
   replacement is the mechanical cleaning pass's **document-wide frequency vote**
   (`SW0T`→`SWOT`: document-wide majority ≥4 occurrences and ≥3× the minority, differing
   by exactly one look-alike character; the evidence is self-evident across the document,
   deterministic, and each case is recorded in `report.removedSpans` with
   reason=`mech:token_vote→…`).
2. **Never breaks the upstream (fail-open)**: any exception, timeout, or LLM
   unavailability returns the input items unchanged, logs loudly, and marks
   `report.failOpen` as `true`.

The fix decisions are made by the LLM ("should this suspected pseudo-heading be demoted,
or is it a false positive?"), but the LLM only **picks one** of the predefined fix
operations — execution, verification, and termination are all controlled by deterministic
code, and **whether the result passes is decided by machine gates, not by LLM
self-assessment**.

## Install

The core is a single Rust implementation; each language binding imports the same code, with
structurally identical options and return values:

| Language | Install | Form |
|------|------|------|
| **Python** | `pip install mineru-refine` | PyO3 native extension ([docs](bindings/python/)) |
| **JS/TS** | `bun add mineru-refine` / `npm i mineru-refine` | napi-rs native addon, Bun / Node ≥18 ([docs](bindings/js/)) |
| **Rust** | `cargo add mineru-refine` | core crate ([docs](crates/mineru-refine/)) |
| **Any language** | `cargo install mineru-refine --features bin` | HTTP server / CLI (see below) |
| **Claude Code** | `/plugin install mineru-refine` | end-to-end skill: file in, clean markdown out ([docs](plugin/)) |

The Python/JS/Rust/HTTP paths above all take an **already-parsed** `content_list` (produced
by MinerU). If all you have is a raw file (PDF/DOC/PPT/image) and you want clean markdown in
one step, see the [Claude Code plugin](#claude-code-plugin-pdf-in-clean-markdown-out) below —
it chains MinerU official-API parsing with mineru-refine cleaning into a single skill.

## Quick start

You need an LLM API key (see [Environment variables](#llm-integration-and-environment-variables)):
`DEEPSEEK_APIKEY` is required, `QWEN_APIKEY` is needed when table vision judging is enabled.

**Python:**

```python
import json
import mineru_refine

items = json.load(open("content_list.json"))
result = mineru_refine.refine(items, image_dir="/abs/path/to/mineru/output")
result["items"]    # cleaned content_list, same schema as the input
result["report"]   # audit report: what was done, what was removed, how many tokens spent
```

**JS/TS:**

```ts
import { refine } from "mineru-refine";

const { items, report } = await refine(contentList, {
  imageDir: "/abs/path/to/mineru/output",
});
```

**Rust:**

```rust
use mineru_refine::{refine, RefineOptions};

let result = refine(items, RefineOptions {
    image_dir: Some("/abs/path/to/mineru/output".into()),
    ..Default::default()
}).await;
// Never returns Err and never leaks a panic: fail-open is built in — check result.report.fail_open
```

**HTTP (any language):**

```bash
cargo install mineru-refine --features bin
mineru-refine-server   # default port 8771, override with MINERU_REFINE_PORT

curl -X POST localhost:8771/refine \
  -d '{"items":[...], "imageDir":"/abs/path/to/mineru/output"}'
curl localhost:8771/health
```

`imageDir` is the MinerU output directory (the one containing `images/`), and is optional:
providing it enables vision judging for cross-page split tables (using table crops to decide
"is this the same table?"); not providing it skips that whole class of problems and keeps the
tables as-is. In HTTP mode the directory must share a filesystem with the server.

**Cleaning progress (optional):** cleaning runs a loop-until-dry worklist of "items to fix"
with no notion of page numbers, so the progress unit is "suspects processed / iteration
rounds". Each iteration emits one frame
`{ iterations, maxIterations, worklistRemaining, inputSuspects }` (including the start point
iterations=0 and the end point worklistRemaining=0), consumable from every surface:

```bash
# HTTP: SSE stream (per-round event: progress, closing event: result = the non-streaming /refine response)
curl -N -X POST localhost:8771/refine/stream -d '{"items":[...]}'
```

```ts
// JS: onProgress as the third argument
await refine(contentList, {}, (p) => console.log(p.worklistRemaining, "/", p.inputSuspects));
```

```python
# Python: progress= keyword, callback receives a dict
mineru_refine.refine(items, progress=lambda p: print(p["worklistRemaining"], "/", p["inputSuspects"]))
```

```rust
// Rust: RefineOptions.progress = Some(Arc<dyn Fn(Progress)+Send+Sync>)
```

In CLI mode, progress goes to stderr as NDJSON (`[mineru-refine:progress] {…}`), while stdout
stays pure result JSON. When no progress callback is passed there is zero overhead and behavior
is byte-for-byte unchanged.

We recommend the consumer call this once after reading `content_list.json` and before consuming
it, replacing the original array with the returned `items`; add a timeout fallback on the caller
side too, which together with the built-in fail-open forms a double safety net.

## Claude Code plugin (PDF in, clean Markdown out)

The libraries/bindings above all start from a `content_list` — they assume you have already run
MinerU. `plugin/` provides a **Claude Code plugin** that chains the "parse + clean" two steps into
one skill: **raw file (PDF/DOC/PPT/image) in, clean markdown out**, with no integration code to
write yourself.

```
/plugin marketplace add LcpMarvel/mineru-refine
/plugin install mineru-refine@mineru-refine
```

Once installed, just say "Clean this PDF: /abs/path/to/report.pdf", and the skill (`mineru-prime`)
will:

1. **Parse** — hand the file to the [MinerU](https://mineru.net) official API to get
   `content_list.json` + images;
2. **Clean** — call mineru-refine to post-process (all three opt-in cleaning layers **on** by
   default, for the cleanest output);
3. Produce a **drop-in replacement directory** (`images/`/`layout.json` mirrored,
   `content_list.json` replaced with the cleaned version, `full.md` re-rendered, plus a
   `refine_report.json`) placed under `mineru-refine-out/refined/`.

On first run the skill guides you through writing the working-directory `.env` (persisted):
`MINERU_API_TOKEN` (parsing), `DEEPSEEK_APIKEY` (cleaning), `QWEN_APIKEY` (vision judging, strongly
recommended). Depends on [bun](https://bun.sh) + `unzip`; the npm native binding runs `bun install`
automatically on first run (includes a prebuilt binary, no Rust toolchain required).

See [`plugin/README.md`](plugin/) for details.

## Options and return value

Each language differs only in naming style (Python snake_case, JS camelCase); the semantics are
identical:

| Option | Default | Semantics |
|---|---|---|
| `sha256` | none | Source-file SHA256; enables the in-process cache. The cache key also includes the logic version, model, and prompt version — change any of those and stale results are invalidated automatically, never wrongly hit |
| `maxIterations` | adaptive | Hard cap on the fix loop. Defaults to adaptive with suspect count (`clamp(2N+16, 48, 512)`), force-stops at the cap |
| `concurrency` | 8 | Number of suspects judged in parallel; `1` = strictly serial |
| `imageDir` | none | MinerU output directory; providing it enables vision judging for cross-page split tables (vision is the only judging path for that class of problem) |
| `fixOcrConfusion` | `false` | **opt-in** OCR character-confusion fix layer (CE0→CEO, 入=n→λ=n, 竟争→竞争, …), covering body and table cells. Once on, the output contract changes from "remove-only" to a dual contract; see [Confusion-fix layer](#confusion-fix-layer-opt-in) below |
| `extraConfusionPairs` | `[]` | User-supplied extra allowlist pairs for confusion, each exactly 2 distinct characters (e.g. `"0D"` means 0↔D can be swapped directly). An invalid config triggers fail-open immediately, not silently swallowed |
| `rewriteGarbledTables` | `false` | **opt-in** vision re-transcription layer for heavily garbled tables (代格→代码, 数据来酒→数据来源, Midhuel→Michael, …). The mechanical detector selects tables judged whole-table junk, and Qwen-VL re-transcribes cell by cell against the `img_path` crop. Requires `imageDir` (fail-open if missing); see [Garbled-table re-transcription layer](#garbled-table-re-transcription-layer-opt-in) below |
| `degradeGarbledTables` | `false` | **opt-in** garbled-table fallback (purely mechanical, no LLM/VL). Runs after the re-transcription layer: tables still judged junk that have an `img_path` are demoted whole to `image` (`table_body` removed and recorded, counted in `report.tableDegraded`). Both layers on = rescue first, demote what can't be rescued |
| `modelConfig` | none | Config-driven model swap: point the text (`reasoning`) and/or vision (`vision`) roles at any multi-vendor / OpenAI-compatible LLM (DeepSeek, Qwen, OpenAI, Anthropic, Gemini, MiniMax, a self-hosted endpoint, …). Omitted roles fall back to the env default (DeepSeek/Qwen). See [Swapping models](#swapping-models-custom-llms) below |

Return value `{ items, report, provenance }`:

| Field | Meaning |
|---|---|
| `items` | Cleaned content_list; field set/types match MinerU, unknown fields passed through verbatim |
| `report.iterations` | Actual number of fix-loop rounds |
| `report.opCounts` | Execution count for each fix operation |
| `report.dismissed` | Number of suspects ruled false positives (or deferred) |
| `report.dismissedSuspects` | Per-item detail of the `dismissed` count: each entry has `kind` (detector category) / `itemId` / `reason` (defer category: `llm_dismiss` LLM actively judged false positive / `max_rounds_exhausted` rounds exhausted / `vision_unavailable` split_table had no image to judge / `llm_no_tool_call` / `llm_error` call retries exhausted) / `detail` (one-line LLM reason or error message, may be empty) / `evidence` (raw detector evidence). Used for offline review of "why it wasn't fixed" and tuning the detector/prompt accordingly; omitted when there are none |
| `report.removedSpans` | Removal record: for each removed span, itemId / original text / reason, auditable line by line |
| `report.violations` | Number of fidelity-gate rollbacks (fix output violated fidelity and was auto-reverted) |
| `report.tokenUsage` | LLM token consumption |
| `report.failOpen` | Whether fail-open was triggered; when `true`, `items` is the original input |
| `report.confusionFixes` | Each replacement the confusion layer applied (itemId / field / offset / before & after chars / allowlist source / LLM basis). Present only when `fixOcrConfusion` is on and there were replacements |
| `report.confusionRejected` | Number of confusion proposals rejected by the gates (structurally invalid / density over the limit / vetoed by second-pass judging) |
| `report.confusionObservations` | Out-of-table OCR-quality issues the LLM noticed while judging; recorded only, never applied, usable as a downstream quality signal |
| `report.tableRewrites` | Each whole-cell replacement the re-transcription layer applied (itemId / row & col / before / after / character range of the new string). `before` is the undo credential — write it back over the range to reverse programmatically. Present only when `rewriteGarbledTables` is on and there were replacements |
| `report.tableRewriteRejected` | Number of re-transcription proposals rejected by the gates (structurally invalid / row/col does not exist / whole-table coverage regression fails) |
| `report.tableDegraded` | Number of tables the demotion layer demoted to images; each has a record in `removedSpans` (reason=`garbled:degrade_to_image(coverage=…)`). Present only when `degradeGarbledTables` is on and there were demotions |
| `provenance` | Always empty by default (pure reduction adds no characters); when the confusion / re-transcription layers are on, each of their replacements is registered here (origin=`ocr_confusion` / `garbled_table`) |

## Swapping models (custom LLMs)

By default the text role is DeepSeek and the vision role is Qwen-VL. You are not locked
into that — the text (`reasoning`) and vision (`vision`) roles can each be pointed at a
different LLM independently. There are two mechanisms, in priority order.

### 1. `modelConfig` — config-driven, multi-vendor (recommended)

Built on the [genai](https://github.com/jeremychone/rust-genai) crate, which natively
knows DeepSeek, Aliyun (Qwen/DashScope), OpenAI, Anthropic, Gemini, Ollama, Groq, xAI,
plus **any OpenAI-compatible custom endpoint**. Pass a `modelConfig` with two independent
roles; omit a role to fall back to the env default.

Each role is `{ provider?, model, key?, baseUrl? }`:

| Field | Meaning |
|---|---|
| `provider` | Vendor/protocol: `deepseek` / `aliyun` (`qwen`, `dashscope`) / `openai` (also `openai-compatible`, `custom`) / `anthropic` (`claude`) / `gemini` (`google`) / `ollama` / `groq` / `xai` (`grok`). Omit to infer from the model name |
| `model` | Model name (e.g. `deepseek-chat`, `qwen-vl-max`, `gpt-4o`, `MiniMax-M3`) |
| `key` | API key. Omit to fall back to the vendor's default env var |
| `baseUrl` | OpenAI-compatible endpoint (private deployment / custom gateway). Omit to use the vendor default |

**Example — MiniMax-M3** (OpenAI-compatible and natively multimodal, so one model serves
both roles):

```ts
// JS
await refine(contentList, {
  imageDir: "/abs/mineru/out",
  modelConfig: {
    reasoning: { provider: "openai", model: "MiniMax-M3", key: process.env.MINIMAX_APIKEY, baseUrl: "https://api.minimaxi.com/v1" },
    vision:    { provider: "openai", model: "MiniMax-M3", key: process.env.MINIMAX_APIKEY, baseUrl: "https://api.minimaxi.com/v1" },
  },
});
```

```python
# Python
mineru_refine.refine(
    items,
    image_dir="/abs/mineru/out",
    model_config={
        "reasoning": {"provider": "openai", "model": "MiniMax-M3", "key": key, "baseUrl": "https://api.minimaxi.com/v1"},
        "vision":    {"provider": "openai", "model": "MiniMax-M3", "key": key, "baseUrl": "https://api.minimaxi.com/v1"},
    },
)
```

```rust
// Rust
use mineru_refine::{ModelConfig, ProviderConfig};
let minimax = ProviderConfig {
    provider: Some("openai".into()),
    model: "MiniMax-M3".into(),
    key: Some(key.clone()),
    base_url: Some("https://api.minimaxi.com/v1".into()),
};
let opts = RefineOptions {
    image_dir: Some("/abs/mineru/out".into()),
    model_config: Some(ModelConfig {
        reasoning: Some(minimax.clone()),
        vision: Some(minimax),
    }),
    ..Default::default()
};
```

Requirements: the text endpoint must support **tool-call** (`tool_choice: "required"`) and
the vision endpoint must accept image input. Reasoning models that emit `<think>…</think>`
blocks (MiniMax, DeepSeek-R1, QwQ, …) are handled — the reasoning block is stripped
automatically so it never pollutes the judging output. `modelConfig` goes into the cache
key (by model identity), so swapping models never wrongly hits a cache entry from another
model. Judging quality is model-dependent; the fidelity gate and fail-open still backstop
(bad changes are rolled back, worst case returns unchanged), so compare `report` on a few
real documents when trying a new model.

### 2. Custom callbacks — the escape hatch

When you need auth/proxy/model logic that `modelConfig` can't express, inject your own
`chat` / `vision` implementation. These take priority over `modelConfig`, which takes
priority over the env default. See the per-language docs for the exact callback shapes:
[Python](bindings/python/#swapping-models-custom-llms) (an object/callable),
[JS](bindings/js/#swapping-models-custom-llms) (async callbacks),
[Rust](crates/mineru-refine/) (`Arc<dyn ChatClient>` / `Arc<dyn VisionClient>`).

## Hard guarantees

- **Fidelity**: the output content characters (`text` + `list_items` + `table_caption`,
  counting non-whitespace only) are a sub-multiset of the input — written `C_out ⊆ C_in`,
  i.e. containing no character absent from the input. Each fix operation is verified
  immediately after execution and rolled back on violation; the whole document is verified
  once more at the exit, and if it fails, fail-open.
- **Table bytes unchanged**: for tables not processed, `table_body` is byte-for-byte equal
  to the input. Cross-page-merged tables are demoted to **row-level byte fidelity**: each
  `<tr>` row must come byte-for-byte from the input row pool, and the row-external "shell"
  must byte-match some input table shell — apart from "splicing certain input rows verbatim
  into some input table", any byte change is rolled back by the gate.
- **Schema transparency**: the output field set/types match MinerU, unknown fields pass
  through verbatim; the stable IDs used internally are stripped before the exit and never
  enter the output.
- **fail-open**: any exception / timeout / LLM unavailability → return the input unchanged +
  log loudly, never breaking the upstream.
- **Idempotent**: run the cleaning result through again and the output is byte-for-byte
  unchanged (verified to hold on three real documents). A document with no suspects makes
  zero LLM calls; providing `sha256` can hit the cache and skip entirely.
- **Auditable**: every removed span is recorded in `report.removedSpans` (itemId / original
  text / reason).

All of the above hold in the default configuration. With `fixOcrConfusion` explicitly on,
"fidelity" and "table bytes unchanged" become the dual contract described below while the
rest are unchanged; with `rewriteGarbledTables` explicitly on, the individual tables the
mechanical detector judged junk get a separate "whole-cell replacement + full record"
contract, see [Garbled-table re-transcription layer](#garbled-table-re-transcription-layer-opt-in);
with `degradeGarbledTables` explicitly on, garbled tables that can't be rescued are demoted
whole to images (pure reduction + record, see [Fallback demotion](#fallback-demotion-degradegarbledtables)).

## Confusion-fix layer (opt-in)

High-frequency OCR look-alike misreads (`CE0`→`CEO`, `0A系统`→`OA`, `入=n`→`λ=n`,
`竟争`→`竞争`, `B1.36%`→`81.36%`) hurt retrieval and can't be fixed by reduction — this is a
**replacement**. Off by default; it runs only when you explicitly pass `fixOcrConfusion: true`,
**after** the core cleaning and all exit gates, as a separate post-processing layer.

The output contract once on (a dual contract, both machine-verifiable/traceable):

1. **Core layer** as before: remove-only (`C_out ⊆ C_in` holds for the core stage);
2. **Confusion layer**: all changes are sparse one-for-one pinpoint replacements; each either
   belongs to a built-in confusion equivalence class (`0↔O`, `1↔l↔I↔|`, `8↔B`, `入↔人↔λ`,
   `竟↔竞`, etc., extensible via `extraConfusionPairs`), or passed an independent adversarial
   second-pass judging; all recorded in `report.confusionFixes` and `provenance`, auditable
   and programmatically reversible.

Power structure: **the LLM has only proposal power, not write power**. Each proposal passes
three mechanical gates — exactly 1 character, a per-field replacement-density ceiling
(confusion is sparse; over the limit the whole field is rejected), and the allowlist
(in-table direct application / out-of-table second-pass judging). An LLM failure inside the
layer only defers the corresponding batch (miss a fix rather than mis-fix), and a
layer-level exception discards only this layer, returning the core output unchanged.

**Tables**: after lexing `table_body`, only text inside td/th cells can become a candidate —
the HTML tag skeleton (the `1` in `colspan=1`) is by construction never replaceable, and
entities (`&amp;`) are skipped as a black box; i.e. "tag skeleton byte-for-byte unchanged,
cell text only sparse one-for-one replacements within the allowlist". Table candidates are
judged with structured row/column context (table caption / header / containing row), plus an
extra per-table aggregate-density gate: cells each individually compliant but too many
proposals across the whole table = garbled-table signature, reject the whole table — the fate
of a garbled table is whole-table judgment, not character-by-character "repair".

**Document-wide frequency vote**: a candidate character together with its neighbors forms a
high-frequency word that appears consistently across the document (≥5 times) with no in-class
variant spellings → most likely a real term, allowlisted and skipped from judging (suppresses
false positives, saves calls); minority spellings of a Latin token (`OGSTM`×2 vs `OGSMT`×20,
single-character difference or adjacent transposition) generate a pinpoint candidate, and when
the LLM confirms it and it hits the majority spelling it is applied directly without
second-pass judging (`source=frequency_vote` — the difference itself is document-wide evidence).

**observations closed loop**: an "X should be Y" out-of-table observation the LLM reports while
judging is parsed into a single-character replacement, generating a pinpoint candidate for a
second round of judging (the three gates as usual), recovering already-spent tokens. At most
one round of re-feed; the second round's observations are recorded but not re-fed (to prevent
loops); frequency-allowlisted terms ("烟感"×5) are not re-fed.

`fixOcrConfusion` and `extraConfusionPairs` both go into the cache key, so calls with different
switches never pollute each other's cache.

## Garbled-table re-transcription layer (opt-in)

Some tables are **judged whole-table junk** by OCR (one real table had 13+ garbled spots:
代格/目择值/数据来酒/合格军/Midhuel…), which character-by-character confusion fixing can't touch —
yet its `img_path` crop is perfectly clear and readable. The fate of such tables is
**cell-by-cell re-transcription against the image**. Off by default; it runs only when you
explicitly pass `rewriteGarbledTables: true` (requires `imageDir`, fail-open as a config error
if missing), after all exit gates and before the confusion layer.

Power structure: **target selection is 100% by the mechanical detector, the LLM has no
nomination power**. The detector runs forward-maximum matching on the CJK segments of cell text
(with an embedded 60k common-word dictionary) and computes the "fraction of characters covered
by dictionary words" — a garbled word is a non-word combination of common characters
(代格/目择/来酒) and its coverage collapses, while a normal table stays clearly higher even when
full of proper nouns (stock codes / company names). Thresholds are calibrated to real documents:
garbled table 0.46, worst normal table 0.61, taking 0.55 as the junk threshold.

A table judged junk, along with its current cell contents, is sent to Qwen-VL against the crop;
the vision model has only **cell-level proposal power**, and applying it passes three mechanical
gates:

1. **Eligibility**: the original cell must have "garbled and destroyed" evidence — spaces, pure
   numeric cells, short-ID cells (`G1.4`), and cells with normal word coverage may never be
   touched. In practice the vision model row/column-misaligns on a 33-column-wide table,
   bringing another cell's content over under the wrong name (`79.41%`→`84.1%`, a long sentence
   →`Michael`); this gate blocks all such proposals;
2. **Structure**: the row/col number must hit an existing cell, no tags/control characters may be
   introduced, length has an upper bound, the proposal may not be pure numeric, and its length
   magnitude must be comparable to the original cell (all misalignment signatures); for duplicate
   proposals on the same cell only the first is honored;
3. **Whole-table regression**: the dictionary coverage after re-transcription must be **strictly
   higher** than before — anything the vision model does beyond "repair" is held down by this
   gate and the whole table is reverted.

The HTML tag skeleton is by construction untouchable (replacement happens only in the cell-inner
range); each replacement enters `report.tableRewrites` (the `before` field is the undo
credential) and `provenance` (origin=`garbled_table`), auditable and programmatically reversible.
No image / vision failure / oversized table just defers the corresponding table (miss rather than
mis-fix), and a layer-level exception discards only this layer. Incidentally, whole-cell
re-transcription naturally covers **word-level** errors like `Midhuel→Michael` — they exceed the
confusion layer's single-character contract but are a natural product in the vision-review context.

`rewriteGarbledTables` goes into the cache key (including the re-transcription prompt version), so
calls with different switches never pollute each other's cache.

### Fallback demotion (degradeGarbledTables)

Re-transcription is best-effort: a vision failure defers, all gates reject, or coverage
regression fails — in all these cases the garbled table stays in the output as-is. A fake table
full of "目择值/数据来酒" is **actively misleading** to downstream retrieval/RAG, while its
`img_path` crop is perfectly clear. Explicitly passing `degradeGarbledTables: true` enables a
purely mechanical fallback (no LLM/VL, can be turned on independently of the re-transcription
layer): running after the re-transcription layer, tables **still judged junk** (dictionary
coverage collapse) that have an `img_path` are demoted whole to `image` — caption/footnote move
to `image_caption`/`image_footnote`, `table_body` is removed and recorded (`removedSpans`, reason
including coverage), and it renders as an image reference in full.md. Both layers on = rescue
first, demote what can't be rescued; tables that are rescued (coverage passes the threshold) are
naturally skipped. It also goes into the cache key.

## How it works

```
  in items
     │
     ▼
  ① Anomaly detector (deterministic heuristics)  →  suspect queue
     │
     ▼
  ② tool-use loop (DeepSeek):
       preload context → LLM picks a fix op / rules false positive
       → execute + fidelity gate (rollback on violation) → re-detect
       (ends only when the queue empties + multiple guards)
     │
     ▼
  ③ Exit gate: fidelity ∧ suspect count non-increasing ∧ geometrically locatable
       pass → continue   ·   fail → fail-open (return the original input)
     │
     ▼
  { items (same schema), report }
```

Control flow is driven by a **deterministic outer loop**: pop a suspect from the queue → hand it
plus context to the LLM → the LLM returns a fix operation or a "false positive" ruling → execute →
re-detect. The LLM doesn't drive the flow freely — this keeps it controllable, cheap, and
unit-testable.

Each item carries an **internal stable ID** (like `it_0001`) throughout the flow; all operation
parameters, the queue, and LLM references use the ID rather than an array index — one merge/split
would shift every index. The ID is an internal field, stripped before the exit.

### Detector: what it can find

**Actionable (has a corresponding fix operation):**

| Suspect type | Heuristic | Fix |
|---|---|---|
| `pseudo_heading` | Has `text_level` but contains a comma / sentence-ending punctuation / body too long | `demote` / `merge` |
| `cross_page_break` | Adjacent blocks across pages, the former not ending in sentence-ending punctuation | `merge` |
| `giant_block` | A single text over the threshold containing multiple suspected section numbers | `split` |
| `page_artifact` | High-frequency repeated short text, or same text as an identified header/footer (≥2 corroborations) | `drop` |
| `residual_markup` | LaTeX debris like markdown links, `$...$`, `\frac` | `strip` |
| `empty_table` | Zero-content table (no rows/caption/image) — a placeholder left after MinerU's cross-page merge | `drop` |
| `split_table` | Two substantive tables across pages with only page furniture in between. Supports chained splits across three+ pages (merge one pair per round, biting together segment by segment) | `mergeTable` (**vision-only judging**, see below) |
| `split_list` | Two adjacent lists across pages | `mergeList` |
| `missed_heading` | Same-level numbered siblings are headings but this block is body text, and the numbers are adjacent | `promote` |
| `trailing_marker` | A section marker stuck at a paragraph's end (a standalone structural block like "[Related documents]", sucked in by a cross-page merge) | `split` |
| `separated_caption` | A caption-like short text separated from its table by a heading block | `reorder` |
| `caption_heading` | A `table_caption` entry is a numbered heading-like short text, and there exists an adjacent same-level numbered heading sibling (MinerU stuffed a section heading into the table caption) | `extractCaption` |
| `caption_artifact` | A `table_caption` entry is same text as a classified `header`/`footer` (≥2 corroborations) or is document-wide high-frequency repeated — MinerU stuffed a running header/footer into the table caption, `mergeTable` faithfully preserved it, and it renders as residue | `dropCaption` |
| `extra_char` | Function-word doubling (的的/地地/是是/了了, legitimate reduplication excepted), isolated radicals ("3)亻") | `deleteChar` |

**Marked only, no fix operation** (the LLM can only rule false positive, counted in report for
observation): orphan/empty caption (`caption_issue`).

### Fix operation set (12 reduction/reorganization + dismiss)

All are pure functions `(items, args) -> items` with built-in fidelity checks; a violation is rolled
back and counted in `report.violations`.

| Operation | Semantics | bbox / page_idx derivation |
|---|---|---|
| `merge(idA, idB)` | Join two adjacent blocks, dropping the separator MinerU inserted | bbox union; page_idx from the first block |
| `split(id, offset)` | Cut into two blocks at offset | Both children inherit the parent |
| `demote(id)` | Demote a pseudo-heading to body text (clear `text_level`) | unchanged |
| `promote(id, level)` | Promote body text to a heading | unchanged |
| `reorder(idsInOrder)` | Fix cross-page misordering (permutations within a contiguous range only) | Each block unchanged |
| `drop(id)` | Delete page number/header/footer/watermark/empty-shell table (must hit the allowlisted type) | — (deletion) |
| `strip(id, pattern)` | Remove residual markup. Pattern allowlist: `md_link` / `latex_dollar` / `latex_block` / `latex_command` / `escaped_dollar` / `html_tag` | unchanged |
| `deleteChar(id, offset)` | Delete a single OCR spurious character. Strict allowlist: function-word doubling adjacent to an identical character (的/地/是/了) or an isolated radical; 的的确确/地地道道/是是非非 are constructively protected | unchanged |
| `mergeTable(idA, idB)` | Cross-page split-table merge: B's `<tr>` rows are appended **byte-verbatim** after A's last row, caption/footnote concatenated; when B's first row is byte-for-byte identical to A's header (header reprinted per page), it is de-duplicated and recorded | bbox union; page_idx from the first block |
| `mergeList(idA, idB, joinSeam?)` | Cross-page split-list merge: `list_items` concatenated; `joinSeam` seams A's last item and B's first item into one (sentence break across pages) | bbox union; page_idx from the first block |
| `extractCaption(id, captionIndex, position, level?)` | Extract a section heading swallowed into `table_caption` as a standalone text block (pure character move), inserted before/after the table; if `level` is given, set `text_level` directly | The new block inherits the table's bbox/page_idx |
| `dropCaption(id, captionIndex)` | Delete a header/footer furniture entry swallowed into `table_caption` (pure reduction; the table body and remaining captions untouched, recorded in `removedSpans`); must hit the `caption_artifact` allowlist | unchanged |
| `dismiss(id, reason)` | Rule a false positive, don't change text; don't mark it again on re-detection | — |

`mergeTable` does **no column-alignment judging and no column-alignment fixing**: "is it the same
table" is judged by the model from content (deliberately not making "equal column count" a gate —
rowspan carried across a page, or an empty column omitted by MinerU on one page, both make column
counts legitimately unequal); rows with ragged columns are kept as-is, never inventing empty cells
to "pad" — which column to pad is a semantic guess, guessing wrong is tampering, and the row-level
fidelity gate keeps exactly this kind of "fix" out. Any misalignment that exists was already in the
MinerU input; merging introduces no new damage.

The geometry fields (`bbox` / `page_idx`) derivation rules guarantee **every output item can still
point back to at least one source item** — downstream highlight positioning depends on them.

### Vision judging for cross-page split tables (Qwen-VL, the only path)

"Are these two tables the same table" is a fact plainly visible in the image but only guessable in
text, so the `split_table` suspect **goes to vision judging only**: send the two tables' MinerU crops
(the content_list `img_path` already points at them) to `qwen-vl-max` with a narrow question, and map
the structured answer to `mergeTable` or `dismiss`. No text fallback path is provided — a first/last
row summary is not enough to verify the true membership of table rows, and mis-merging is worse than
missing a merge. Key points:

- The vision model **only outputs a decision, produces no content characters** — the merge still goes
  through the row-level fidelity gate, never crossing the pure-reduction red line.
- No `imageDir` provided → `split_table` is skipped wholesale, tables kept as-is.
- No image / no key / vision model unavailable / verdict rejected by the gate → defer that suspect
  (without blocking the other fixes).
- Verified 7-for-7 on two real documents (5 real continuation-table merges + 2 fake continuation
  tables ruled false positive, including hard forms like unequal column counts from rowspan and
  same-position different tables on a doc-control page), ~2k tokens per call.

### Guards and termination

- **Ends only when the queue empties**: the loop reaches the end only when all suspects that have a
  fix operation and weren't ruled false positive are processed.
- **False-positive ruling set**: suspects already ruled false positive are excluded on re-detection,
  preventing the same false positive from re-entering the queue repeatedly and the loop from failing
  to converge.
- **Hard cap**: `maxIterations` force-stops at the cap; a single suspect exhausting its rounds → forced
  defer (counted in dismissed). The default cap is adaptive with the initial suspect count — fixes
  unlock new suspects (total workload measured at ~1.6× the initial count), and a fixed constant would
  necessarily truncate large documents.
- **Anti-oscillation**: a merge output may not be split immediately, and a split output pair may not be
  merged back immediately.
- **Contradiction guard**: a single reply that calls both dismiss and a change operation → rejected
  wholesale, feeding the contradiction back to the LLM for a forced re-judgment (in practice the LLM
  writes the "should drop" analysis into the dismiss reason while calling drop in parallel). Every
  change operation carries a one-line basis into the audit log.
- **Joint judging**: strongly correlated suspects are merged into one judgment — the `missed_heading`
  of a same-level numbered sibling group is judged together (preventing per-item judging from
  promoting some and not others), and same-text `page_artifact` is judged together (a given header
  text should be all-deleted or all-kept).
- The exit pass/fail decision is all machine checks: queue empty ∧ fidelity ∧ suspect count ≤ input ∧
  geometrically locatable; any one unmet → fail-open.

## LLM integration and environment variables

All LLM calls go over bare HTTP (`reqwest`), with zero SDK dependency.

| Variable | Required | Purpose |
|---|---|---|
| `DEEPSEEK_APIKEY` | Yes | The main text-judging engine. Also accepts `RAGENT_DEEPSEEK_APIKEY`. If missing, refine goes straight to fail-open |
| `DEEPSEEK_BASE_URL` | No | Defaults to `https://api.deepseek.com`; can point at a private OpenAI-compatible endpoint |
| `DEEPSEEK_MODEL` | No | Defaults to `deepseek-v4-pro`; the in-process cache is isolated by model name automatically when you switch |
| `QWEN_APIKEY` | For vision judging | Qwen-VL judging of cross-page split tables; if missing, those suspects are deferred |
| `QWEN_BASE_URL` | No | Defaults to the DashScope OpenAI-compatible endpoint |
| `QWEN_VISION_MODEL` | No | Defaults to `qwen-vl-max` |
| `MINERU_REFINE_PORT` | No | HTTP server port, defaults to 8771 |

**Fully private deployment**: the endpoints and model names of both the text and vision chains can be
overridden, so documents never leave the intranet. Stand up a service with an OpenAI-compatible
framework like vLLM / SGLang and point `DEEPSEEK_BASE_URL` / `QWEN_BASE_URL` at it. Requirements: the
text endpoint must support **tool-call** (`tool_choice: "required"`), and the vision endpoint must
support multi-image input. Judging quality has not been benchmarked on private models — the fidelity
gate and fail-open still backstop (bad changes are rolled back, worst case returns unchanged), but the
false-positive/fix rates may differ from the default model, so compare `report` on a few real documents
first.

The CLI and HTTP server auto-load `.env` from the current directory at startup; when calling as a
library, set the environment variables yourself (or load `.env` in the host program).

Implementation notes (affecting cost and reproducibility):

- **DeepSeek text judging**: `temperature: 0` + thinking disabled (reproducible, saves reasoning
  tokens); `tool_choice: "required"` forces one operation per round, naturally forbidding body output;
  tool-call arguments go through JSON repair before parsing, backstopping the occasional bad JSON. The
  system prompt and document outline are placed as a message prefix and unchanged per round, hitting
  the DeepSeek input cache (hit price ~1/120 of miss).
- **Qwen-VL vision judging**: images go as base64 data URLs, `temperature: 0`, and the reply is parsed
  as structured JSON.
- **Fault tolerance**: network errors / 429 / 5xx auto-retry; a single suspect's failure only defers
  itself without destroying the whole (fail-open only when a whole round has zero successes).
- **Performance**: suspects are judged 8-way parallel by default; the context of common suspects
  (±2 neighbors, the full cross-page pages) is preloaded into the first message, saving extra
  observation rounds.

### Cost reference

Measured consumption on three real documents (`report.tokenUsage`), estimated at DeepSeek-V4-Pro's
current pricing (from 2026-06: input cache hit ¥0.025 / miss ¥3, output ¥6, all per million tokens):

| Document | Judging rounds | prompt | completion | Estimated cost |
|---|---|---|---|---|
| Strategy management spec (large, content_list 334 KB) | 66 | 1.95M | 27k | ¥0.5 ~ 1.2 (capped at ¥6 if all cache misses) |
| Organizational performance management spec | 8 | 77k | 1k | < ¥0.25 |
| Management review procedure | 7 | 67k | 1k | < ¥0.25 |

The loop is designed to hit the input cache (the system prompt and outline are a stable prefix), so
across many iterations the vast majority of prompt tokens run at the hit price, and actual cost is
usually far below the all-miss ceiling. A Qwen-VL table judgment is ~2k tokens per call, negligible. A
document with no suspects makes zero LLM calls, at zero cost.

## CLI

```bash
cargo install mineru-refine --features bin
cat content_list.json | mineru-refine > refined.json
# stdin can also be a wrapper object: { "items": [...], "sha256"?, "maxIterations"?, "imageDir"? }
```

## Development

```bash
just test         # cargo test: LLM fully mocked, no network (160+ tests)
just check        # clippy -D warnings + fmt --check
just smoke-vl     # smoke: real Qwen-VL table judging (three pairs of real table crops, needs a key)
just js-build     # JS binding local build (napi)
just py-dev       # Python binding build and install into .venv
```

The tests cover six kinds of properties: ① golden fixtures ② fidelity (`C_out ⊆ C_in`) ③ table_body
bytes unchanged ④ monotonic decrease in suspect count ⑤ geometric locatability ⑥ idempotence. Without
a "clean original" as ground truth, "fidelity + suspect decrease + idempotence" is the strongest proxy
metric available.

### Real-data workflow

```bash
# .env needs MINERU_API_TOKEN
just mineru-fetch               # hand the PDFs/DOCs under test_data/source/ to the MinerU official API,
                                # landing output in test_data/mineru/<stem>/
                                # (--force re-runs; --batch <id> reuses a completed batch)
just refine-real                # run refine on all real content_lists (real LLM),
                                # output to test_data/refined/<stem>/, print before/after suspect comparison
just refine-real <stem>         # run just one document; REFINE_MAX_ITERATIONS tunes the cap
```

`test_data/refined/<stem>/` is a **drop-in replacement** for the corresponding MinerU output directory:
images/, layout.json, etc. mirrored verbatim (`img_path` references stay unbroken), `content_list.json`
replaced with the cleaned version, `full.md` deterministically re-rendered from the cleaned items, plus
a `refine_report.json` (audit: ops/dismissed/removedSpans/tokens).

## Directory structure

```
crates/mineru-refine/            # Rust core
  src/types.rs                   #   MineruItem (order-preserving JSON object) / WorkItem / OpCall / RefineReport
  src/id.rs                      #   internal stable ID (stripped at exit, never enters the output schema)
  src/detect.rs                  #   deterministic anomaly detector → suspect queue
  src/mechanical.rs              #   mechanical cleaning pass (table noise + frequency vote, deterministic, no LLM)
  src/ops.rs                     #   12 reduction/reorganization operations + fidelity gate + rollback
  src/extrachar.rs               #   spurious-character allowlist (the gate for deleteChar)
  src/invariant.rs               #   fidelity / table_body / geometry checks
  src/confusion.rs               #   confusion-fix layer (opt-in, fixOcrConfusion)
  src/garbled.rs                 #   garbled-table re-transcription layer + fallback demotion (opt-in)
  src/agent_loop.rs              #   deterministic outer loop + LLM tool-use + guards
  src/llm.rs                     #   bare reqwest: DeepSeek + Qwen-VL (trait injection, test mocks)
  src/markdown.rs                #   cleaned items → full.md deterministic re-render
  src/refine.rs                  #   entry: fail-open + cache + exit gate
  src/bin/{cli,server}.rs        #   stdin/stdout and HTTP transport
  examples/{qwen_smoke,refine_real}.rs   # real-data workflow
  tests/                         #   six-property tests + guard/binding regressions (mock LLM)
bindings/python/                 # PyO3 → pip install mineru-refine
bindings/js/                     # napi-rs → bun add mineru-refine
scripts/mineru_fetch.ts          # MinerU official-API fetch of test output (Bun)
plugin/                          # Claude Code plugin (end-to-end skill: file in, clean markdown out)
  skills/mineru-prime/SKILL.md   #   orchestration: MinerU parse → mineru-refine clean → drop-in output
  scripts/{mineru_fetch,refine}.ts  # parse and clean scripts (Bun)
```

## Scope (deliberately out of scope)

- **No character addition**: content generation like OCR correction or caption completion is never
  done — pure reduction makes fidelity fully provable. The return value reserves a `provenance` channel
  (registering AI-added characters per character) for future extensions.
- **No table column-alignment fixing**: when merging tables, no empty cells are padded and no cells are
  rearranged (see the fix operation set notes).
- Aware of no downstream business model; does not replace MinerU's parsing, only post-processes its output.

## License

MIT
