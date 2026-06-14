// 用 MinerU 官方 API（https://mineru.net/apiManage/docs）解析单个文件，
// 把解析产物（content_list.json / full.md / layout.json / images/ …）落盘到 <out-dir>。
//
// 流程：POST /api/v4/file-urls/batch 拿上传 URL → PUT 裸字节 → 轮询 /api/v4/extract-results/batch/{id}
//      → state=done 后下载 full_zip_url → unzip → 归一化出经典 content_list.json。
//
// 跑：  bun mineru_fetch.ts <input-file> <out-dir>
//   env: MINERU_API_TOKEN（必需，https://mineru.net 申请）
//        MINERU_MODEL_VERSION（可选，默认 "pipeline"）
//   Bun 会自动加载 cwd 下的 .env，所以从含 .env 的工作目录运行即可。
//
// 末行输出一行 JSON：{ ok, stem, outDir, items }，供上层程序化读取。

import { mkdir, readdir, rm } from "node:fs/promises";
import { basename, extname, join } from "node:path";

const API = "https://mineru.net/api/v4";
const SUPPORTED = new Set(["pdf", "doc", "docx", "ppt", "pptx", "png", "jpg", "jpeg", "html"]);
const POLL_INTERVAL_MS = 10_000;
const POLL_TIMEOUT_MS = 20 * 60_000;

const [input, outDir] = process.argv.slice(2);
if (!input || !outDir) throw new Error("用法: bun mineru_fetch.ts <input-file> <out-dir>");

const TOKEN = process.env.MINERU_API_TOKEN;
if (!TOKEN) throw new Error("MINERU_API_TOKEN 未设置 — 在工作目录 .env 里填入（https://mineru.net 申请）");

const ext = extname(input).slice(1).toLowerCase();
if (!SUPPORTED.has(ext)) throw new Error(`不支持的文件类型 .${ext}（支持: ${[...SUPPORTED].join(", ")}）`);
if (!(await Bun.file(input).exists())) throw new Error(`输入文件不存在: ${input}`);

const name = basename(input);
const stem = basename(input, extname(input));

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

// ── 1. 申请上传 URL ──
console.log(`申请上传 URL: ${name}`);
const data = await api<{ batch_id: string; file_urls: string[] }>("/file-urls/batch", {
  method: "POST",
  body: JSON.stringify({
    language: "ch",
    enable_formula: true,
    enable_table: true,
    model_version: process.env.MINERU_MODEL_VERSION ?? "pipeline",
    files: [{ name, is_ocr: true, data_id: stem }],
  }),
});
const batch_id = data.batch_id;
const uploadUrl = data.file_urls[0];
if (!uploadUrl) throw new Error("MinerU 没有返回上传 URL");

// ── 2. PUT 文件 ──
// 预签名 OSS URL 按"无 Content-Type"签名：必须用裸字节 body，
// 不能让 fetch 自动带 application/pdf，否则 SignatureDoesNotMatch 403。
const bytes = new Uint8Array(await Bun.file(input).arrayBuffer());
const put = await fetch(uploadUrl, { method: "PUT", body: bytes });
if (!put.ok) throw new Error(`上传失败 HTTP ${put.status}: ${await put.text()}`);
console.log(`⬆️  已上传，batch_id = ${batch_id}，开始轮询…`);

// ── 3. 轮询解析结果 ──
type ExtractResult = {
  file_name: string;
  state: "done" | "failed" | "pending" | "running" | "converting" | "waiting-file";
  full_zip_url?: string;
  err_msg?: string;
  extract_progress?: { extracted_pages?: number; total_pages?: number };
};

let result: ExtractResult | undefined;
const deadline = Date.now() + POLL_TIMEOUT_MS;
while (true) {
  const poll = await api<{ extract_result: ExtractResult[] }>(`/extract-results/batch/${batch_id}`);
  result = poll.extract_result.find((r) => r.file_name === name) ?? poll.extract_result[0];
  if (!result) throw new Error(`轮询返回空结果，batch_id=${batch_id}`);
  const prog = result.extract_progress;
  const pct = prog?.total_pages ? ` ${prog.extracted_pages}/${prog.total_pages}页` : "";
  console.log(`   ${new Date().toLocaleTimeString()}  ${result.file_name}=${result.state}${pct}`);
  if (result.state === "failed") throw new Error(`MinerU 解析失败: ${result.err_msg ?? "未知错误"}`);
  if (result.state === "done") break;
  if (Date.now() > deadline) throw new Error(`轮询超时（${POLL_TIMEOUT_MS / 60000} 分钟），batch_id=${batch_id}`);
  await Bun.sleep(POLL_INTERVAL_MS);
}

// ── 4. 下载 zip 并解包 + 归一化 ──
if (!result.full_zip_url) throw new Error(`state=done 却无 full_zip_url`);
await rm(outDir, { recursive: true, force: true });
await mkdir(outDir, { recursive: true });

const zipRes = await fetch(result.full_zip_url);
if (!zipRes.ok) throw new Error(`下载 zip 失败: HTTP ${zipRes.status}`);
const zipPath = join(outDir, "_result.zip");
await Bun.write(zipPath, await zipRes.arrayBuffer());

const unzip = Bun.spawnSync(["unzip", "-o", "-q", zipPath, "-d", outDir]);
if (unzip.exitCode !== 0) throw new Error(`unzip 失败: ${unzip.stderr.toString()}`);
await rm(zipPath);

// zip 里的产物名可能带 UUID 前缀（如 <uuid>_content_list.json，另有 _v2 新格式变体）。
// 归一化出经典 schema 的 content_list.json（mineru-refine 消费的正是它）。
if (!(await Bun.file(join(outDir, "content_list.json")).exists())) {
  const files = await readdir(outDir);
  const classic = files.find((f) => f.endsWith("content_list.json") && !f.endsWith("content_list_v2.json"));
  if (!classic) throw new Error(`zip 里没有 content_list.json，实际内容: ${files.join(", ")}`);
  await Bun.write(join(outDir, "content_list.json"), Bun.file(join(outDir, classic)));
}

const items = (await Bun.file(join(outDir, "content_list.json")).json()) as unknown[];
console.log(`✅ ${name} → ${outDir}/  (${items.length} 个 items)`);
console.log(JSON.stringify({ ok: true, stem, outDir, items: items.length }));
