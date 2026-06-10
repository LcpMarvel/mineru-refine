// 从 content_list items 确定性重渲染 full.md（与 MinerU pipeline 的 markdown 拼接规则对齐，
// 经真实产物对拍验证）。纯拼接、零生成——不违反纯削减（不加字）原则。
//
// MinerU 规则（从真实 full.md 反推并对拍）：
// - text + text_level=n  → "#"×n + 空格 + 文本
// - text                 → 段落
// - header/footer/page_number → 不进 full.md（页面家具）
// - table                → caption 行 + 裸 HTML table_body + footnote 行
// - image/chart          → ![](img_path) + caption 行 + footnote 行
// - equation             → text 原样（自带 $$...$$ 块）
// - list                 → list_items 逐行
// - 块间以空行分隔

import type { MineruItem } from "./types.ts";

const SKIP_TYPES = new Set(["page_number", "header", "footer"]);

function lines(...parts: (string | string[] | undefined)[]): string[] {
  const out: string[] = [];
  for (const p of parts) {
    if (p === undefined) continue;
    for (const s of Array.isArray(p) ? p : [p]) {
      const t = s.trim();
      if (t) out.push(t);
    }
  }
  return out;
}

function renderItem(item: MineruItem): string[] {
  if (SKIP_TYPES.has(item.type)) return [];
  switch (item.type) {
    case "text": {
      const text = (item.text ?? "").trim();
      if (!text) return [];
      const level = item.text_level;
      if (typeof level === "number" && level >= 1) {
        return [`${"#".repeat(Math.min(level, 6))} ${text}`];
      }
      return [text];
    }
    case "table":
      return lines(
        item.table_caption,
        item.table_body,
        (item as { table_footnote?: string[] }).table_footnote,
      );
    case "image":
    case "chart": {
      const img = item.img_path ? [`![](${item.img_path})`] : [];
      return lines(
        img,
        (item as { img_caption?: string[] }).img_caption,
        (item as { img_footnote?: string[] }).img_footnote,
      );
    }
    case "equation":
      return lines(item.text);
    case "list":
      return lines(item.list_items);
    default:
      // 未知类型：尽力而为——有文本出文本，有图出图，否则跳过（不抛：渲染是出口侧附属品）
      return lines(item.text, item.img_path ? `![](${item.img_path})` : undefined);
  }
}

/** items → full.md 文本。每个 item 一个块，块间空行。 */
export function renderMarkdown(items: readonly MineruItem[]): string {
  const blocks: string[] = [];
  for (const item of items) {
    const ls = renderItem(item);
    if (ls.length > 0) blocks.push(ls.join("\n\n"));
  }
  return blocks.join("\n\n") + "\n";
}
