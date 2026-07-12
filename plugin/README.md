# mineru-refine (Claude Code plugin)

> 🌏 中文文档：[README.zh-CN.md](./README.zh-CN.md)

**PDF/DOC/PPT/image in, clean Markdown out.** One skill handles the whole pipeline:

1. **Parse** — hand the file to the [MinerU](https://mineru.net) official API to get
   `content_list.json` + images;
2. **Refine** — post-process with [`mineru-refine`](https://github.com/LcpMarvel/mineru-refine)
   to fix pseudo-headings, cross-page sentence breaks, cross-page split tables, in-table
   continuation rows, page furniture, residual markup, OCR look-alike characters, garbled
   tables, and more. Throughout, it is **machine-verified fidelity-preserving** (every
   output character comes from the input) and **fail-open** (on error it returns the input
   unchanged rather than breaking things).

The output is a **drop-in replacement directory**: `images/` and `layout.json` mirrored
verbatim, `content_list.json` replaced with the cleaned version, `full.md` re-rendered
deterministically, plus a `refine_report.json` (what was done, what was removed, how many
tokens were spent — auditable line by line).

By default this plugin turns **all three opt-in cleaning layers on** (OCR look-alike fix
/ garbled-table vision re-transcription / garbled-table fallback demotion) for the
cleanest possible output; the trade-off is that the output contract shifts from
"remove-only" to a dual contract (all replacements are fully recorded and programmatically
reversible).

## Install

The marketplace manifest is at the repo root `.claude-plugin/marketplace.json` (the plugin
itself lives in the `plugin/` subdirectory). In Claude Code:

```
/plugin marketplace add LcpMarvel/mineru-refine
/plugin install mineru-refine@mineru-refine
```

`LcpMarvel/mineru-refine` is a GitHub repo shorthand, equivalent to the full URL
`https://github.com/LcpMarvel/mineru-refine`. For local development you can use a local
path instead: `/plugin marketplace add /abs/path/to/mineru-refine` (the repo root, not
`plugin/`). After changing the SKILL/scripts, hot-reload with `/reload-plugins`.

## Usage

Once installed, just tell Claude Code:

> Clean this PDF: /abs/path/to/report.pdf

The skill (`mineru-prime`) walks you through first-time key setup, then runs the full
parse + refine and drops the output into `mineru-refine-out/refined/`, reporting a
before/after suspect comparison and an audit summary.

## Three keys required

On first run the skill asks for and writes them to the working-directory `.env`
(persisted, so you won't re-enter them next time):

| key | Necessity | Where / purpose |
|---|---|---|
| `MINERU_API_TOKEN` | Required for parsing | https://mineru.net — official API parsing quota |
| `DEEPSEEK_APIKEY` | Required for refining | https://platform.deepseek.com — refine text judging; if missing, fail-open |
| `QWEN_APIKEY` | Strongly recommended | DashScope — cross-page split-table vision judging + garbled-table vision re-transcription; if missing, those two are deferred |

> Private deployment: `DEEPSEEK_BASE_URL` / `QWEN_BASE_URL` / the various `*_MODEL` can all
> be overridden in `.env` to point at a self-hosted OpenAI-compatible endpoint. See the
> main mineru-refine repo README for details.

## Dependencies

- **[bun](https://bun.sh)** — the runtime (the skill detects it and guides installation if missing).
- **`unzip`** — to unpack the MinerU output zip (bundled on macOS/Linux).
- `mineru-refine` npm native binding — the skill runs `bun install` automatically on first
  run (includes a prebuilt binary, no Rust toolchain required).

## Structure

```
plugin/
├── .claude-plugin/
│   ├── plugin.json               # plugin manifest
│   └── marketplace.json          # bundled marketplace (points at this directory)
├── bin/                          # auto-added to PATH when enabled; the skill calls these as commands
│   ├── mineru-prime-fetch        #   → scripts/mineru_fetch.ts
│   └── mineru-prime-refine       #   → scripts/refine.ts (auto bun install on first run)
├── skills/mineru-prime/SKILL.md  # the orchestration flow
└── scripts/
    ├── mineru_fetch.ts           # MinerU official-API parsing (single file → output dir)
    ├── refine.ts                 # calls mineru-refine → drop-in replacement dir + report
    └── package.json              # declares the mineru-refine dependency
```
