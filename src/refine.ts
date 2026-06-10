// 入口：refine(items, opts) -> { items, provenance, report }。
// 收/发内存对象。fail-open：任何异常/LLM 不可用 → 原样返回输入 + 大声 log。
// 出口闸门：保真不变式 + 异常数单调 + 几何，任一不过 → fail-open。

import { assignIds, stripIds } from "./id.ts";
import { detect } from "./detect.ts";
import { checkFidelity } from "./invariant.ts";
import { runLoop, skippedWithoutVision, type ChatFn, type LoadImageFn } from "./loop.ts";
import type { VisionJudgeFn } from "./qwen_vl.ts";
import type { MineruItem, ProvenanceEntry, RefineReport, RefineResult, WorkItem } from "./types.ts";

export const REFINE_LOGIC_VERSION = "0.6.0"; // 0.5：split_table 视觉裁决；0.5.1：maxIterations 自适应默认；0.6：拆表检测放宽到链式（页码不要求相邻）+ split_table 仅视觉裁决（无视觉模型/视觉失败一律搁置，文本路径撤掉 mergeTable）
export const PROMPT_VERSION = "p4"; // p4：system prompt 与工具集移除 mergeTable
export const MODEL_ID = "deepseek-v4-pro";

export type RefineOptions = {
  markdown?: string; // 可选原始 markdown（当前仅留作上下文扩展位）
  sha256?: string; // 源文件 SHA256；提供时启用缓存
  maxIterations?: number; // 外层循环硬上限；不传则自适应（adaptiveMaxIterations，随疑点数 48~512）
  concurrency?: number; // 并行裁决的疑点数（默认 8；1 = 严格串行）
  /** 只读图片访问器（imgPath 相对路径 → 字节）。split_table 仅视觉裁决：提供时走 Qwen-VL（取不到图/视觉失败 → 搁置该疑点）；不提供 = 无视觉模型，split_table 整体跳过。任何情况下都不走文本路径做 mergeTable。 */
  loadImage?: LoadImageFn;
  /** 内部/测试用：注入 LLM 调用（默认 DeepSeek 裸 API）。 */
  chatFn?: ChatFn;
  /** 内部/测试用：注入视觉裁决（默认 Qwen-VL 裸 API）。 */
  visionFn?: VisionJudgeFn;
  log?: (msg: string) => void;
};

/**
 * maxIterations 的自适应默认值：随初始可处理疑点数走，固定常数对大文档必然截断。
 * 2× 给"修复解锁新疑点"留余量（实测大文档总工作量 ≈ 1.6× 初始疑点数：空壳 drop 后
 * 表格变相邻冒出新拆表对等），下限 48 保持小文档现状，上限 512 兜病态文档的成本。
 * 显式传 opts.maxIterations 时不走这里。
 */
export function adaptiveMaxIterations(actionableSuspects: number): number {
  return Math.min(Math.max(48, 2 * actionableSuspects + 16), 512);
}

/** 便捷构造：以 baseDir 为根的只读图片访问器（imgPath 缺失/读取失败 → null，绝不抛）。 */
export function imageDirLoader(baseDir: string): LoadImageFn {
  return async (imgPath: string) => {
    try {
      const file = Bun.file(`${baseDir}/${imgPath}`);
      if (!(await file.exists())) return null;
      return new Uint8Array(await file.arrayBuffer());
    } catch {
      return null;
    }
  };
}

// 缓存 key = sha256(源文件) + refineLogicVersion + model + promptVersion（只用源文件 SHA256 是错的）
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
  const provenance: ProvenanceEntry[] = []; // 纯削减模式（不加字）→ 恒为空，结构预留

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
    // 无视觉模型时 split_table 整体跳过，不计入迭代预算，也不参与"异常数单调"闸门
    //（跳过的疑点原样留在输出里，按原计数会被误判为"修不动"触发 fail-open）
    const hasVision = !!opts.loadImage;
    const gateCountable = (w: WorkItem) => w.hasOp && !skippedWithoutVision(w, hasVision);
    const inputSuspects = detect(ref).filter(gateCountable).length;
    const refBefore = ref.map((r) => ({ id: r.id, item: structuredClone(r.item) }));

    const loop = await runLoop(ref, nextId, {
      maxIterations: opts.maxIterations ?? adaptiveMaxIterations(inputSuspects),
      concurrency: opts.concurrency,
      chatFn: opts.chatFn,
      loadImage: opts.loadImage,
      visionFn: opts.visionFn,
      log,
    });

    // ── 出口闸门（合格判定）──
    const fidelity = checkFidelity(refBefore, loop.items);
    if (!fidelity.ok) return failOpen(`出口保真闸门不过: ${fidelity.reason}`);

    const outputSuspects = detect(loop.items).filter(gateCountable).length;
    if (outputSuspects > inputSuspects) {
      return failOpen(`异常数不单调: 输入 ${inputSuspects} → 输出 ${outputSuspects}`);
    }

    const result: RefineResult = {
      items: stripIds(loop.items), // 出口剥除内部 ID（schema 透明）
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
