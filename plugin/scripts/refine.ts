// 对一个 MinerU 产物目录跑 mineru-refine（全链路 plugin 默认三层 opt-in 全开），
// 产出一个 drop-in 替身目录：images/、layout.json 等原样镜像（img_path 不断链），
// content_list.json 替换为清洗版，full.md 从清洗后 items 确定性重渲染，
// 另写 refine_report.json（审计：ops/dismissed/removedSpans/tokens/failOpen）。
//
// 跑：  bun refine.ts <mineru-out-dir> [refined-out-dir]
//   env: DEEPSEEK_APIKEY（必需，缺则 refine 直接 fail-open）
//        QWEN_APIKEY（视觉裁决 / 乱码表重转写需要，缺则该类疑点搁置）
//   Bun 会自动加载 cwd 下的 .env，所以从含 .env 的工作目录运行即可。
//
// 末行输出一行 JSON 摘要，供上层程序化读取。

import { cp, mkdir, readdir, readFile, rm, writeFile } from "node:fs/promises";
import { basename, join, resolve } from "node:path";

// MinerU 批量 API 的 zip 解包出一批带 UUID 前缀的原始件（<uuid>_origin.pdf /
// _model.json / _content_list.json / _content_list_v2.json …），fetch 已归一化出无前缀的
// content_list.json，这些原始件在 drop-in 替身里是冗余。镜像后按此前缀清掉，
// 无前缀的标准产物（content_list.json / full.md / layout.json / images/）一律保留。
const RAW_ARTIFACT = /^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}_/;
import { detectSuspects, refine, renderMarkdown } from "mineru-refine";

const [srcArg, dstArg] = process.argv.slice(2);
if (!srcArg) throw new Error("用法: bun refine.ts <mineru-out-dir> [refined-out-dir]");

const srcDir = resolve(srcArg);
const dstDir = resolve(dstArg ?? `${srcDir}-refined`);

const contentPath = join(srcDir, "content_list.json");
if (!(await Bun.file(contentPath).exists())) {
  throw new Error(`${contentPath} 不存在 — 先用 mineru_fetch.ts 解析出 MinerU 产物`);
}

const items = JSON.parse(await readFile(contentPath, "utf8"));
if (!Array.isArray(items)) throw new Error(`content_list.json 不是数组，schema 异常`);

const suspectsBefore = (detectSuspects(items) as unknown[]).length;

// 三层 opt-in 全开：最大限度清洗。imageDir 指向 MinerU 产物目录，
// 供 split_table 视觉裁决 + 乱码表视觉重转写读取表格裁剪图。
const result = await refine(items, {
  imageDir: srcDir,
  fixOcrConfusion: true,
  rewriteGarbledTables: true,
  degradeGarbledTables: true,
});

const suspectsAfter = (detectSuspects(result.items) as unknown[]).length;

// ── 产出 drop-in 替身目录 ──
// 先整目录镜像（保住 images/、layout.json，img_path 引用不断链），再覆盖三件套。
await rm(dstDir, { recursive: true, force: true });
await cp(srcDir, dstDir, { recursive: true });
await mkdir(dstDir, { recursive: true });
// 剔除 MinerU 原始件（带 UUID 前缀），让 drop-in 替身只剩标准产物。
for (const name of await readdir(dstDir)) {
  if (RAW_ARTIFACT.test(name)) await rm(join(dstDir, name), { recursive: true, force: true });
}
await writeFile(join(dstDir, "content_list.json"), JSON.stringify(result.items, null, 2));
await writeFile(join(dstDir, "full.md"), renderMarkdown(result.items));
await writeFile(join(dstDir, "refine_report.json"), JSON.stringify(result.report, null, 2));

const r = result.report ?? {};
const summary = {
  ok: true,
  stem: basename(srcDir),
  outDir: dstDir,
  suspectsBefore,
  suspectsAfter,
  failOpen: r.failOpen ?? false,
  iterations: r.iterations ?? 0,
  opCounts: r.opCounts ?? {},
  dismissed: r.dismissed ?? 0,
  removedSpans: Array.isArray(r.removedSpans) ? r.removedSpans.length : 0,
  confusionFixes: Array.isArray(r.confusionFixes) ? r.confusionFixes.length : 0,
  tableRewrites: Array.isArray(r.tableRewrites) ? r.tableRewrites.length : 0,
  tableDegraded: r.tableDegraded ?? 0,
  violations: r.violations ?? 0,
  tokenUsage: r.tokenUsage ?? null,
};

console.log(`✅ ${summary.stem} → ${dstDir}/  (疑点 ${suspectsBefore} → ${suspectsAfter}${summary.failOpen ? "，⚠️ failOpen" : ""})`);
console.log(JSON.stringify(summary));
