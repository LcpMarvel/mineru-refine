// CLI transport（SPEC §12 备选）：stdin 收 JSON、stdout 回 JSON，docfuse subprocess 调用。
// stdin 形如 { "items": [...], "sha256"?: "...", "maxIterations"?: n } 或直接是 items 数组。
//
// 跑：  cat content_list.json | bun run src/cli.ts

import { refine } from "./refine.ts";
import type { MineruItem } from "./types.ts";

const raw = await Bun.stdin.text();
if (!raw.trim()) {
  console.error("[mineru-refine] stdin 为空 — 需要 content_list JSON");
  process.exit(2);
}

let parsed: unknown;
try {
  parsed = JSON.parse(raw);
} catch (e) {
  console.error(`[mineru-refine] stdin 不是合法 JSON: ${(e as Error).message}`);
  process.exit(2);
}

const isWrapped = !Array.isArray(parsed) && typeof parsed === "object" && parsed !== null;
const items = (isWrapped ? (parsed as { items: MineruItem[] }).items : parsed) as MineruItem[];
const opts = isWrapped ? (parsed as { sha256?: string; maxIterations?: number }) : {};

const result = await refine(items, { sha256: opts.sha256, maxIterations: opts.maxIterations });
console.log(JSON.stringify(result));
