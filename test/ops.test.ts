// 各 op 的纯函数语义 + 固定 op 序列 replay + 保真闸回滚（不接 LLM）。

import { describe, expect, test } from "bun:test";
import { assignIds } from "../src/id.ts";
import { checkTableBodies, inputPages } from "../src/invariant.ts";
import { applyOpChecked, type ApplyContext } from "../src/ops/index.ts";
import type { MineruItem, OpCall, RefItem } from "../src/types.ts";
import { bbox, goldenInput } from "./helpers.ts";

function setup(items: MineruItem[]): { ref: RefItem[]; ctx: ApplyContext } {
  const { ref, nextId } = assignIds(items);
  return { ref, ctx: { nextId, validPages: inputPages(ref) } };
}

function mustApply(items: RefItem[], call: OpCall, ctx: ApplyContext) {
  const r = applyOpChecked(items, call, ctx);
  if (!r.ok) throw new Error(`op 应成功却失败: ${r.reason}`);
  return r;
}

describe("单 op 语义", () => {
  test("demote 清 text_level、继承原 ID；promote 反向", () => {
    const { ref, ctx } = setup([{ type: "text", text: "第一章", text_level: 1, page_idx: 0, bbox: bbox(0) }]);
    const d = mustApply(ref, { op: "demote", id: "it_0001" }, ctx);
    expect(d.items[0]!.id).toBe("it_0001");
    expect(d.items[0]!.item.text_level).toBeUndefined();
    expect("text_level" in d.items[0]!.item).toBe(false); // 删字段，不是设 undefined

    const p = mustApply(d.items, { op: "promote", id: "it_0001", level: 2 }, ctx);
    expect(p.items[0]!.item.text_level).toBe(2);
    // 入参未被突变
    expect(ref[0]!.item.text_level).toBe(1);
  });

  test("merge 仅限相邻 text，产新 ID，bbox 并集，page_idx 取首块", () => {
    const { ref, ctx } = setup([
      { type: "text", text: "前半段未完", page_idx: 0, bbox: [10, 700, 500, 720] },
      { type: "text", text: "后半段收尾。", page_idx: 1, bbox: [10, 40, 480, 60] },
    ]);
    const r = mustApply(ref, { op: "merge", idA: "it_0001", idB: "it_0002" }, ctx);
    expect(r.items).toHaveLength(1);
    expect(r.newIds).toEqual(["it_0003"]);
    expect(r.items[0]!.item.text).toBe("前半段未完后半段收尾。");
    expect(r.items[0]!.item.page_idx).toBe(0);
    expect(r.items[0]!.item.bbox).toEqual([10, 40, 500, 720]);
  });

  test("merge 英文断词处补空格（空白不计入 C 比对）", () => {
    const { ref, ctx } = setup([
      { type: "text", text: "the quick brown", page_idx: 0, bbox: bbox(0) },
      { type: "text", text: "fox jumps.", page_idx: 1, bbox: bbox(0) },
    ]);
    const r = mustApply(ref, { op: "merge", idA: "it_0001", idB: "it_0002" }, ctx);
    expect(r.items[0]!.item.text).toBe("the quick brown fox jumps.");
  });

  test("merge 非相邻 / 非 text → invalid_args", () => {
    const { ref, ctx } = setup([
      { type: "text", text: "a", page_idx: 0, bbox: bbox(0) },
      { type: "table", table_body: "<table></table>", table_caption: ["t"], page_idx: 0, bbox: bbox(20) },
      { type: "text", text: "b", page_idx: 0, bbox: bbox(40) },
    ]);
    expect(applyOpChecked(ref, { op: "merge", idA: "it_0001", idB: "it_0003" }, ctx).ok).toBe(false);
    expect(applyOpChecked(ref, { op: "merge", idA: "it_0001", idB: "it_0002" }, ctx).ok).toBe(false);
  });

  test("split 产两个新 ID，子块继承 bbox/page_idx，后块清 text_level", () => {
    const text = "1.1 范围。本规范适用于全公司。1.2 术语。下列术语适用本文件。";
    const offset = text.indexOf("1.2");
    const { ref, ctx } = setup([{ type: "text", text, text_level: 1, page_idx: 3, bbox: bbox(100) }]);
    const r = mustApply(ref, { op: "split", id: "it_0001", offset }, ctx);
    expect(r.items).toHaveLength(2);
    expect(r.newIds).toEqual(["it_0002", "it_0003"]);
    expect(r.items[0]!.item.text).toBe("1.1 范围。本规范适用于全公司。");
    expect(r.items[1]!.item.text).toBe("1.2 术语。下列术语适用本文件。");
    expect(r.items[0]!.item.bbox).toEqual(bbox(100));
    expect(r.items[1]!.item.page_idx).toBe(3);
    expect(r.items[1]!.item.text_level).toBeUndefined();
  });

  test("split offset 越界 / 切出空块 → invalid_args", () => {
    const { ref, ctx } = setup([{ type: "text", text: "甲乙  丙", page_idx: 0, bbox: bbox(0) }]);
    expect(applyOpChecked(ref, { op: "split", id: "it_0001", offset: 0 }, ctx).ok).toBe(false);
    expect(applyOpChecked(ref, { op: "split", id: "it_0001", offset: 99 }, ctx).ok).toBe(false);
  });

  test("reorder 仅限连续区间的排列，各块 ID 不变", () => {
    const { ref, ctx } = setup([
      { type: "text", text: "a", page_idx: 0, bbox: bbox(0) },
      { type: "text", text: "b", page_idx: 0, bbox: bbox(20) },
      { type: "text", text: "c", page_idx: 1, bbox: bbox(0) },
      { type: "text", text: "d", page_idx: 1, bbox: bbox(20) },
    ]);
    const r = mustApply(ref, { op: "reorder", idsInOrder: ["it_0003", "it_0002"] }, ctx);
    expect(r.items.map((x) => x.id)).toEqual(["it_0001", "it_0003", "it_0002", "it_0004"]);
    // 非连续区间被拒
    expect(applyOpChecked(ref, { op: "reorder", idsInOrder: ["it_0004", "it_0001"] }, ctx).ok).toBe(false);
  });

  test("drop 白名单：page_number/短文本可删，长正文与 table 不可", () => {
    const { ref, ctx } = setup([
      { type: "page_number", text: "3", page_idx: 0, bbox: bbox(780) },
      { type: "text", text: "很长的正文。".repeat(50), page_idx: 0, bbox: bbox(100) },
      { type: "table", table_body: "<table></table>", table_caption: ["t"], page_idx: 0, bbox: bbox(300) },
    ]);
    const r = mustApply(ref, { op: "drop", id: "it_0001" }, ctx);
    expect(r.items).toHaveLength(2);
    expect(r.removedSpans).toEqual([{ itemId: "it_0001", text: "3", reason: "drop" }]);
    expect(applyOpChecked(ref, { op: "drop", id: "it_0002" }, ctx).ok).toBe(false);
    expect(applyOpChecked(ref, { op: "drop", id: "it_0003" }, ctx).ok).toBe(false);
  });

  test("drop 提供 droppableIds 时必须命中（探测器双保险）", () => {
    const { ref, ctx } = setup([{ type: "page_number", text: "3", page_idx: 0, bbox: bbox(780) }]);
    const denied = applyOpChecked(ref, { op: "drop", id: "it_0001" }, { ...ctx, droppableIds: new Set<string>() });
    expect(denied.ok).toBe(false);
  });

  test("strip 各白名单 pattern；removedSpans 留痕；空匹配/掏空被拒", () => {
    const { ref, ctx } = setup([
      { type: "text", text: "见[附件](http://a.b/c)与$x+y$及<b>加粗</b>。", page_idx: 0, bbox: bbox(0) },
    ]);
    const r1 = mustApply(ref, { op: "strip", id: "it_0001", pattern: "md_link" }, ctx);
    expect(r1.items[0]!.item.text).toBe("见附件与$x+y$及<b>加粗</b>。");
    expect(r1.removedSpans[0]).toEqual({ itemId: "it_0001", text: "[附件](http://a.b/c)", reason: "strip:md_link" });

    const r2 = mustApply(r1.items, { op: "strip", id: "it_0001", pattern: "latex_dollar" }, ctx);
    expect(r2.items[0]!.item.text).toBe("见附件与x+y及<b>加粗</b>。");

    const r3 = mustApply(r2.items, { op: "strip", id: "it_0001", pattern: "html_tag" }, ctx);
    expect(r3.items[0]!.item.text).toBe("见附件与x+y及加粗。");

    // 已无可匹配 → 拒绝空操作
    expect(applyOpChecked(r3.items, { op: "strip", id: "it_0001", pattern: "md_link" }, ctx).ok).toBe(false);
  });

  test("strip latex_dollar 连命令残骸一起剥（真实数据回归）", () => {
    const { ref, ctx } = setup([
      { type: "text", text: "每个元素均大于零，且 $\\mathsf { A i j } ^ { * } \\mathsf { A j i } { = } 1$ 。", page_idx: 0, bbox: bbox(0) },
    ]);
    const r = mustApply(ref, { op: "strip", id: "it_0001", pattern: "latex_dollar" }, ctx);
    expect(r.items[0]!.item.text).toBe("每个元素均大于零，且 A i j ^ * A j i = 1 。");
    expect(r.removedSpans[0]!.text).toBe("$\\mathsf { A i j } ^ { * } \\mathsf { A j i } { = } 1$");
  });

  test("strip latex_command 删无定界符的裸命令/花括号残骸", () => {
    const { ref, ctx } = setup([
      { type: "text", text: "一般，如果 { \\mathsf { C R } } { < } 0 . 1 ，则通过一致性检验。", page_idx: 0, bbox: bbox(0) },
    ]);
    const r = mustApply(ref, { op: "strip", id: "it_0001", pattern: "latex_command" }, ctx);
    expect(r.items[0]!.item.text!.replace(/\s+/g, "")).toBe("一般，如果CR<0.1，则通过一致性检验。");
    expect(r.items[0]!.item.text).not.toMatch(/[\\{}]/);
  });

  test("strip escaped_dollar：\\$APPEALS → $APPEALS", () => {
    const { ref, ctx } = setup([
      { type: "text", text: "通过\\$APPEALS客户需求分析工具了解客户需求。", page_idx: 0, bbox: bbox(0) },
    ]);
    const r = mustApply(ref, { op: "strip", id: "it_0001", pattern: "escaped_dollar" }, ctx);
    expect(r.items[0]!.item.text).toBe("通过$APPEALS客户需求分析工具了解客户需求。");
    expect(r.removedSpans).toEqual([{ itemId: "it_0001", text: "\\$", reason: "strip:escaped_dollar" }]);
  });

  test("strip latex_block 整段删除并留痕", () => {
    const { ref, ctx } = setup([{ type: "text", text: "推导 $\\frac{a}{b}=c$ 略。", page_idx: 0, bbox: bbox(0) }]);
    const r = mustApply(ref, { op: "strip", id: "it_0001", pattern: "latex_block" }, ctx);
    expect(r.items[0]!.item.text).toBe("推导  略。");
    expect(r.removedSpans[0]!.text).toBe("$\\frac{a}{b}=c$");
  });
});

describe("保真闸回滚（fidelity_violation）", () => {
  test("几何闸：validPages 不含该页 → 任何 op 回滚，原 items 不动", () => {
    const { ref } = setup([{ type: "text", text: "x", text_level: 1, page_idx: 0, bbox: bbox(0) }]);
    const { nextId } = assignIds([]); // 独立 id gen 无所谓
    const r = applyOpChecked(ref, { op: "demote", id: "it_0001" }, { nextId, validPages: new Set([99]) });
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.kind).toBe("fidelity_violation");
    expect(ref[0]!.item.text_level).toBe(1); // 未被突变
  });
});

describe("固定 op 序列 replay（不接 LLM）", () => {
  test("demote → merge → drop → strip 全链路 + C_out ⊆ C_in", () => {
    const { ref, ctx } = setup(goldenInput());
    let items = ref;
    const seq: OpCall[] = [
      { op: "demote", id: "it_0002" },
      { op: "merge", idA: "it_0003", idB: "it_0004" },
      { op: "drop", id: "it_0005" },
      { op: "strip", id: "it_0006", pattern: "md_link" },
    ];
    for (const call of seq) {
      const r = applyOpChecked(items, call, ctx);
      expect(r.ok).toBe(true);
      if (r.ok) items = r.items;
    }
    expect(items).toHaveLength(5); // 7 项：merge 并掉 1、drop 删掉 1
    expect(items[1]!.item.text_level).toBeUndefined();
    expect(items[2]!.item.text).toBe("战略管理是指公司为实现长期发展目标而进行的一系列计划、执行与评估活动。");
    expect(items.find((x) => x.item.text === "- 3 -")).toBeUndefined();
    expect(items[3]!.item.text).toBe("详见公司官网发布的文件。");
    // 旧 ID 已失效（merge 产新 ID）→ 再用旧 ID 是 invalid_args，不是错位执行
    expect(applyOpChecked(items, { op: "demote", id: "it_0003" }, ctx).ok).toBe(false);
  });
});

describe("merge 跨页面家具", () => {
  test("A/B 之间隔 header+page_number 可合并，家具原位保留；隔内容块被拒", async () => {
    const { assignIds } = await import("../src/id.ts");
    const { inputPages } = await import("../src/invariant.ts");
    const { applyOpChecked } = await import("../src/ops/index.ts");
    const items = [
      { type: "text", text: "前半句没说完", page_idx: 0, bbox: [50, 700, 550, 720] },
      { type: "header", text: "XX公司 版本K", page_idx: 0, bbox: [50, 10, 550, 30] },
      { type: "page_number", text: "第1页共9页", page_idx: 0, bbox: [50, 780, 550, 800] },
      { type: "text", text: "后半句收尾。", page_idx: 1, bbox: [50, 40, 550, 60] },
    ];
    const { ref, nextId } = assignIds(items);
    const ctx = { nextId, validPages: inputPages(ref) };
    const r = applyOpChecked(ref, { op: "merge", idA: "it_0001", idB: "it_0004" }, ctx);
    expect(r.ok).toBe(true);
    if (r.ok) {
      expect(r.items.map((x) => x.item.type)).toEqual(["text", "header", "page_number"]);
      expect(r.items[0]!.item.text).toBe("前半句没说完后半句收尾。");
    }
    // 中间隔着内容块（text）→ 拒绝
    const items2 = [
      { type: "text", text: "前半句没说完", page_idx: 0, bbox: [50, 700, 550, 720] },
      { type: "text", text: "插入的另一段。", page_idx: 0, bbox: [50, 740, 550, 760] },
      { type: "text", text: "后半句收尾。", page_idx: 1, bbox: [50, 40, 550, 60] },
    ];
    const s2 = assignIds(items2);
    const r2 = applyOpChecked(s2.ref, { op: "merge", idA: "it_0001", idB: "it_0003" }, { nextId: s2.nextId, validPages: inputPages(s2.ref) });
    expect(r2.ok).toBe(false);
  });
});

describe("html_tag 白名单（真实数据回归）", () => {
  test("正文里的表单引用 <MB-ZZ-155 部门OGSMT> 不被误删", () => {
    const { ref, ctx } = setup([
      { type: "text", text: "提交评审记录（表格<MB-ZZ-155 部门OGSMT>）。", page_idx: 0, bbox: bbox(0) },
    ]);
    const r = applyOpChecked(ref, { op: "strip", id: "it_0001", pattern: "html_tag" }, ctx);
    expect(r.ok).toBe(false); // 无已知标签可匹配 → 拒绝空操作
  });
});

describe("mergeTable（跨页拆表合并）", () => {
  const A: MineruItem = {
    type: "table",
    table_body: "<table><tbody>\n<tr><td>表头</td><td>列2</td></tr>\n<tr><td>甲</td><td>1</td></tr>\n</tbody></table>",
    table_caption: ["表1 示例"],
    table_footnote: ["注：A 的脚注"],
    page_idx: 0,
    bbox: [50, 100, 550, 800],
  };
  const B: MineruItem = {
    type: "table",
    table_body: "<table><tbody><tr><td>乙</td><td>2</td></tr><tr><td>丙</td><td>3</td></tr></tbody></table>",
    table_caption: ["（续）"],
    page_idx: 1,
    bbox: [50, 80, 550, 300],
  };

  test("B 行原字节追加到 A 末行后、A 外壳不动；caption/footnote 拼接；bbox 并集、page_idx 取首块；产新 ID", () => {
    const { ref, ctx } = setup([
      structuredClone(A),
      { type: "page_number", text: "1", page_idx: 0, bbox: bbox(780) },
      structuredClone(B),
    ]);
    const r = mustApply(ref, { op: "mergeTable", idA: "it_0001", idB: "it_0003" }, ctx);
    expect(r.items).toHaveLength(2); // 合并块 + 原位保留的页码
    const merged = r.items[0]!.item;
    expect(r.newIds).toEqual([r.items[0]!.id]);
    expect(merged.table_body).toBe(
      "<table><tbody>\n<tr><td>表头</td><td>列2</td></tr>\n<tr><td>甲</td><td>1</td></tr><tr><td>乙</td><td>2</td></tr><tr><td>丙</td><td>3</td></tr>\n</tbody></table>",
    );
    expect(merged.table_caption).toEqual(["表1 示例", "（续）"]);
    expect(merged.table_footnote).toEqual(["注：A 的脚注"]);
    expect(merged.page_idx).toBe(0);
    expect(merged.bbox).toEqual([50, 80, 550, 800]);
    expect(r.items[1]!.item.type).toBe("page_number"); // 家具原位保留
  });

  test("B 首行与 A 首行逐字节相同（每页重印表头）→ 去重并留痕；近似相同不去", () => {
    const dupB: MineruItem = {
      ...structuredClone(B),
      table_body: "<table><tbody><tr><td>表头</td><td>列2</td></tr><tr><td>乙</td><td>2</td></tr></tbody></table>",
    };
    const { ref, ctx } = setup([structuredClone(A), dupB]);
    const r = mustApply(ref, { op: "mergeTable", idA: "it_0001", idB: "it_0002" }, ctx);
    expect(r.items[0]!.item.table_body).toContain("<tr><td>甲</td><td>1</td></tr><tr><td>乙</td><td>2</td></tr>");
    expect(r.items[0]!.item.table_body!.match(/表头/g)).toHaveLength(1);
    expect(r.removedSpans).toEqual([
      { itemId: "it_0002", text: "<tr><td>表头</td><td>列2</td></tr>", reason: "mergeTable:dup_header" },
    ]);

    // 近似但不逐字节相等（多一个空格）→ 不去重
    const nearB: MineruItem = {
      ...structuredClone(B),
      table_body: "<table><tbody><tr><td>表头 </td><td>列2</td></tr><tr><td>乙</td><td>2</td></tr></tbody></table>",
    };
    const { ref: ref2, ctx: ctx2 } = setup([structuredClone(A), nearB]);
    const r2 = mustApply(ref2, { op: "mergeTable", idA: "it_0001", idB: "it_0002" }, ctx2);
    expect(r2.removedSpans).toHaveLength(0);
    expect(r2.items[0]!.item.table_body!.match(/表头/g)).toHaveLength(2);
  });

  test("列数不等（rowspan/尾列空被略去）也允许合并——参差行原样保留", () => {
    const ragged: MineruItem = {
      ...structuredClone(B),
      table_body: "<table><tbody><tr><td>乙</td><td>2</td><td>新列</td></tr></tbody></table>",
    };
    const { ref, ctx } = setup([structuredClone(A), ragged]);
    const r = mustApply(ref, { op: "mergeTable", idA: "it_0001", idB: "it_0002" }, ctx);
    expect(r.items[0]!.item.table_body).toContain("<tr><td>乙</td><td>2</td><td>新列</td></tr>");
  });

  test("空壳表 / 非 table / 隔内容块 → invalid_args", () => {
    const husk: MineruItem = { type: "table", img_path: "", table_caption: [], page_idx: 1, bbox: bbox(0) };
    const { ref, ctx } = setup([structuredClone(A), husk]);
    const r = applyOpChecked(ref, { op: "mergeTable", idA: "it_0001", idB: "it_0002" }, ctx);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.kind).toBe("invalid_args");

    const { ref: ref2, ctx: ctx2 } = setup([
      structuredClone(A),
      { type: "text", text: "中间的正文", page_idx: 0, bbox: bbox(500) },
      structuredClone(B),
    ]);
    const r2 = applyOpChecked(ref2, { op: "mergeTable", idA: "it_0001", idB: "it_0003" }, ctx2);
    expect(r2.ok).toBe(false);

    const { ref: ref3, ctx: ctx3 } = setup([structuredClone(A), { type: "text", text: "x", page_idx: 1, bbox: bbox(0) }]);
    const r3 = applyOpChecked(ref3, { op: "mergeTable", idA: "it_0001", idB: "it_0002" }, ctx3);
    expect(r3.ok).toBe(false);
  });
});

describe("mergeList（跨页拆列表合并）", () => {
  const LA: MineruItem = { type: "list", list_items: ["第一项", "第二项未完"], page_idx: 0, bbox: [50, 600, 550, 800] };
  const LB: MineruItem = { type: "list", list_items: ["的后半句。", "第三项"], page_idx: 1, bbox: [50, 80, 550, 200] };

  test("默认纯拼接；joinSeam=true 缝合 A 尾项与 B 首项；bbox 并集、page_idx 取首块", () => {
    const { ref, ctx } = setup([structuredClone(LA), structuredClone(LB)]);
    const r = mustApply(ref, { op: "mergeList", idA: "it_0001", idB: "it_0002" }, ctx);
    expect(r.items).toHaveLength(1);
    expect(r.items[0]!.item.list_items).toEqual(["第一项", "第二项未完", "的后半句。", "第三项"]);

    const { ref: ref2, ctx: ctx2 } = setup([structuredClone(LA), structuredClone(LB)]);
    const r2 = mustApply(ref2, { op: "mergeList", idA: "it_0001", idB: "it_0002", joinSeam: true }, ctx2);
    const merged = r2.items[0]!.item;
    expect(merged.list_items).toEqual(["第一项", "第二项未完的后半句。", "第三项"]);
    expect(merged.page_idx).toBe(0);
    expect(merged.bbox).toEqual([50, 80, 550, 800]);
  });

  test("joinSeam 英文断词处补空格", () => {
    const { ref, ctx } = setup([
      { type: "list", list_items: ["item one and"], page_idx: 0, bbox: bbox(700) },
      { type: "list", list_items: ["item two"], page_idx: 1, bbox: bbox(80) },
    ]);
    const r = mustApply(ref, { op: "mergeList", idA: "it_0001", idB: "it_0002", joinSeam: true }, ctx);
    expect(r.items[0]!.item.list_items).toEqual(["item one and item two"]);
  });

  test("非 list / 空 list_items → invalid_args", () => {
    const { ref, ctx } = setup([structuredClone(LA), { type: "text", text: "x", page_idx: 1, bbox: bbox(0) }]);
    expect(applyOpChecked(ref, { op: "mergeList", idA: "it_0001", idB: "it_0002" }, ctx).ok).toBe(false);
    const { ref: ref2, ctx: ctx2 } = setup([structuredClone(LA), { type: "list", list_items: [], page_idx: 1, bbox: bbox(0) }]);
    expect(applyOpChecked(ref2, { op: "mergeList", idA: "it_0001", idB: "it_0002" }, ctx2).ok).toBe(false);
  });
});

describe("drop 空壳表（白名单扩展）", () => {
  test("零内容空壳表可 drop；有行的表仍不可", () => {
    const { ref, ctx } = setup([
      { type: "table", img_path: "", table_caption: [], table_footnote: [], page_idx: 0, bbox: bbox(0) },
      { type: "table", table_body: "<table><tr><td>有内容</td></tr></table>", table_caption: [], page_idx: 0, bbox: bbox(300) },
    ]);
    const r = mustApply(ref, { op: "drop", id: "it_0001" }, ctx);
    expect(r.items).toHaveLength(1);
    expect(r.removedSpans).toEqual([{ itemId: "it_0001", text: "[table]", reason: "drop" }]);
    expect(applyOpChecked(ref, { op: "drop", id: "it_0002" }, ctx).ok).toBe(false);
  });

  test("droppableIds 提供时空壳必须命中 empty_table 标记（双保险）", () => {
    const { ref, ctx } = setup([
      { type: "table", img_path: "", table_caption: [], page_idx: 0, bbox: bbox(0) },
    ]);
    const denied = applyOpChecked(ref, { op: "drop", id: "it_0001" }, { ...ctx, droppableIds: new Set() });
    expect(denied.ok).toBe(false);
    const allowed = applyOpChecked(ref, { op: "drop", id: "it_0001" }, { ...ctx, droppableIds: new Set(["it_0001"]) });
    expect(allowed.ok).toBe(true);
  });
});

describe("mergeTable 列参差矩阵（空列被 MinerU 略去/保留的各种形态）", () => {
  /** 3 列逻辑表的第一页：尾列全空被 MinerU 略去 → 识别成 2 列。 */
  const A_TAIL_DROPPED: MineruItem = {
    type: "table",
    table_body:
      "<table><tbody><tr><td>名称</td><td>数量</td></tr><tr><td>甲</td><td>1</td></tr></tbody></table>",
    table_caption: ["表X"],
    page_idx: 0,
    bbox: [50, 100, 550, 800],
  };

  function merge2(a: MineruItem, b: MineruItem) {
    const { ref, ctx } = setup([structuredClone(a), structuredClone(b)]);
    return mustApply(ref, { op: "mergeTable", idA: "it_0001", idB: "it_0002" }, ctx);
  }

  test("尾列空被略去：A 2列 + B 3列 → 参差合并，A/B 行逐字节保留，不补不裁", () => {
    const b: MineruItem = {
      type: "table",
      table_body: "<table><tbody><tr><td>乙</td><td>2</td><td>备注B</td></tr></tbody></table>",
      table_caption: [],
      page_idx: 1,
      bbox: [50, 80, 550, 200],
    };
    const r = merge2(A_TAIL_DROPPED, b);
    const body = r.items[0]!.item.table_body!;
    expect(body).toContain("<tr><td>甲</td><td>1</td></tr>"); // A 的 2 列行原样
    expect(body).toContain("<tr><td>乙</td><td>2</td><td>备注B</td></tr>"); // B 的 3 列行原样
    expect(body.match(/<td><\/td>/g)).toBeNull(); // 绝不发明空单元格去"对齐"
  });

  test("首格空但被保留为 <td></td>（真实形态 JZY idx326）：列天然对齐，逐字节保留", () => {
    const b: MineruItem = {
      type: "table",
      table_body: "<table><tbody><tr><td></td><td>2</td></tr><tr><td>丙</td><td>3</td></tr></tbody></table>",
      table_caption: [],
      page_idx: 1,
      bbox: [50, 80, 550, 200],
    };
    const r = merge2(A_TAIL_DROPPED, b);
    expect(r.items[0]!.item.table_body).toContain("<tr><td>甲</td><td>1</td></tr><tr><td></td><td>2</td></tr>");
  });

  test("首列空被整个丢掉（B 行左移 1 格）：原样保留——错位是 MinerU 输入即有的，不引入新损伤", () => {
    const b: MineruItem = {
      type: "table",
      // 逻辑上是「(空), 2」但 MinerU 只吐了一格
      table_body: "<table><tbody><tr><td>2</td></tr></tbody></table>",
      table_caption: [],
      page_idx: 1,
      bbox: [50, 80, 550, 200],
    };
    const r = merge2(A_TAIL_DROPPED, b);
    expect(r.items[0]!.item.table_body).toContain("<tr><td>甲</td><td>1</td></tr><tr><td>2</td></tr>");
  });

  test("rowspan 跨页携带（真实形态 ZBZ-047 it_0193+it_0197）：首行列数 5≠4 仍可合并", () => {
    const a: MineruItem = {
      type: "table",
      table_body:
        "<table><tbody><tr><td rowspan=1 colspan=1>考核项目</td><td rowspan=1 colspan=1>权重</td><td rowspan=1 colspan=1>维度编号</td><td rowspan=1 colspan=1>评分标准</td><td rowspan=1 colspan=1>得分</td></tr><tr><td rowspan=1 colspan=1>评分依据：所直接关联的上级战略指标的达成情况。</td></tr></tbody></table>",
      table_caption: ["报告评分表"],
      page_idx: 13,
      bbox: [50, 100, 550, 800],
    };
    const b: MineruItem = {
      type: "table",
      table_body:
        "<table><tbody><tr><td rowspan=8 colspan=1>指标的战略协同与支撑</td><td rowspan=8 colspan=1></td><td rowspan=2 colspan=1></td><td></td></tr></tbody></table>",
      table_caption: [],
      page_idx: 14,
      bbox: [50, 80, 550, 300],
    };
    const r = merge2(a, b);
    const body = r.items[0]!.item.table_body!;
    expect(body).toContain("评分依据：所直接关联的上级战略指标的达成情况。</td></tr><tr><td rowspan=8");
    expect(r.items[0]!.item.table_caption).toEqual(["报告评分表"]);
    // 行级保真：合并体能通过出口闸门
    expect(
      checkTableBodies([a, b], [r.items[0]!.item]).ok,
    ).toBe(true);
  });

  test("colspan 行（如「文件状态」整行跨列）原样保留", () => {
    const b: MineruItem = {
      type: "table",
      table_body: '<table><tbody><tr><td colspan="2">文件状态：受控</td></tr></tbody></table>',
      table_caption: [],
      page_idx: 1,
      bbox: [50, 80, 550, 200],
    };
    const r = merge2(A_TAIL_DROPPED, b);
    expect(r.items[0]!.item.table_body).toContain('<tr><td colspan="2">文件状态：受控</td></tr>');
  });

  test("参差行若被『修复』（发明空格子改行）→ 行级保真闸 fail（机器闸防住语义猜测）", () => {
    const a = A_TAIL_DROPPED;
    const b: MineruItem = {
      type: "table",
      table_body: "<table><tbody><tr><td>乙</td><td>2</td><td>备注B</td></tr></tbody></table>",
      table_caption: [],
      page_idx: 1,
      bbox: [50, 80, 550, 200],
    };
    // 假想某实现把 A 的行补齐成 3 列：<tr><td>甲</td><td>1</td><td></td></tr>
    const padded = {
      type: "table",
      table_body:
        "<table><tbody><tr><td>名称</td><td>数量</td></tr><tr><td>甲</td><td>1</td><td></td></tr><tr><td>乙</td><td>2</td><td>备注B</td></tr></tbody></table>",
    } as MineruItem;
    expect(checkTableBodies([a, b], [padded]).ok).toBe(false);
  });
});
