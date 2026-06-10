// 对真实 MinerU 解析产物（test_data/mineru/<stem>/content_list.json）跑 refine。
// 输出目录是 MinerU 产物目录的【完整替身】（drop-in）：images/、full.md、layout.json 等
// 原样镜像（content_list 里的 img_path 引用才不会断），仅替换 content_list.json 为清洗版，
// 另附 refine_report.json。带 UUID 前缀的原始 content_list 副本不拷贝（避免新旧两份产生混淆）。
//
// 跑：  bun run refine:real             # 全部
//      bun run refine:real <stem>      # 只跑某个文档

import { cp, mkdir, readdir, rm } from "node:fs/promises";
import { join } from "node:path";
import { detect } from "../src/detect.ts";
import { assignIds } from "../src/id.ts";
import { renderMarkdown } from "../src/markdown.ts";
import { imageDirLoader, refine } from "../src/refine.ts";
import type { MineruItem } from "../src/types.ts";

const MINERU_DIR = new URL("../test_data/mineru/", import.meta.url).pathname;
const SOURCE_DIR = new URL("../test_data/source/", import.meta.url).pathname;
const OUT_DIR = new URL("../test_data/refined/", import.meta.url).pathname;

const only = process.argv[2];
const stems = (await readdir(MINERU_DIR).catch(() => [] as string[])).filter((d) => !d.startsWith("."));
if (stems.length === 0) throw new Error("test_data/mineru/ 为空 — 先跑 bun run mineru:fetch");
const targets = only ? stems.filter((s) => s === only) : stems;
if (targets.length === 0) throw new Error(`找不到文档 ${only}，已有: ${stems.join(", ")}`);

async function sha256OfSource(stem: string): Promise<string | undefined> {
  for (const f of await readdir(SOURCE_DIR)) {
    if (f.startsWith(`${stem}.`)) {
      const hasher = new Bun.CryptoHasher("sha256");
      hasher.update(await Bun.file(join(SOURCE_DIR, f)).arrayBuffer());
      return hasher.digest("hex");
    }
  }
  return undefined; // 找不到源文件就不启用缓存
}

function suspectStats(items: MineruItem[]): Record<string, number> {
  const stats: Record<string, number> = {};
  for (const w of detect(assignIds(items).ref)) {
    stats[`${w.kind}${w.hasOp ? "" : "*"}`] = (stats[`${w.kind}${w.hasOp ? "" : "*"}`] ?? 0) + 1;
  }
  return stats;
}

for (const stem of targets) {
  const items = (await Bun.file(join(MINERU_DIR, stem, "content_list.json")).json()) as MineruItem[];
  console.log(`\n════ ${stem} ════  (${items.length} items)`);
  console.log(`输入疑点: ${JSON.stringify(suspectStats(items))}  (* = 仅标记类，无 op)`);

  const t0 = Date.now();
  const r = await refine(items, {
    sha256: await sha256OfSource(stem),
    // 不传则用自适应默认（随疑点数 48~512）；REFINE_MAX_ITERATIONS 仅作显式覆盖
    maxIterations: process.env.REFINE_MAX_ITERATIONS ? Number(process.env.REFINE_MAX_ITERATIONS) : undefined,
    loadImage: imageDirLoader(join(MINERU_DIR, stem)), // split_table 走 Qwen-VL 视觉裁决
  });
  const secs = ((Date.now() - t0) / 1000).toFixed(1);

  console.log(`输出疑点: ${JSON.stringify(suspectStats(r.items))}`);
  console.log(
    `耗时 ${secs}s | items ${items.length}→${r.items.length} | 迭代 ${r.report.iterations} | ` +
      `ops ${JSON.stringify(r.report.opCounts)} | dismissed ${r.report.dismissed} | ` +
      `violations ${r.report.violations} | failOpen ${r.report.failOpen} | ` +
      `tokens p=${r.report.tokenUsage.prompt} c=${r.report.tokenUsage.completion}`,
  );
  for (const s of r.report.removedSpans) {
    console.log(`  删除 [${s.reason}] ${s.itemId}: 「${s.text.slice(0, 60)}」`);
  }

  // 镜像整个 MinerU 产物目录（drop-in 替身），再覆盖 content_list.json 为清洗版
  const src = join(MINERU_DIR, stem);
  const dest = join(OUT_DIR, stem);
  await rm(dest, { recursive: true, force: true });
  await mkdir(dest, { recursive: true });
  await cp(src, dest, {
    recursive: true,
    filter: (p) => !p.endsWith("content_list.json") && !p.endsWith("content_list_v2.json"),
  });
  await Bun.write(join(dest, "content_list.json"), JSON.stringify(r.items, null, 2));
  // full.md 从清洗后 items 确定性重渲染（与清洗版 content_list 保持一致；
  // 注意 MinerU 原版 full.md 本身就与其 content_list 有少量出入，以 content_list 为准）
  await Bun.write(join(dest, "full.md"), renderMarkdown(r.items));
  await Bun.write(join(dest, "refine_report.json"), JSON.stringify(r.report, null, 2));
  const copied = (await readdir(dest)).join(", ");
  console.log(`→ test_data/refined/${stem}/  [${copied}]`);
}
