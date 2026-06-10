// 用 MinerU 官方 API（https://mineru.net/apiManage/docs）解析 test_data/source/ 下的文件，
// 把每个文件的解析产物（content_list.json / full.md / layout.json …）落盘到 test_data/mineru/<stem>/。
//
// 流程：POST /api/v4/file-urls/batch 拿上传 URL → PUT 文件 → 轮询 /api/v4/extract-results/batch/{id}
//      → state=done 后下载 full_zip_url → unzip。
//
// 跑：  bun run mineru:fetch                 # .env 里需有 MINERU_API_TOKEN
//      bun run mineru:fetch --force         # 忽略已有产物重新解析
//      bun run mineru:fetch --batch <id>    # 复用已有 batch（跳过上传，直接轮询+下载）

import { mkdir, readdir, rm } from "node:fs/promises";
import { join, parse } from "node:path";

const API = "https://mineru.net/api/v4";
const SOURCE_DIR = new URL("../test_data/source/", import.meta.url).pathname;
const OUT_DIR = new URL("../test_data/mineru/", import.meta.url).pathname;
const SUPPORTED = new Set(["pdf", "doc", "docx", "ppt", "pptx", "png", "jpg", "jpeg", "html"]);

const POLL_INTERVAL_MS = 10_000;
const POLL_TIMEOUT_MS = 20 * 60_000;

const TOKEN = process.env.MINERU_API_TOKEN;
if (!TOKEN) throw new Error("MINERU_API_TOKEN 未设置 — 在项目根 .env 里填入（https://mineru.net 申请）");

const force = process.argv.includes("--force");
const batchArg = process.argv.includes("--batch") ? process.argv[process.argv.indexOf("--batch") + 1] : undefined;

async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`${API}${path}`, {
    ...init,
    headers: { Authorization: `Bearer ${TOKEN}`, "Content-Type": "application/json", ...init?.headers },
  });
  const text = await res.text();
  if (!res.ok) throw new Error(`MinerU HTTP ${res.status} ${path}: ${text}`);
  const json = JSON.parse(text) as { code: number; msg?: string; data: T };
  if (json.code !== 0) throw new Error(`MinerU code=${json.code} ${path}: ${json.msg ?? text}`);
  return json.data;
}

let batch_id: string;
if (batchArg) {
  batch_id = batchArg;
  console.log(`复用已有 batch_id = ${batch_id}，跳过上传，直接轮询…`);
} else {
  // ── 1. 收集待解析文件 ──
  const all = (await readdir(SOURCE_DIR)).filter((f) => SUPPORTED.has(parse(f).ext.slice(1).toLowerCase()));
  if (all.length === 0) throw new Error(`${SOURCE_DIR} 下没有可解析的文件`);

  const pending: string[] = [];
  for (const f of all) {
    const done = await Bun.file(join(OUT_DIR, parse(f).name, "content_list.json")).exists();
    if (done && !force) {
      console.log(`⏭️  跳过（已有产物）: ${f}`);
    } else {
      pending.push(f);
    }
  }
  if (pending.length === 0) {
    console.log("全部文件已有解析产物，无事可做（--force 可重跑）。");
    process.exit(0);
  }

  // ── 2. 申请上传 URL 并 PUT 文件 ──
  console.log(`申请上传 URL: ${pending.join(", ")}`);
  const data = await api<{ batch_id: string; file_urls: string[] }>("/file-urls/batch", {
    method: "POST",
    body: JSON.stringify({
      language: "ch",
      enable_formula: true,
      enable_table: true,
      model_version: process.env.MINERU_MODEL_VERSION ?? "pipeline",
      files: pending.map((name) => ({ name, is_ocr: true, data_id: parse(name).name })),
    }),
  });
  batch_id = data.batch_id;
  const file_urls = data.file_urls;
  if (file_urls.length !== pending.length) {
    throw new Error(`file_urls 数量(${file_urls.length})与文件数(${pending.length})不符`);
  }

  for (let i = 0; i < pending.length; i++) {
    const path = join(SOURCE_DIR, pending[i]!);
    // 预签名 OSS URL 按"无 Content-Type"签名：必须用裸字节 body，
    // 不能传 Bun.file（它会自动带 application/pdf 导致 SignatureDoesNotMatch 403）
    const bytes = new Uint8Array(await Bun.file(path).arrayBuffer());
    const res = await fetch(file_urls[i]!, { method: "PUT", body: bytes });
    if (!res.ok) throw new Error(`上传失败 ${pending[i]} HTTP ${res.status}: ${await res.text()}`);
    console.log(`⬆️  已上传: ${pending[i]}`);
  }
  console.log(`batch_id = ${batch_id}，开始轮询…`);
}

// ── 3. 轮询解析结果 ──
type ExtractResult = {
  file_name: string;
  state: "done" | "failed" | "pending" | "running" | "converting" | "waiting-file";
  full_zip_url?: string;
  err_msg?: string;
  extract_progress?: { extracted_pages?: number; total_pages?: number };
};

let results: ExtractResult[] = [];
const deadline = Date.now() + POLL_TIMEOUT_MS;
while (true) {
  const data = await api<{ extract_result: ExtractResult[] }>(`/extract-results/batch/${batch_id}`);
  results = data.extract_result;
  const line = results
    .map((r) => {
      const prog = r.extract_progress;
      const pct = prog?.total_pages ? ` ${prog.extracted_pages}/${prog.total_pages}页` : "";
      return `${r.file_name}=${r.state}${pct}`;
    })
    .join("  ");
  console.log(`   ${new Date().toLocaleTimeString()}  ${line}`);

  const failed = results.filter((r) => r.state === "failed");
  for (const f of failed) console.error(`❌ 解析失败: ${f.file_name}: ${f.err_msg}`);
  if (results.every((r) => r.state === "done" || r.state === "failed")) break;
  if (Date.now() > deadline) throw new Error(`轮询超时（${POLL_TIMEOUT_MS / 60000} 分钟），batch_id=${batch_id}`);
  await Bun.sleep(POLL_INTERVAL_MS);
}

// ── 4. 下载 zip 并解包 ──
let ok = 0;
for (const r of results) {
  if (r.state !== "done") continue;
  if (!r.full_zip_url) throw new Error(`${r.file_name} state=done 却无 full_zip_url`);
  const stem = parse(r.file_name).name;
  const dest = join(OUT_DIR, stem);
  await rm(dest, { recursive: true, force: true });
  await mkdir(dest, { recursive: true });

  const zipRes = await fetch(r.full_zip_url);
  if (!zipRes.ok) throw new Error(`下载 zip 失败 ${r.file_name}: HTTP ${zipRes.status}`);
  const zipPath = join(dest, "_result.zip");
  await Bun.write(zipPath, await zipRes.arrayBuffer());

  const unzip = Bun.spawnSync(["unzip", "-o", "-q", zipPath, "-d", dest]);
  if (unzip.exitCode !== 0) throw new Error(`unzip 失败 ${r.file_name}: ${unzip.stderr.toString()}`);
  await rm(zipPath);

  // zip 里的产物名带 UUID 前缀（如 <uuid>_content_list.json，另有 _v2 新格式变体）。
  // 归一化出经典 schema 的 content_list.json（docfuse 消费的正是它）。
  if (!(await Bun.file(join(dest, "content_list.json")).exists())) {
    const files = await readdir(dest);
    const classic = files.find((f) => f.endsWith("content_list.json") && !f.endsWith("content_list_v2.json"));
    if (!classic) {
      throw new Error(`${r.file_name} 的 zip 里没有 content_list.json，实际内容: ${files.join(", ")}`);
    }
    await Bun.write(join(dest, "content_list.json"), Bun.file(join(dest, classic)));
  }
  const items = (await Bun.file(join(dest, "content_list.json")).json()) as unknown[];
  console.log(`✅ ${r.file_name} → test_data/mineru/${stem}/  (${items.length} 个 items)`);
  ok++;
}

console.log(`\n完成：${ok}/${results.length} 个文件解析落盘。下一步: bun run refine:real`);
