// 保真不变式单测。

import { describe, expect, test } from "bun:test";
import { assignIds } from "../src/id.ts";
import { checkCharSubset, checkFidelity, checkGeometry, checkTableBodies, contentChars, inputPages } from "../src/invariant.ts";
import type { MineruItem } from "../src/types.ts";

describe("contentChars", () => {
  test("统计 text + list_items + table_caption 的非空白字符，忽略 table_body/img_path", () => {
    const items: MineruItem[] = [
      { type: "text", text: "甲 乙\n甲" },
      { type: "list", list_items: ["丙", "丁 丁"] },
      { type: "table", table_body: "<table>不计入</table>", table_caption: ["表1"] },
      { type: "image", img_path: "images/x.jpg" },
    ];
    const c = contentChars(items);
    expect(c.get("甲")).toBe(2);
    expect(c.get("乙")).toBe(1);
    expect(c.get("丁")).toBe(2);
    expect(c.get("表")).toBe(1);
    expect(c.get("不")).toBeUndefined(); // table_body 不计
    expect(c.get(" ")).toBeUndefined(); // 空白不计
  });
});

describe("checkCharSubset (C_out ⊆ C_in)", () => {
  const cin: MineruItem[] = [{ type: "text", text: "天地玄黄，宇宙洪荒。" }];

  test("削减合法", () => {
    expect(checkCharSubset(cin, [{ type: "text", text: "天地玄黄。" }]).ok).toBe(true);
  });
  test("重组合法（重排不增字）", () => {
    expect(checkCharSubset(cin, [{ type: "text", text: "宇宙洪荒，天地玄黄。" }]).ok).toBe(true);
  });
  test("新增字符违规", () => {
    const r = checkCharSubset(cin, [{ type: "text", text: "天地玄黄，宇宙洪荒，日月盈昃。" }]);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toContain("C_out ⊄ C_in");
  });
  test("同字符超量违规（多重集语义）", () => {
    expect(checkCharSubset(cin, [{ type: "text", text: "天天" }]).ok).toBe(false);
  });
  test("空白字符增减均不违规", () => {
    expect(checkCharSubset(cin, [{ type: "text", text: "天 地\n玄\t黄，宇宙洪荒。" }]).ok).toBe(true);
  });
});

describe("checkTableBodies", () => {
  const tin: MineruItem[] = [
    { type: "table", table_body: "<table>A</table>" },
    { type: "table", table_body: "<table>B</table>" },
  ];
  test("逐字节相等通过；drop 一个表也通过（多重集包含）", () => {
    expect(checkTableBodies(tin, tin).ok).toBe(true);
    expect(checkTableBodies(tin, [tin[0]!]).ok).toBe(true);
  });
  test("篡改一个字节即违规", () => {
    const r = checkTableBodies(tin, [{ type: "table", table_body: "<table>a</table>" }]);
    expect(r.ok).toBe(false);
  });
});

describe("checkGeometry", () => {
  test("bbox 非法 / page_idx 超出输入页集合 → fail", () => {
    const good = assignIds([{ type: "text", text: "x", page_idx: 0, bbox: [0, 0, 1, 1] }]).ref;
    expect(checkGeometry(good, new Set([0])).ok).toBe(true);
    expect(checkGeometry(good, new Set([5])).ok).toBe(false);
    const bad = assignIds([{ type: "text", text: "x", page_idx: 0, bbox: [0, 0, 1] }]).ref;
    expect(checkGeometry(bad, new Set([0])).ok).toBe(false);
  });
});

describe("checkFidelity 组合", () => {
  test("输入缺 bbox 时跳过几何检查（部分 MinerU 版本无 bbox）", () => {
    const { ref: before } = assignIds([{ type: "text", text: "甲乙丙" }]);
    const { ref: after } = assignIds([{ type: "text", text: "甲乙" }]);
    expect(checkFidelity(before, after).ok).toBe(true);
  });
  test("输入带几何时强检", () => {
    const { ref: before } = assignIds([{ type: "text", text: "甲乙丙", page_idx: 0, bbox: [0, 0, 1, 1] }]);
    const { ref: afterBad } = assignIds([{ type: "text", text: "甲乙", page_idx: 9 }]);
    expect(checkFidelity(before, afterBad).ok).toBe(false);
  });
  test("inputPages 收集页集合", () => {
    expect([...inputPages([{ type: "text", page_idx: 0 }, { type: "text", page_idx: 2 }])]).toEqual([0, 2]);
  });
});

describe("checkTableBodies 行级路径（mergeTable 产物）", () => {
  const bodyA = "<table><tbody>\n<tr><td>表头</td></tr>\n<tr><td>甲</td></tr>\n</tbody></table>";
  const bodyB = "<table><tbody><tr><td>乙</td></tr></tbody></table>";
  const tin = [
    { type: "table", table_body: bodyA } as MineruItem,
    { type: "table", table_body: bodyB } as MineruItem,
  ];

  test("合法合并（A 外壳 + A 行 ++ B 行）→ pass；去掉重复行（⊆）也 pass", () => {
    const merged = "<table><tbody>\n<tr><td>表头</td></tr>\n<tr><td>甲</td></tr><tr><td>乙</td></tr>\n</tbody></table>";
    expect(checkTableBodies(tin, [{ type: "table", table_body: merged } as MineruItem]).ok).toBe(true);
    // 少一行（如重复表头被去）仍是子集 → pass
    const fewer = "<table><tbody>\n<tr><td>表头</td></tr>\n<tr><td>乙</td></tr>\n</tbody></table>";
    expect(checkTableBodies(tin, [{ type: "table", table_body: fewer } as MineruItem]).ok).toBe(true);
  });

  test("行内字节被篡改 → fail；行外（外壳）被篡改 → fail；行重复消费 → fail", () => {
    const tamperedRow = "<table><tbody>\n<tr><td>表头！</td></tr>\n<tr><td>甲</td></tr><tr><td>乙</td></tr>\n</tbody></table>";
    expect(checkTableBodies(tin, [{ type: "table", table_body: tamperedRow } as MineruItem]).ok).toBe(false);
    const tamperedShell = "<table class=x><tbody>\n<tr><td>表头</td></tr>\n<tr><td>甲</td></tr><tr><td>乙</td></tr>\n</tbody></table>";
    expect(checkTableBodies(tin, [{ type: "table", table_body: tamperedShell } as MineruItem]).ok).toBe(false);
    // 同一输入行被两个输出表消费 → 第二次 fail
    const dupUse = [
      { type: "table", table_body: bodyB } as MineruItem,
      { type: "table", table_body: "<table><tbody><tr><td>乙</td></tr><tr><td>甲</td></tr></tbody></table>" } as MineruItem,
    ];
    expect(checkTableBodies(tin, dupUse).ok).toBe(false);
  });
});
