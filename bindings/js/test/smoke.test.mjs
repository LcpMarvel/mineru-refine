// 绑定层冒烟（不打网络）：原生模块加载、detect/render 正确、refine 形状正确。
// 核心逻辑的全量测试在 Rust 侧（cargo test）。

import { test } from "node:test";
import assert from "node:assert/strict";
import { detectSuspects, renderMarkdown, REFINE_LOGIC_VERSION, refine } from "../index.js";

const items = [
  { type: "text", text: "第一章 总则", text_level: 1, page_idx: 0, bbox: [50, 40, 550, 60] },
  { type: "text", text: "- 3 -", page_idx: 1, bbox: [50, 780, 550, 800] },
];

test("常量与导出", () => {
  assert.match(REFINE_LOGIC_VERSION, /^\d+\.\d+\.\d+$/);
  assert.equal(typeof refine, "function");
});

test("detectSuspects：混入正文的页码被标记", () => {
  const sus = detectSuspects(items);
  assert.equal(sus.length, 1);
  assert.equal(sus[0].kind, "page_artifact");
  assert.equal(sus[0].itemId, "it_0002");
  assert.equal(sus[0].hasOp, true);
});

test("renderMarkdown：标题与段落", () => {
  assert.equal(renderMarkdown(items), "# 第一章 总则\n\n- 3 -\n");
});

test("refine：非法输入直接抛（不是静默吞）", async () => {
  await assert.rejects(() => refine({ not: "an array" }), /content_list/);
});
