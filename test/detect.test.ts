// 异常探测器单测。

import { describe, expect, test } from "bun:test";
import { detect, droppableIds } from "../src/detect.ts";
import { assignIds } from "../src/id.ts";
import type { MineruItem, SuspectKind } from "../src/types.ts";
import { bbox, goldenInput } from "./helpers.ts";

function kindsOf(items: MineruItem[]): Map<string, SuspectKind[]> {
  const { ref } = assignIds(items);
  const m = new Map<string, SuspectKind[]>();
  for (const w of detect(ref)) m.set(w.itemId, [...(m.get(w.itemId) ?? []), w.kind]);
  return m;
}

describe("detect 可处理疑点（hasOp）", () => {
  test("golden 文档标出 4 类疑点，真标题/干净表格不误标", () => {
    const m = kindsOf(goldenInput());
    expect(m.get("it_0001")).toBeUndefined(); // 真标题
    expect(m.get("it_0002")).toEqual(["pseudo_heading"]);
    expect(m.get("it_0003")).toEqual(["cross_page_break"]);
    expect(m.get("it_0005")).toEqual(["page_artifact"]);
    expect(m.get("it_0006")).toEqual(["residual_markup"]);
    expect(m.get("it_0007")).toBeUndefined(); // 有 caption 的表格
  });

  test("跨页断句：前块句末标点收尾 / 后块标题特征开头 → 不标", () => {
    const base = { type: "text" as const, bbox: bbox(0) };
    expect(
      kindsOf([
        { ...base, text: "上一句已经说完了。", page_idx: 0 },
        { ...base, text: "新起的一段。", page_idx: 1 },
      ]).size,
    ).toBe(0);
    expect(
      kindsOf([
        { ...base, text: "前文未完", page_idx: 0 },
        { ...base, text: "第二章 总体要求", page_idx: 1 },
      ]).size,
    ).toBe(0);
  });

  test("巨型块：超长且含多个小标题编号", () => {
    const giant =
      "1.1 总则\n" + "正文。".repeat(300) + "\n1.2 范围\n" + "正文。".repeat(300);
    const m = kindsOf([{ type: "text", text: giant, page_idx: 0, bbox: bbox(0) }]);
    expect(m.get("it_0001")).toEqual(["giant_block"]);
  });

  test("高频重复短文本（≥3 页同文）→ page_artifact", () => {
    const header = (p: number): MineruItem => ({ type: "text", text: "XX公司内部资料", page_idx: p, bbox: bbox(10) });
    // 页码取 0/2/4 避免同时落入跨页断句的相邻页条件，单测只聚焦高频重复规则
    const m = kindsOf([header(0), header(2), header(4)]);
    expect(m.get("it_0001")).toEqual(["page_artifact"]);
    expect(m.get("it_0003")).toEqual(["page_artifact"]);
  });

  test("MinerU 已正确分类的 page_number/header/footer 不进 worklist（非 quirk）", () => {
    const { ref } = assignIds([
      { type: "page_number", text: "7", page_idx: 0, bbox: bbox(780) },
      { type: "header", text: "MN-ZBZ-003 版本K", page_idx: 0, bbox: bbox(10) },
      { type: "footer", text: "内部资料", page_idx: 0, bbox: bbox(800) },
    ]);
    expect(detect(ref)).toHaveLength(0);
  });

  test("混入正文（type=text）的页码进 droppableIds", () => {
    const { ref } = assignIds([{ type: "text", text: "- 7 -", page_idx: 0, bbox: bbox(780) }]);
    const wl = detect(ref);
    expect(wl[0]!.kind).toBe("page_artifact");
    expect(droppableIds(wl).has("it_0001")).toBe(true);
  });
});

describe("跨页拆表/拆列表/空壳表（hasOp）与空 caption（仅标记）", () => {
  const T1 = "<table><tr><td>表头</td></tr><tr><td>甲</td></tr></table>";
  const T2 = "<table><tr><td>乙</td></tr></table>";

  test("跨页两有体 table / 两 list → split_table/split_list（hasOp），空 caption 仅标记", () => {
    const items: MineruItem[] = [
      { type: "table", table_body: T1, table_caption: ["表1"], page_idx: 0, bbox: bbox(0) },
      { type: "table", table_body: T2, table_caption: [], page_idx: 1, bbox: bbox(0) },
      { type: "list", list_items: ["a"], page_idx: 1, bbox: bbox(100) },
      { type: "list", list_items: ["b"], page_idx: 2, bbox: bbox(0) },
      { type: "image", img_path: "images/x.jpg", page_idx: 2, bbox: bbox(200) },
    ];
    const { ref } = assignIds(items);
    const wl = detect(ref);
    const byKind = (k: SuspectKind) => wl.filter((w) => w.kind === k);
    expect(byKind("split_table")).toHaveLength(1);
    expect(byKind("split_list")).toHaveLength(1);
    expect(byKind("split_table")[0]!.hasOp).toBe(true);
    expect(byKind("split_list")[0]!.hasOp).toBe(true);
    expect(byKind("caption_issue").map((w) => w.itemId)).toEqual(["it_0002", "it_0005"]);
    expect(byKind("caption_issue").every((w) => !w.hasOp)).toBe(true);
  });

  test("跨页表对隔着页面家具仍能探测（真实数据：中间总有 2-3 个页眉/页码）", () => {
    const { ref } = assignIds([
      { type: "table", table_body: T1, table_caption: ["表1"], page_idx: 0, bbox: bbox(0) },
      { type: "page_number", text: "1", page_idx: 0, bbox: bbox(780) },
      { type: "header", text: "公司内部资料", page_idx: 1, bbox: bbox(10) },
      { type: "table", table_body: T2, table_caption: [], page_idx: 1, bbox: bbox(100) },
    ]);
    const st = detect(ref).filter((w) => w.kind === "split_table");
    expect(st).toHaveLength(1);
    expect(st[0]!.itemId).toBe("it_0001");
    expect(st[0]!.evidence).toContain("后块=it_0004");
  });

  test("链式拆表：页码隔 ≥2 但中间仅家具仍标 split_table（真实数据：MN-JZY-001 三页拆表合并一段后产物在 p24、残片在 p26）", () => {
    const { ref } = assignIds([
      { type: "table", table_body: T1, table_caption: ["表1"], page_idx: 24, bbox: bbox(0) },
      { type: "header", text: "附件3：战略管理之“看市场和客户”", page_idx: 24, bbox: bbox(700) },
      { type: "page_number", text: "25 /71", page_idx: 24, bbox: bbox(780) },
      { type: "page_number", text: "26 / 71", page_idx: 25, bbox: bbox(780) },
      { type: "table", table_body: T2, table_caption: [], page_idx: 26, bbox: bbox(100) },
    ]);
    const st = detect(ref).filter((w) => w.kind === "split_table");
    expect(st).toHaveLength(1);
    expect(st[0]!.itemId).toBe("it_0001");
    expect(st[0]!.evidence).toContain("后块=it_0005");
    expect(st[0]!.evidence).toContain("中间隔 1 页");
  });

  test("零内容空壳表 → empty_table（hasOp）且进 droppableIds；不参与 split_table", () => {
    const { ref } = assignIds([
      { type: "table", table_body: T1, table_caption: ["表1"], page_idx: 0, bbox: bbox(0) },
      // 真实形态：MinerU 跨页合并后留下的占位
      { type: "table", img_path: "", table_caption: [], table_footnote: [], page_idx: 1, bbox: bbox(0) },
      { type: "table", img_path: "", table_caption: [], table_footnote: [], page_idx: 2, bbox: bbox(0) }, // 空壳链
    ]);
    const wl = detect(ref);
    const husks = wl.filter((w) => w.kind === "empty_table");
    expect(husks.map((w) => w.itemId)).toEqual(["it_0002", "it_0003"]);
    expect(husks.every((w) => w.hasOp)).toBe(true);
    expect(wl.filter((w) => w.kind === "split_table")).toHaveLength(0);
    expect(droppableIds(wl).has("it_0002")).toBe(true);
    expect(droppableIds(wl).has("it_0003")).toBe(true);
  });

  test("有 caption 但无行的表不算空壳（字符不可丢）", () => {
    const { ref } = assignIds([
      { type: "table", table_caption: ["仅有标题的表"], page_idx: 0, bbox: bbox(0) },
    ]);
    expect(detect(ref).filter((w) => w.kind === "empty_table")).toHaveLength(0);
  });

  test("同页两表 / 隔内容块的跨页两表不标 split_table", () => {
    const { ref } = assignIds([
      { type: "table", table_body: T1, table_caption: ["表1"], page_idx: 0, bbox: bbox(0) },
      { type: "table", table_body: T2, table_caption: ["表2"], page_idx: 0, bbox: bbox(300) },
    ]);
    expect(detect(ref).filter((w) => w.kind === "split_table")).toHaveLength(0);
    const { ref: ref2 } = assignIds([
      { type: "table", table_body: T1, table_caption: ["表1"], page_idx: 0, bbox: bbox(0) },
      { type: "text", text: "中间隔着一段正文说明。", page_idx: 0, bbox: bbox(300) },
      { type: "table", table_body: T2, table_caption: ["表2"], page_idx: 1, bbox: bbox(0) },
    ]);
    expect(detect(ref2).filter((w) => w.kind === "split_table")).toHaveLength(0);
  });
});

describe("家具同文泄漏（真实数据回归）", () => {
  test("text 与 ≥2 处已分类 header 同文 → page_artifact，不受 3 页阈值限制", () => {
    const m = kindsOf([
      { type: "header", text: "附件5战略管理之“看自己”", page_idx: 0, bbox: bbox(10) },
      { type: "header", text: "附件5战略管理之“看自己”", page_idx: 1, bbox: bbox(10) },
      { type: "text", text: "附件5战略管理之“看自己”", page_idx: 2, bbox: bbox(10) },
    ]);
    expect(m.get("it_0003")).toEqual(["page_artifact"]);
    expect(m.get("it_0001")).toBeUndefined(); // 已分类家具自身不进 worklist
  });

  test("公司名+版本号拼成一块的泄漏形态也命中", () => {
    const m = kindsOf([
      { type: "header", text: "真诺测量仪表（上海）有限公司", page_idx: 0, bbox: bbox(10) },
      { type: "header", text: "真诺测量仪表（上海）有限公司", page_idx: 1, bbox: bbox(10) },
      { type: "header", text: "MN-ZBZ-003 版本 K-", page_idx: 0, bbox: bbox(30) },
      { type: "header", text: "MN-ZBZ-003 版本K-", page_idx: 1, bbox: bbox(30) },
      { type: "text", text: "真诺测量仪表（上海）有限公司 MN-ZBZ-003 版本 K-", page_idx: 2, bbox: bbox(10) },
    ]);
    expect(m.get("it_0005")).toEqual(["page_artifact"]);
  });

  test("仅 1 处家具佐证 / 文本含家具以外内容 → 不触发", () => {
    expect(
      kindsOf([
        { type: "header", text: "某公司", page_idx: 0, bbox: bbox(10) },
        { type: "text", text: "某公司", page_idx: 2, bbox: bbox(10) },
      ]).size,
    ).toBe(0);
    const m = kindsOf([
      { type: "header", text: "某公司", page_idx: 0, bbox: bbox(10) },
      { type: "header", text: "某公司", page_idx: 1, bbox: bbox(10) },
      { type: "text", text: "某公司是行业领先的供应商。", page_idx: 3, bbox: bbox(100) },
    ]);
    expect(m.get("it_0003")).toBeUndefined();
  });
});

describe("residual_markup 扩展（真实数据回归）", () => {
  test("去掉 $ 定界符后的裸 LaTeX 命令残骸（\\mathsf { … }）仍被探测", () => {
    const m = kindsOf([
      { type: "text", text: "每一个元素均大于零，且 \\mathsf { A i j } ^ { * } \\mathsf { A j i } { = } 1 。", page_idx: 0, bbox: bbox(0) },
    ]);
    expect(m.get("it_0001")).toEqual(["residual_markup"]);
  });

  test("孤立 \\$ 转义（\\$APPEALS）被探测", () => {
    const m = kindsOf([
      { type: "text", text: "通过\\$APPEALS客户需求分析工具，从8个方面了解客户需求。", page_idx: 0, bbox: bbox(0) },
    ]);
    expect(m.get("it_0001")).toEqual(["residual_markup"]);
  });
});

describe("跨页断句不误伤列表（真实数据回归）", () => {
  test("相邻列表项（-/①开头）跨页不标 cross_page_break", () => {
    const base = { type: "text" as const, bbox: bbox(0) };
    expect(
      kindsOf([
        { ...base, text: "-顾客期待同我们保持何种关系", page_idx: 0 },
        { ...base, text: "-目前我们建立了哪些类型的关系", page_idx: 1 },
      ]).size,
    ).toBe(0);
    expect(
      kindsOf([
        { ...base, text: "示例如下：", page_idx: 0 },
        { ...base, text: "①行业规模和发展趋势图+定性总结", page_idx: 1 },
      ]).size,
    ).toBe(0);
  });
});
