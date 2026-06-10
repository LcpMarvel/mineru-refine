// 7 个削减/重组 op（SPEC §8，D4=(c) 全集）。纯函数：(items, args) -> 新 items，绝不突变入参。
// applyOpChecked 是唯一对外执行入口：执行 + 保真闸（§5）+ 几何派生（§6），违反即回滚（丢弃副本）。
// 参数一律稳定 ID（§4a）。op 自身参数非法（ID 不存在 / 不相邻 / 不在白名单）直接抛错。

import type { IdGen } from "../id.ts";
import { mustIndexOfId } from "../id.ts";
import { checkFidelity } from "../invariant.ts";
import { PAGE_FURNITURE_TYPES, type MineruItem, type OpCall, type RefItem, type RemovedSpan, type StripPattern } from "../types.ts";

type OpOutcome = { items: RefItem[]; removedSpans: RemovedSpan[] };

function cloneItem(r: RefItem): MineruItem {
  return structuredClone(r.item);
}

function nonWs(s: string): string {
  return s.replace(/\s+/g, "");
}

// ── merge(idA, idB)：两 text 块拼成一块（修跨页断句）。bbox=并集，page_idx 取首块（§6）。
// 两块之间只允许隔着页面家具（header/page_number/footer），家具原位保留在合并块之后。──
function opMerge(items: RefItem[], nextId: IdGen, idA: string, idB: string): OpOutcome {
  const ia = mustIndexOfId(items, idA);
  const ib = mustIndexOfId(items, idB);
  if (ib <= ia) throw new Error(`merge 要求 ${idB} 在 ${idA} 之后（实际位置 ${ia} / ${ib}）`);
  const between = items.slice(ia + 1, ib);
  const blocker = between.find((r) => !PAGE_FURNITURE_TYPES.has(r.item.type));
  if (blocker) {
    throw new Error(`merge 被拒：${idA} 与 ${idB} 之间隔着内容块 ${blocker.id}（type=${blocker.item.type}），仅允许隔页面家具`);
  }
  const a = items[ia]!.item;
  const b = items[ib]!.item;
  if (a.type !== "text" || b.type !== "text") throw new Error(`merge 仅限 text 块（实际 ${a.type} + ${b.type}）`);
  if (typeof a.text !== "string" || typeof b.text !== "string") throw new Error("merge 的两块都必须有 text");

  const head = a.text.replace(/\s+$/, "");
  const tail = b.text.replace(/^\s+/, "");
  // 英文断词处补一个空格（空白符不计入 C 比对，§5 白名单）；中文直接拼。
  const glue = /[A-Za-z0-9,;]$/.test(head) && /^[A-Za-z0-9]/.test(tail) ? " " : "";

  const merged: MineruItem = { ...cloneItem(items[ia]!), text: head + glue + tail };
  if (Array.isArray(a.bbox) && Array.isArray(b.bbox) && a.bbox.length === 4 && b.bbox.length === 4) {
    merged.bbox = [
      Math.min(a.bbox[0]!, b.bbox[0]!),
      Math.min(a.bbox[1]!, b.bbox[1]!),
      Math.max(a.bbox[2]!, b.bbox[2]!),
      Math.max(a.bbox[3]!, b.bbox[3]!),
    ];
  }
  const out = items.slice();
  // merge 产一个新 ID（§4a）；A/B 之间的页面家具原位保留在合并块之后
  out.splice(ia, ib - ia + 1, { id: nextId(), item: merged }, ...between);
  return { items: out, removedSpans: [] };
}

// ── split(id, offset)：text 在字符 offset 处切两块。两子块继承父 bbox/page_idx（§6 一期）。──
function opSplit(items: RefItem[], nextId: IdGen, id: string, offset: number): OpOutcome {
  const i = mustIndexOfId(items, id);
  const it = items[i]!.item;
  if (it.type !== "text" || typeof it.text !== "string") throw new Error(`split 仅限 text 块（${id} 是 ${it.type}）`);
  if (!Number.isInteger(offset) || offset <= 0 || offset >= it.text.length) {
    throw new Error(`split offset 越界：${offset}（text 长 ${it.text.length}，须在开区间内）`);
  }
  const headText = it.text.slice(0, offset).replace(/\s+$/, "");
  const tailText = it.text.slice(offset).replace(/^\s+/, "");
  if (nonWs(headText).length === 0 || nonWs(tailText).length === 0) {
    throw new Error(`split 产生空块：offset=${offset} 切出的某一半无内容字符`);
  }
  const head: MineruItem = { ...cloneItem(items[i]!), text: headText };
  const tail: MineruItem = { ...cloneItem(items[i]!), text: tailText };
  delete tail.text_level; // 切出的后块默认正文；若实为小标题，由后续 promote 处理
  const out = items.slice();
  out.splice(i, 1, { id: nextId(), item: head }, { id: nextId(), item: tail }); // split 产两个新 ID
  return { items: out, removedSpans: [] };
}

// ── demote(id)：伪标题降为正文（清 text_level）。继承原 ID。──
function opDemote(items: RefItem[], id: string): OpOutcome {
  const i = mustIndexOfId(items, id);
  const it = items[i]!.item;
  if (it.text_level === undefined) throw new Error(`demote：${id} 本就没有 text_level`);
  const item = cloneItem(items[i]!);
  delete item.text_level;
  const out = items.slice();
  out[i] = { id, item };
  return { items: out, removedSpans: [] };
}

// ── promote(id, level)：text 升为 header。继承原 ID。──
function opPromote(items: RefItem[], id: string, level: number): OpOutcome {
  const i = mustIndexOfId(items, id);
  const it = items[i]!.item;
  if (it.type !== "text" || typeof it.text !== "string") throw new Error(`promote 仅限 text 块（${id} 是 ${it.type}）`);
  if (!Number.isInteger(level) || level < 1 || level > 6) throw new Error(`promote level 非法：${level}`);
  const item = cloneItem(items[i]!);
  item.text_level = level;
  const out = items.slice();
  out[i] = { id, item };
  return { items: out, removedSpans: [] };
}

// ── reorder(idsInOrder)：仅允许对一个【连续区间】内的块重排（修跨页错序），各块 ID/bbox/page_idx 不变。──
function opReorder(items: RefItem[], idsInOrder: string[]): OpOutcome {
  if (!Array.isArray(idsInOrder) || idsInOrder.length < 2) throw new Error("reorder 至少需要 2 个 ID");
  if (new Set(idsInOrder).size !== idsInOrder.length) throw new Error("reorder ID 重复");
  const indices = idsInOrder.map((id) => mustIndexOfId(items, id)).sort((x, y) => x - y);
  const lo = indices[0]!;
  const hi = indices[indices.length - 1]!;
  if (hi - lo !== idsInOrder.length - 1) {
    throw new Error(`reorder 的 ID 必须构成连续区间（实际散布在 [${lo}, ${hi}]）`);
  }
  const out = items.slice();
  idsInOrder.forEach((id, k) => {
    out[lo + k] = items[mustIndexOfId(items, id)]!;
  });
  return { items: out, removedSpans: [] };
}

// ── drop(id)：删页码/页眉/页脚/水印。白名单：type=page_number，或短 text/header（≤120 内容字符）。──
const DROP_MAX_CHARS = 120;

function opDrop(items: RefItem[], id: string, droppableIds?: ReadonlySet<string>): OpOutcome {
  const i = mustIndexOfId(items, id);
  const it = items[i]!.item;
  const isPageNumber = it.type === "page_number";
  const isShortText =
    (it.type === "text" || it.type === "header") &&
    typeof it.text === "string" &&
    nonWs(it.text).length <= DROP_MAX_CHARS;
  if (!isPageNumber && !isShortText) {
    throw new Error(`drop 白名单不命中：${id}（type=${it.type}）只允许删页码或 ≤${DROP_MAX_CHARS} 字的短文本`);
  }
  if (droppableIds && !droppableIds.has(id)) {
    throw new Error(`drop 被拒：${id} 未被探测器标记为 page_artifact 疑点`);
  }
  const out = items.slice();
  out.splice(i, 1);
  const removed = typeof it.text === "string" ? it.text : `[${it.type}]`;
  return { items: out, removedSpans: [{ itemId: id, text: removed, reason: "drop" }] };
}

// ── strip(id, pattern)：去残留符号，pattern 仅限白名单（不收任意 regex）。继承原 ID。──
// 把公式体里的 LaTeX 命令残骸剥成内容字符：\mathsf { A i j } { = } 1 → A i j = 1。
// 只删不增（命令名/花括号被移除，内容字符与空白保留），C_out ⊆ C_in 天然成立。
function stripLatexCommands(body: string): string {
  return body.replace(/\\[a-zA-Z]+/g, " ").replace(/[{}]/g, " ").replace(/\s+/g, " ").trim();
}

const STRIP_PATTERNS: Record<StripPattern, { re: RegExp; keep: (m: RegExpExecArray) => string }> = {
  md_link: { re: /\[([^\]]*)\]\(([^)]*)\)/g, keep: (m) => m[1]! }, // [t](url) → t
  latex_dollar: { re: /\$([^$\n]+)\$/g, keep: (m) => stripLatexCommands(m[1]!) }, // $\mathsf{x}$ → x（去定界符+命令残骸）
  latex_block: { re: /\$[^$\n]+\$/g, keep: () => "" }, // 整段公式删除（内容进 removedSpans 审计）
  // 无 $ 定界符的裸命令残骸（latex_dollar 旧版只去定界符留下的，或 MinerU 直接吐出的）
  latex_command: { re: /\\[a-zA-Z]+|[{}]/g, keep: () => "" },
  escaped_dollar: { re: /\\\$/g, keep: () => "$" }, // \$APPEALS → $APPEALS（去转义反斜杠）
  // 只删已知 HTML 标签名：宽泛匹配会误删正文里的「<表单编号 …>」类引用（真实数据踩过）
  html_tag: {
    re: /<\/?(?:br|hr|b|i|u|s|em|strong|sub|sup|span|div|p|a|img|font|center|small|big|del|ins|mark|code|pre|table|tbody|thead|tr|td|th)(?:\s[^<>]*)?\/?>/gi,
    keep: () => "",
  },
};

function opStrip(items: RefItem[], id: string, pattern: StripPattern): OpOutcome {
  const spec = STRIP_PATTERNS[pattern];
  if (!spec) throw new Error(`strip pattern 不在白名单：${pattern}`);
  const i = mustIndexOfId(items, id);
  const it = items[i]!.item;
  if (typeof it.text !== "string") throw new Error(`strip：${id} 没有 text 字段`);

  const removedSpans: RemovedSpan[] = [];
  const re = new RegExp(spec.re.source, spec.re.flags);
  const newText = it.text.replace(re, (...args) => {
    const m = [args[0], ...args.slice(1, -2)] as unknown as RegExpExecArray;
    const kept = spec.keep(m);
    removedSpans.push({ itemId: id, text: args[0] as string, reason: `strip:${pattern}` });
    return kept;
  });
  if (removedSpans.length === 0) throw new Error(`strip：${id} 中未匹配到 pattern ${pattern}，拒绝空操作`);
  if (nonWs(newText).length === 0) throw new Error(`strip 会把 ${id} 掏空，应改用 drop`);

  const item = cloneItem(items[i]!);
  item.text = newText;
  const out = items.slice();
  out[i] = { id, item };
  return { items: out, removedSpans };
}

// ── 调度 + 保真闸 ──

export type ApplyContext = {
  nextId: IdGen;
  /** 探测器当前标记为 page_artifact 的 id 集；提供时 drop 必须命中（双保险）。 */
  droppableIds?: ReadonlySet<string>;
  /** 输入文档的页集合（几何校验基准）。 */
  validPages: ReadonlySet<number>;
};

export type ApplyResult =
  | { ok: true; items: RefItem[]; removedSpans: RemovedSpan[]; newIds: string[] }
  | { ok: false; reason: string; kind: "invalid_args" | "fidelity_violation" };

/** 执行单个 op：参数非法 → invalid_args；保真闸不过 → fidelity_violation（回滚，原 items 不动）。 */
export function applyOpChecked(items: RefItem[], call: OpCall, ctx: ApplyContext): ApplyResult {
  const before = new Set(items.map((r) => r.id));
  let outcome: OpOutcome;
  try {
    switch (call.op) {
      case "merge":
        outcome = opMerge(items, ctx.nextId, call.idA, call.idB);
        break;
      case "split":
        outcome = opSplit(items, ctx.nextId, call.id, call.offset);
        break;
      case "demote":
        outcome = opDemote(items, call.id);
        break;
      case "promote":
        outcome = opPromote(items, call.id, call.level);
        break;
      case "reorder":
        outcome = opReorder(items, call.idsInOrder);
        break;
      case "drop":
        outcome = opDrop(items, call.id, ctx.droppableIds);
        break;
      case "strip":
        outcome = opStrip(items, call.id, call.pattern);
        break;
      default:
        return { ok: false, reason: `未知 op: ${(call as { op: string }).op}`, kind: "invalid_args" };
    }
  } catch (e) {
    return { ok: false, reason: (e as Error).message, kind: "invalid_args" };
  }

  // 保真闸（§5）：drop/strip 是有意削减，字符子集天然成立；闸门防的是 op 实现 bug 与几何破坏。
  const fidelity = checkFidelity(items, outcome.items, ctx.validPages);
  if (!fidelity.ok) {
    return { ok: false, reason: fidelity.reason, kind: "fidelity_violation" };
  }
  const newIds = outcome.items.filter((r) => !before.has(r.id)).map((r) => r.id);
  return { ok: true, items: outcome.items, removedSpans: outcome.removedSpans, newIds };
}
