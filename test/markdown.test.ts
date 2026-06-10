// full.md 重渲染（src/markdown.ts）：规则单测 + 与真实 MinerU full.md 对拍（产物存在时）。

import { describe, expect, test } from "bun:test";
import { join } from "node:path";
import { renderMarkdown } from "../src/markdown.ts";
import type { MineruItem } from "../src/types.ts";

describe("renderMarkdown 规则", () => {
  test("标题/段落/页面家具/表格/图片/公式/列表", () => {
    const items: MineruItem[] = [
      { type: "text", text: "组织绩效管理规范", text_level: 1 },
      { type: "text", text: "正文一段。" },
      { type: "header", text: "MN-ZBZ-047 版本 Ed" }, // 页面家具不进 md
      { type: "page_number", text: "第1页共17页" },
      { type: "footer", text: "内部资料" },
      {
        type: "table",
        table_caption: ["表1 安排"],
        table_body: "<table><tr><td>A</td></tr></table>",
        table_footnote: ["注：略"],
      } as MineruItem,
      { type: "image", img_path: "images/a.jpg", img_caption: ["图1：流程"], img_footnote: [] } as MineruItem,
      { type: "chart", img_path: "images/b.jpg" } as MineruItem,
      { type: "equation", text: "$$\nE=mc^2\n$$" },
      { type: "list", list_items: ["第一项", "第二项"] } as MineruItem,
    ];
    expect(renderMarkdown(items)).toBe(
      [
        "# 组织绩效管理规范",
        "正文一段。",
        "表1 安排",
        "<table><tr><td>A</td></tr></table>",
        "注：略",
        "![](images/a.jpg)",
        "图1：流程",
        "![](images/b.jpg)",
        "$$\nE=mc^2\n$$",
        "第一项",
        "第二项",
      ].join("\n\n") + "\n",
    );
  });

  test("空文本/空 caption 不产生空块；未知类型尽力而为", () => {
    const items: MineruItem[] = [
      { type: "text", text: "  " },
      { type: "table", table_caption: [], table_body: "<table></table>" } as MineruItem,
      { type: "weird_future_type", text: "未知类型的文本" } as MineruItem,
    ];
    expect(renderMarkdown(items)).toBe("<table></table>\n\n未知类型的文本\n");
  });
});

// 对拍：renderMarkdown(MinerU 原始 content_list) ≈ MinerU 原版 full.md（忽略行尾空白与空行）。
// 该文档（MN-ZBZ-003）实测逐行一致；产物目录被 gitignore，不存在时跳过。
// 注意 decodeURIComponent：URL.pathname 会把中文目录名百分号编码
const REAL = decodeURIComponent(new URL("../test_data/mineru/MN-ZBZ-003_管理评审程序/", import.meta.url).pathname);
const hasReal = await Bun.file(join(REAL, "content_list.json")).exists();

describe.skipIf(!hasReal)("与真实 MinerU full.md 对拍", () => {
  test("MN-ZBZ-003: 渲染原始 content_list 与原版 full.md 逐行一致", async () => {
    const items = (await Bun.file(join(REAL, "content_list.json")).json()) as MineruItem[];
    const orig = await Bun.file(join(REAL, "full.md")).text();
    const norm = (s: string) =>
      s.split("\n").map((l) => l.replace(/\s+$/, "")).filter((l) => l !== "").join("\n");
    expect(norm(renderMarkdown(items))).toBe(norm(orig));
  });
});
