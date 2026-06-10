// 确定性外层循环（SPEC §7/§10）：弹出 worklist 疑点 → 交 LLM（带上下文）→
// LLM 回一个 op 或 dismiss → 执行（保真闸+回滚）→ 重探测。loop-until-dry + 守卫。
// LLM 不当司机：每个疑点一个独立小对话，工具集固定，tool_choice:required。

import { parseJsonSafe } from "safe-json-repair";
import { chat, type ChatResult, type Message, type Tool, type ToolCall } from "./deepseek.ts";
import { detect, droppableIds } from "./detect.ts";
import type { IdGen } from "./id.ts";
import { mustIndexOfId, indexOfId } from "./id.ts";
import { inputPages } from "./invariant.ts";
import { applyOpChecked } from "./ops/index.ts";
import type { OpCall, RefItem, RemovedSpan, StripPattern, WorkItem } from "./types.ts";

export type ChatFn = typeof chat;

export type LoopResult = {
  items: RefItem[];
  iterations: number;
  opCounts: Record<string, number>;
  dismissed: number;
  removedSpans: RemovedSpan[];
  violations: number;
  tokenUsage: { prompt: number; completion: number };
};

export type LoopOptions = {
  maxIterations?: number; // 外层硬上限（§10）
  maxRoundsPerSuspect?: number; // 单疑点内层对话轮数上限
  concurrency?: number; // 同批并行裁决的疑点数（1 = 严格串行）
  chatFn?: ChatFn; // 依赖注入，测试用 mock
  log?: (msg: string) => void;
};

const DEFAULT_MAX_ITERATIONS = 48;
const DEFAULT_MAX_ROUNDS = 8;
const DEFAULT_CONCURRENCY = 8;

// ── 工具定义（§8 全集 → DeepSeek function schema）──

const idParam = { type: "string", description: "item 的稳定 ID（如 it_0003），来自疑点描述或观察工具" };

export const TOOLS: Tool[] = [
  // 观察类（只读）
  {
    type: "function",
    function: {
      name: "outline",
      description: "返回全文标题骨架：所有 header / 带 text_level 的块的 ID、层级、文本。用于判断某块在章节结构中的位置。",
      parameters: { type: "object", properties: {} },
    },
  },
  {
    type: "function",
    function: {
      name: "getItems",
      description: "查看某 item 及其前后相邻块的完整内容（含类型、页码、文本全文）。",
      parameters: {
        type: "object",
        properties: {
          id: idParam,
          before: { type: "integer", description: "向前取几个相邻块，默认 1" },
          after: { type: "integer", description: "向后取几个相邻块，默认 1" },
        },
        required: ["id"],
      },
    },
  },
  {
    type: "function",
    function: {
      name: "whyFlagged",
      description: "查看探测器为何标记某块（该块当前所有疑点及证据）。",
      parameters: { type: "object", properties: { id: idParam }, required: ["id"] },
    },
  },
  {
    type: "function",
    function: {
      name: "peekPage",
      description: "查看某块所在页及上下页的全部块（跨页判断必需：merge 前必须用它确认上下页内容连续）。",
      parameters: { type: "object", properties: { id: idParam }, required: ["id"] },
    },
  },
  // 裁决类
  {
    type: "function",
    function: {
      name: "dismiss",
      description: "判定当前疑点为误报，不做任何改动。拿不准时宁可 dismiss，不可错改/误删真标题。",
      parameters: {
        type: "object",
        properties: { id: idParam, reason: { type: "string", description: "为何是误报" } },
        required: ["id", "reason"],
      },
    },
  },
  // 变更类（7 个削减/重组 op）
  {
    type: "function",
    function: {
      name: "merge",
      description:
        "把两个 text 块拼成一块（修跨页断句）。idB 须在 idA 之后，两者之间只允许隔页眉/页码/页脚（页面家具会原位保留）。合并前必须先 peekPage 确认上下页内容连续。",
      parameters: {
        type: "object",
        properties: { idA: { ...idParam, description: "前块 ID" }, idB: { ...idParam, description: "紧随其后的块 ID" } },
        required: ["idA", "idB"],
      },
    },
  },
  {
    type: "function",
    function: {
      name: "split",
      description: "把一个 text 块在字符 offset 处切成两块（拆巨型块）。offset 是 text 中的字符位置（0 < offset < 长度），应切在自然段/小标题边界。",
      parameters: {
        type: "object",
        properties: { id: idParam, offset: { type: "integer", description: "切分点字符位置" } },
        required: ["id", "offset"],
      },
    },
  },
  {
    type: "function",
    function: {
      name: "demote",
      description: "把被误判为标题的块降级为正文（清除 text_level）。",
      parameters: { type: "object", properties: { id: idParam }, required: ["id"] },
    },
  },
  {
    type: "function",
    function: {
      name: "promote",
      description: "把 text 块升为标题（设 text_level=level，1 最高）。",
      parameters: {
        type: "object",
        properties: { id: idParam, level: { type: "integer", description: "标题层级 1-6" } },
        required: ["id", "level"],
      },
    },
  },
  {
    type: "function",
    function: {
      name: "reorder",
      description: "重排一段连续区间内的块顺序（修跨页错序）。传入这些块 ID 的正确顺序，它们必须在文档中本就连续。",
      parameters: {
        type: "object",
        properties: { idsInOrder: { type: "array", items: { type: "string" }, description: "按正确顺序排列的稳定 ID 列表" } },
        required: ["idsInOrder"],
      },
    },
  },
  {
    type: "function",
    function: {
      name: "drop",
      description: "删除混入正文的页码/页眉/页脚/水印块。只允许删被探测器标记为 page_artifact 的块。",
      parameters: { type: "object", properties: { id: idParam }, required: ["id"] },
    },
  },
  {
    type: "function",
    function: {
      name: "strip",
      description:
        "去掉块内残留符号。pattern 白名单：md_link（[文字](url)→文字）、latex_dollar（$\\mathsf{x}$→x 去定界符和命令残骸）、latex_block（整段 $...$ 删除）、latex_command（删无定界符的裸 \\命令 和花括号残骸）、escaped_dollar（\\$→$ 去转义反斜杠，如 \\$APPEALS）、html_tag（删 HTML 标签）。",
      parameters: {
        type: "object",
        properties: {
          id: idParam,
          pattern: { type: "string", enum: ["md_link", "latex_dollar", "latex_block", "latex_command", "escaped_dollar", "html_tag"] },
        },
        required: ["id", "pattern"],
      },
    },
  },
];

const OP_NAMES = new Set(["merge", "split", "demote", "promote", "reorder", "drop", "strip"]);
const OBSERVE_NAMES = new Set(["outline", "getItems", "whyFlagged", "peekPage"]);

// system prompt 稳定不变（放 messages 前缀吃 DeepSeek prefix cache，§11）。
const SYSTEM_PROMPT = `你是 MinerU PDF 解析结果的结构修复器（linter/fixer）。文档被解析成块（item）数组，每块有稳定 ID、类型（text/header/table/list/page_number/image）、页码 page_idx 和文本。

你的任务：对【当前疑点】做一次裁决。你只能调用工具，绝不输出正文文本。

规则：
1. 不确定就先观察：getItems 看上下文、peekPage 看整页、outline 看章节骨架、whyFlagged 看证据。
2. 跨页 merge 前【必须】先 peekPage 确认上下页内容确实连续（中间无标题/表格/无关块）。
3. 拿不准就 dismiss（宁可漏修，不可错改/误删真标题）。
   - 伪标题裁决前先看 outline：若存在结构平行的同级编号标题（如 4.1/4.2/4.3…，即使含逗号或表引用），通常是真标题 → dismiss。
   - 列表项（-、•、①、(1) 等开头的行）之间绝不 merge——行尾无标点是列表的常态，不是断句。
   - 但 page_artifact 证据若给出「已分类页眉/页脚同文佐证」，说明同文块在别处已被正确分类为页面家具，该块就是漏标的同款 → 应 drop，不要因「像标题」而 dismiss。
   - 同一文本的多处 page_artifact 疑点应裁决一致：要删都删，不要删一处留其余。
4. 修复只许削减/重组（merge/split/demote/promote/reorder/drop/strip），系统会机器校验"不新增任何字符"，违规会被自动回滚。
5. 每个疑点最终以【一个】变更 op 或 dismiss 收尾。`;

// ── 观察工具实现（确定性，只读）──

function fmtItem(r: RefItem, maxText = 600): string {
  const it = r.item;
  const fields: string[] = [`id=${r.id}`, `type=${it.type}`, `page=${it.page_idx}`];
  if (it.text_level !== undefined) fields.push(`text_level=${it.text_level}`);
  if (typeof it.text === "string") {
    const t = it.text.length > maxText ? `${it.text.slice(0, maxText)}…(共${it.text.length}字)` : it.text;
    fields.push(`text=「${t}」`);
  }
  if (Array.isArray(it.list_items)) fields.push(`list_items=${JSON.stringify(it.list_items).slice(0, 300)}`);
  if (Array.isArray(it.table_caption)) fields.push(`table_caption=${JSON.stringify(it.table_caption)}`);
  if (typeof it.table_body === "string") fields.push(`table_body=(${it.table_body.length} bytes HTML，不可修改)`);
  if (typeof it.img_path === "string") fields.push(`img_path=${it.img_path}`);
  return fields.join(" | ");
}

function execObserve(name: string, args: Record<string, unknown>, items: RefItem[], worklist: WorkItem[]): string {
  switch (name) {
    case "outline": {
      // 注意 MinerU 的 type=header 是页眉而非标题；文档标题 = text + text_level
      const heads = items.filter((r) => r.item.text_level !== undefined);
      if (heads.length === 0) return "（全文没有任何标题块）";
      return heads
        .map((r) => `${r.id} L${r.item.text_level ?? "?"} ${(r.item.text ?? "").slice(0, 60)}`)
        .join("\n");
    }
    case "getItems": {
      const i = mustIndexOfId(items, String(args.id));
      const before = Math.min(Math.max(Number(args.before ?? 1), 0), 5);
      const after = Math.min(Math.max(Number(args.after ?? 1), 0), 5);
      const lo = Math.max(0, i - before);
      const hi = Math.min(items.length - 1, i + after);
      return items
        .slice(lo, hi + 1)
        .map((r) => (r.id === args.id ? `>>> ${fmtItem(r, 2000)}` : `    ${fmtItem(r)}`))
        .join("\n");
    }
    case "whyFlagged": {
      const flags = worklist.filter((w) => w.itemId === args.id);
      if (flags.length === 0) return `${args.id} 当前没有疑点。`;
      return flags.map((w) => `[${w.kind}]${w.hasOp ? "" : "（仅标记，无对应 op，只能 dismiss）"} ${w.evidence}`).join("\n");
    }
    case "peekPage": {
      const i = mustIndexOfId(items, String(args.id));
      const page = items[i]!.item.page_idx;
      if (typeof page !== "number") return `${args.id} 没有 page_idx。`;
      const lines: string[] = [];
      for (const p of [page - 1, page, page + 1]) {
        const inPage = items.filter((r) => r.item.page_idx === p);
        if (inPage.length === 0) continue;
        lines.push(`── 第 ${p} 页 ──`);
        for (const r of inPage) lines.push(r.id === args.id ? `>>> ${fmtItem(r)}` : `    ${fmtItem(r)}`);
      }
      return lines.join("\n");
    }
    default:
      throw new Error(`未知观察工具: ${name}`);
  }
}

function toOpCall(name: string, args: Record<string, unknown>): OpCall {
  switch (name) {
    case "merge":
      return { op: "merge", idA: String(args.idA), idB: String(args.idB) };
    case "split":
      return { op: "split", id: String(args.id), offset: Number(args.offset) };
    case "demote":
      return { op: "demote", id: String(args.id) };
    case "promote":
      return { op: "promote", id: String(args.id), level: Number(args.level) };
    case "reorder":
      return { op: "reorder", idsInOrder: (args.idsInOrder as string[]).map(String) };
    case "drop":
      return { op: "drop", id: String(args.id) };
    case "strip":
      return { op: "strip", id: String(args.id), pattern: String(args.pattern) as StripPattern };
    default:
      throw new Error(`未知 op: ${name}`);
  }
}

/** 防震荡（§10）：禁止刚做过的逆操作。merge 产物禁 split；split 产物对禁 merge。 */
class OscillationGuard {
  private bannedSplitIds = new Set<string>(); // merge 产物
  private bannedMergePairs = new Set<string>(); // split 产物对 "idA+idB"

  record(call: OpCall, newIds: string[]): void {
    if (call.op === "merge" && newIds[0]) this.bannedSplitIds.add(newIds[0]);
    if (call.op === "split" && newIds.length === 2) this.bannedMergePairs.add(`${newIds[0]}+${newIds[1]}`);
  }

  rejects(call: OpCall): string | null {
    if (call.op === "split" && this.bannedSplitIds.has(call.id)) {
      return `${call.id} 是刚 merge 出来的块，禁止立刻 split（防震荡）`;
    }
    if (call.op === "merge" && this.bannedMergePairs.has(`${call.idA}+${call.idB}`)) {
      return `${call.idA}+${call.idB} 是刚 split 出来的块对，禁止立刻 merge 回去（防震荡）`;
    }
    return null;
  }
}

const suspectKey = (w: WorkItem) => `${w.kind}:${w.itemId}`;

// ── 主循环 ──

export async function runLoop(initial: RefItem[], nextId: IdGen, opts: LoopOptions = {}): Promise<LoopResult> {
  const maxIterations = opts.maxIterations ?? DEFAULT_MAX_ITERATIONS;
  const maxRounds = opts.maxRoundsPerSuspect ?? DEFAULT_MAX_ROUNDS;
  const chatFn = opts.chatFn ?? chat;
  const log = opts.log ?? ((m) => console.error(`[mineru-refine] ${m}`));
  const validPages = inputPages(initial);

  const concurrency = Math.max(1, opts.concurrency ?? DEFAULT_CONCURRENCY);

  // 共享文档状态：并行对话各自观察/落 op 都读写它。JS 单线程，op 落地（applyOpChecked
  // → 替换 state.items）是原子的；并行对话间的冲突（目标 ID 已被别的 op 吃掉）表现为
  // invalid_args，作为工具结果反馈给 LLM，由它改判或 dismiss。
  const state = { items: initial };
  const dismissedKeys = new Set<string>(); // 误报裁决集（§10 防永不终止）
  const guard = new OscillationGuard();
  const opCounts: Record<string, number> = {};
  const removedSpans: RemovedSpan[] = [];
  const tokenUsage = { prompt: 0, completion: 0 };
  let violations = 0;
  let iterations = 0;
  let llmSuccesses = 0; // 至少一次成功 → 后续单点 LLM 故障只搁置疑点；全程零成功 → 上抛触发 fail-open

  // loop-until-dry：worklist（有 op、未 dismiss）弹空才到底
  while (iterations < maxIterations) {
    const worklist = detect(state.items);
    const actionable = worklist.filter((w) => w.hasOp && !dismissedKeys.has(suspectKey(w)));
    if (actionable.length === 0) break;

    // 一批最多 concurrency 个疑点并行裁决（不同位置的块相互独立，这是主要提速来源）
    const batch = actionable.slice(0, Math.min(concurrency, maxIterations - iterations));
    iterations += batch.length;

    const ctx = { nextId, validPages, chatFn, maxRounds, guard, tokenUsage, log };
    const llmErrors: Error[] = [];
    await Promise.all(
      batch.map(async (target) => {
        let outcome: SuspectOutcome;
        try {
          outcome = await handleSuspect(target, state, worklist, ctx);
        } catch (e) {
          // 单疑点 LLM 故障（重试耗尽）：搁置该疑点，不毁全局（其它并行对话照常收尾）
          llmErrors.push(e as Error);
          dismissedKeys.add(suspectKey(target));
          log(`疑点 ${suspectKey(target)} LLM 调用失败，搁置: ${(e as Error).message}`);
          return;
        }
        llmSuccesses++;
        if (outcome.kind === "applied") {
          opCounts[outcome.opName] = (opCounts[outcome.opName] ?? 0) + 1;
          removedSpans.push(...outcome.removedSpans);
        } else {
          // dismiss（LLM 主动 / 轮数耗尽 / op 被闸门回滚后放弃）→ 计入裁决集，重探测不再标记
          dismissedKeys.add(suspectKey(target));
          violations += outcome.violations;
          if (outcome.reason !== "llm_dismiss") log(`疑点 ${suspectKey(target)} 强制搁置: ${outcome.reason}`);
        }
      }),
    );
    // LLM 整体不可用（全程一次都没成功过）→ 上抛，由 refine() fail-open（§2 失败行为）
    if (llmErrors.length > 0 && llmSuccesses === 0) throw llmErrors[0];
  }

  if (iterations >= maxIterations) log(`到达 maxIterations=${maxIterations}，强停（§10）`);

  return {
    items: state.items,
    iterations,
    opCounts,
    dismissed: dismissedKeys.size,
    removedSpans,
    violations,
    tokenUsage,
  };
}

type SuspectOutcome =
  | { kind: "applied"; opName: string; removedSpans: RemovedSpan[] }
  | { kind: "dismissed"; reason: string; violations: number };

async function handleSuspect(
  target: WorkItem,
  state: { items: RefItem[] },
  worklist: WorkItem[],
  ctx: {
    nextId: IdGen;
    validPages: ReadonlySet<number>;
    chatFn: ChatFn;
    maxRounds: number;
    guard: OscillationGuard;
    tokenUsage: { prompt: number; completion: number };
    log: (m: string) => void;
  },
): Promise<SuspectOutcome> {
  const OP_HINTS: Partial<Record<WorkItem["kind"], string>> = {
    pseudo_heading: "确认是被误判的正文 → demote；确认是真标题 → dismiss",
    cross_page_break: "确认上下页内容连续 → merge；不连续 → dismiss",
    giant_block: "找到自然边界 → split；本就是一整段 → dismiss",
    page_artifact: "确认是页码/页眉/页脚/水印（非正文）→ drop；是正文 → dismiss。证据含「家具佐证」的基本可直接 drop",
    residual_markup: "确认是解析残留 → strip（选对 pattern：$...$ 用 latex_dollar、裸 \\命令{} 用 latex_command、\\$ 用 escaped_dollar）；本就该有 → dismiss",
  };

  // 上下文前置：把裁决最可能需要的观察结果直接放进首条消息，省掉 1-2 轮观察往返。
  // 跨页疑点预载整页上下文（等价 peekPage），其余预载 ±2 邻居（等价 getItems）。
  let preload = "";
  try {
    preload =
      target.kind === "cross_page_break"
        ? `所在页及上下页内容（peekPage 预载）：\n${execObserve("peekPage", { id: target.itemId }, state.items, worklist)}`
        : `相邻上下文（getItems ±2 预载）：\n${execObserve("getItems", { id: target.itemId, before: 2, after: 2 }, state.items, worklist)}`;
  } catch {
    preload = "（目标块已不存在，无法预载上下文）";
  }

  const i = indexOfId(state.items, target.itemId);
  const messages: Message[] = [
    { role: "system", content: SYSTEM_PROMPT },
    {
      role: "user",
      content:
        `当前疑点：[${target.kind}] item ${target.itemId}\n证据：${target.evidence}\n\n` +
        `该块当前内容：\n${i >= 0 ? fmtItem(state.items[i]!, 2000) : "（已不存在）"}\n\n` +
        `${preload}\n\n` +
        `该类疑点的典型处置：${OP_HINTS[target.kind] ?? "无对应 op，只能 dismiss（仅标记类）"}\n` +
        `若以上上下文已足够判断，请直接给出一个变更 op 或 dismiss；不够再调观察工具（outline 看章节骨架尤其有用）。`,
    },
  ];

  let violationCount = 0;

  for (let round = 0; round < ctx.maxRounds; round++) {
    // 倒数第二轮起强制收敛：实测大文档上 LLM 容易反复观察不裁决，烧满轮数被搁置
    if (round === ctx.maxRounds - 2) {
      messages.push({
        role: "user",
        content: "观察轮数即将用完。请基于已有信息【现在就裁决】：给出一个变更 op，或拿不准就 dismiss。不要再调用观察工具。",
      });
    }
    // LLM 异常直接上抛，由 refine() 的 fail-open 兜（§2 失败行为）
    const r: ChatResult = await ctx.chatFn(messages, TOOLS, { toolChoice: "required" });
    ctx.tokenUsage.prompt += r.usage?.prompt_tokens ?? 0;
    ctx.tokenUsage.completion += r.usage?.completion_tokens ?? 0;

    const calls = r.message.tool_calls;
    if (!calls || calls.length === 0) {
      return { kind: "dismissed", reason: "llm_no_tool_call", violations: violationCount };
    }
    messages.push({ role: "assistant", content: r.message.content ?? null, tool_calls: calls });

    for (const call of calls) {
      const name = call.function.name;
      const args = parseJsonSafe<Record<string, unknown>>(call.function.arguments);
      if (args === undefined) {
        messages.push({ role: "tool", tool_call_id: call.id, content: `arguments 解析失败，请重试: ${call.function.arguments.slice(0, 200)}` });
        continue;
      }

      if (OBSERVE_NAMES.has(name)) {
        let content: string;
        try {
          content = execObserve(name, args, state.items, worklist); // 读最新状态（并行 op 落地后立即可见）
        } catch (e) {
          content = `观察失败: ${(e as Error).message}`;
        }
        messages.push({ role: "tool", tool_call_id: call.id, content });
        continue;
      }

      if (name === "dismiss") {
        ctx.log(`dismiss [${target.kind}] ${target.itemId}: ${String(args.reason ?? "（未给理由）")}`);
        return { kind: "dismissed", reason: "llm_dismiss", violations: violationCount };
      }

      if (OP_NAMES.has(name)) {
        // 无 op 的标记类疑点（D5）只能 dismiss——但变更类 op 若合法依然允许（LLM 可能顺手修别的可修项？不：钉死单疑点单 op 语义，仍执行闸门校验即可）
        let opCall: OpCall;
        try {
          opCall = toOpCall(name, args);
        } catch (e) {
          messages.push({ role: "tool", tool_call_id: call.id, content: `参数错误: ${(e as Error).message}` });
          continue;
        }
        const banned = ctx.guard.rejects(opCall);
        if (banned) {
          messages.push({ role: "tool", tool_call_id: call.id, content: `被拒（${banned}）。请 dismiss 或换别的 op。` });
          continue;
        }
        const result = applyOpChecked(state.items, opCall, {
          nextId: ctx.nextId,
          validPages: ctx.validPages,
          droppableIds: droppableIds(worklist),
        });
        if (result.ok) {
          ctx.guard.record(opCall, result.newIds);
          state.items = result.items; // 原子落地（JS 单线程，无并发写竞争）
          return { kind: "applied", opName: name, removedSpans: result.removedSpans };
        }
        if (result.kind === "fidelity_violation") {
          violationCount++;
          ctx.log(`保真闸回滚 ${name}(${JSON.stringify(args)}): ${result.reason}`);
        }
        messages.push({
          role: "tool",
          tool_call_id: call.id,
          content: `op 被拒绝（${result.kind === "fidelity_violation" ? "保真闸门回滚" : "参数非法"}）: ${result.reason}。请观察后换 op 或 dismiss。`,
        });
        continue;
      }

      messages.push({ role: "tool", tool_call_id: call.id, content: `未知工具 ${name}。` });
    }
  }

  return { kind: "dismissed", reason: "max_rounds_exhausted", violations: violationCount };
}
