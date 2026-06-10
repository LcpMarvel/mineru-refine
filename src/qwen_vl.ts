// Qwen-VL 裸 API 客户端（无 SDK）。DashScope OpenAI 兼容端点，仅做【判定类】视觉裁决——
// 输出是决策（merge/dismiss），不是内容字符，不碰纯削减保真红线。
// 当前唯一用途：split_table 疑点的"是否同一张表"裁决（吃 MinerU 已落盘的表格裁剪图）。

import { parseJsonSafe } from "safe-json-repair";
import type { Usage } from "./deepseek.ts";

export type SplitTableVerdict = {
  verdict: "merge" | "dismiss";
  reason: string;
  usage: Pick<Usage, "prompt_tokens" | "completion_tokens">;
};

/** 视觉裁决函数签名（依赖注入，测试用 mock）。 */
export type VisionJudgeFn = (imgA: Uint8Array, imgB: Uint8Array) => Promise<SplitTableVerdict>;

const DEFAULT_BASE_URL = "https://dashscope.aliyuncs.com/compatible-mode/v1";
const DEFAULT_MODEL = "qwen-vl-max";
const MAX_ATTEMPTS = 3;
const RETRYABLE_STATUS = new Set([429, 500, 502, 503, 504]);

const PROMPT =
  "图1是 PDF 某页末尾的表格，图2是紧接着的下一页开头的表格。" +
  "判断图2是否是图1这张表被分页拆开的延续部分（看列网格是否同一套、切缝处内容/编号是否接续、图2有无自己独立的表头主题）。" +
  '只输出 JSON：{"verdict":"merge"|"dismiss","reason":"一句话依据"}，merge=同一张表的延续，dismiss=两张不同的表。';

function dataUrl(img: Uint8Array): string {
  return `data:image/jpeg;base64,${Buffer.from(img).toString("base64")}`;
}

/** 裸 fetch 问 Qwen-VL「图2是否图1的续表」。缺 key 立即抛（项目约定：早抛，不静默降级——回退由调用方决定）。 */
export async function judgeSplitTable(imgA: Uint8Array, imgB: Uint8Array): Promise<SplitTableVerdict> {
  const key = process.env.QWEN_APIKEY;
  if (!key) throw new Error("QWEN_APIKEY 未设置 — 在 .env 里填（视觉裁决需要）");
  const baseUrl = process.env.QWEN_BASE_URL ?? DEFAULT_BASE_URL;
  const model = process.env.QWEN_VISION_MODEL ?? DEFAULT_MODEL;

  const body = JSON.stringify({
    model,
    temperature: 0,
    messages: [
      {
        role: "user",
        content: [
          { type: "text", text: PROMPT },
          { type: "image_url", image_url: { url: dataUrl(imgA) } },
          { type: "image_url", image_url: { url: dataUrl(imgB) } },
        ],
      },
    ],
  });

  let lastErr: Error | undefined;
  for (let attempt = 1; attempt <= MAX_ATTEMPTS; attempt++) {
    if (attempt > 1) await Bun.sleep(attempt * 1500);
    let res: Response;
    try {
      res = await fetch(`${baseUrl}/chat/completions`, {
        method: "POST",
        headers: { Authorization: `Bearer ${key}`, "Content-Type": "application/json" },
        body,
      });
    } catch (e) {
      lastErr = new Error(`Qwen-VL 网络错误（第 ${attempt}/${MAX_ATTEMPTS} 次）: ${(e as Error).message}`);
      continue;
    }
    if (!res.ok) {
      const text = await res.text();
      if (RETRYABLE_STATUS.has(res.status)) {
        lastErr = new Error(`Qwen-VL HTTP ${res.status}（第 ${attempt}/${MAX_ATTEMPTS} 次）: ${text.slice(0, 300)}`);
        continue;
      }
      throw new Error(`Qwen-VL HTTP ${res.status}: ${text.slice(0, 500)}`);
    }

    const json: any = await res.json();
    const content: string = json.choices?.[0]?.message?.content ?? "";
    const m = content.match(/\{[\s\S]*\}/);
    const parsed = m ? parseJsonSafe<{ verdict?: string; reason?: string }>(m[0]) : undefined;
    if (!parsed || (parsed.verdict !== "merge" && parsed.verdict !== "dismiss")) {
      throw new Error(`Qwen-VL 回复不是合法裁决 JSON: ${content.slice(0, 200)}`);
    }
    return {
      verdict: parsed.verdict,
      reason: parsed.reason ?? "（未给依据）",
      usage: {
        prompt_tokens: json.usage?.prompt_tokens ?? 0,
        completion_tokens: json.usage?.completion_tokens ?? 0,
      },
    };
  }
  throw lastErr ?? new Error("Qwen-VL 重试耗尽");
}
