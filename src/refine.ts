// 入口（SPEC §2/§14）：refine(items, opts) -> { items, provenance, report }。
// 收/发内存对象（D3）。fail-open：任何异常/LLM 不可用 → 原样返回输入 + 大声 log。
// 出口闸门（§7③）：保真不变式 + 异常数单调 + 几何，任一不过 → fail-open。

import { assignIds, stripIds } from "./id.ts";
import { detect } from "./detect.ts";
import { checkFidelity } from "./invariant.ts";
import { runLoop, type ChatFn } from "./loop.ts";
import type { MineruItem, ProvenanceEntry, RefineReport, RefineResult } from "./types.ts";

export const REFINE_LOGIC_VERSION = "0.3.0";
export const PROMPT_VERSION = "p2";
export const MODEL_ID = "deepseek-v4-pro";

export type RefineOptions = {
  markdown?: string; // 可选原始 markdown（当前仅留作上下文扩展位）
  sha256?: string; // 源文件 SHA256；提供时启用缓存
  maxIterations?: number;
  concurrency?: number; // 并行裁决的疑点数（默认 8；1 = 严格串行）
  /** 内部/测试用：注入 LLM 调用（默认 DeepSeek 裸 API）。 */
  chatFn?: ChatFn;
  log?: (msg: string) => void;
};

// 缓存 key = sha256(源文件) + refineLogicVersion + model + promptVersion（§2：只用 SHA256 是错的）
const cache = new Map<string, RefineResult>();

export function cacheKeyFor(sha256: string): string {
  return `${sha256}:${REFINE_LOGIC_VERSION}:${MODEL_ID}:${PROMPT_VERSION}`;
}

/** 测试/运维用：清空进程内缓存。 */
export function clearRefineCache(): void {
  cache.clear();
}

function emptyReport(): RefineReport {
  return {
    iterations: 0,
    opCounts: {},
    dismissed: 0,
    removedSpans: [],
    violations: 0,
    tokenUsage: { prompt: 0, completion: 0 },
    failOpen: false,
  };
}

export async function refine(items: MineruItem[], opts: RefineOptions = {}): Promise<RefineResult> {
  const log = opts.log ?? ((m: string) => console.error(`[mineru-refine] ${m}`));
  const provenance: ProvenanceEntry[] = []; // D4=(c) 纯削减 → 恒为空（§5a）

  if (!Array.isArray(items)) throw new Error("refine: items 必须是数组（content_list）");

  const key = opts.sha256 ? cacheKeyFor(opts.sha256) : null;
  if (key) {
    const hit = cache.get(key);
    if (hit) return structuredClone(hit);
  }

  // fail-open 基准：输入的不可变快照
  const snapshot = structuredClone(items);

  const failOpen = (why: string): RefineResult => {
    log(`FAIL-OPEN：${why} —— 原样返回输入 ${snapshot.length} 个 items`);
    return { items: structuredClone(snapshot), provenance: [], report: { ...emptyReport(), failOpen: true } };
  };

  try {
    const { ref, nextId } = assignIds(snapshot);
    const inputSuspects = detect(ref).filter((w) => w.hasOp).length;
    const refBefore = ref.map((r) => ({ id: r.id, item: structuredClone(r.item) }));

    const loop = await runLoop(ref, nextId, {
      maxIterations: opts.maxIterations,
      concurrency: opts.concurrency,
      chatFn: opts.chatFn,
      log,
    });

    // ── 出口闸门（§7③ / §10 合格判定）──
    const fidelity = checkFidelity(refBefore, loop.items);
    if (!fidelity.ok) return failOpen(`出口保真闸门不过: ${fidelity.reason}`);

    const outputSuspects = detect(loop.items).filter((w) => w.hasOp).length;
    if (outputSuspects > inputSuspects) {
      return failOpen(`异常数不单调: 输入 ${inputSuspects} → 输出 ${outputSuspects}`);
    }

    const result: RefineResult = {
      items: stripIds(loop.items), // 出口剥除内部 ID（§4a / §2 schema 透明）
      provenance,
      report: {
        iterations: loop.iterations,
        opCounts: loop.opCounts,
        dismissed: loop.dismissed,
        removedSpans: loop.removedSpans,
        violations: loop.violations,
        tokenUsage: loop.tokenUsage,
        failOpen: false,
      },
    };

    if (key) cache.set(key, structuredClone(result));
    return result;
  } catch (e) {
    return failOpen(`异常: ${(e as Error).stack ?? (e as Error).message}`);
  }
}
