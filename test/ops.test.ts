// M3：7 个 op 的纯函数语义 + 固定 op 序列 replay + 保真闸回滚（不接 LLM）。

import { describe, expect, test } from "bun:test";
import { assignIds } from "../src/id.ts";
import { inputPages } from "../src/invariant.ts";
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

describe("M3 固定 op 序列 replay（不接 LLM）", () => {
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
