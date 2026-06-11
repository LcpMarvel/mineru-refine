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

use crate::types::{MineruItem, is_page_furniture};

fn push_lines(out: &mut Vec<String>, parts: &[Option<&str>]) {
    for p in parts.iter().flatten() {
        let t = p.trim();
        if !t.is_empty() {
            out.push(t.to_string());
        }
    }
}

fn push_array(out: &mut Vec<String>, item: &MineruItem, key: &str) {
    if let Some(arr) = item.str_array(key) {
        for s in arr {
            let t = s.trim();
            if !t.is_empty() {
                out.push(t.to_string());
            }
        }
    }
}

fn render_item(item: &MineruItem) -> Vec<String> {
    if is_page_furniture(item.item_type()) {
        return vec![];
    }
    let mut out: Vec<String> = Vec::new();
    match item.item_type() {
        "text" => {
            let text = item.text().unwrap_or("").trim();
            if text.is_empty() {
                return vec![];
            }
            match item.text_level() {
                Some(level) if level >= 1 => {
                    out.push(format!("{} {}", "#".repeat(level.min(6) as usize), text));
                }
                _ => out.push(text.to_string()),
            }
        }
        "table" => {
            push_array(&mut out, item, "table_caption");
            push_lines(&mut out, &[item.table_body()]);
            push_array(&mut out, item, "table_footnote");
        }
        "image" | "chart" => {
            if let Some(img) = item.img_path()
                && !img.is_empty()
            {
                out.push(format!("![]({img})"));
            }
            push_array(&mut out, item, "img_caption");
            push_array(&mut out, item, "img_footnote");
        }
        "equation" => push_lines(&mut out, &[item.text()]),
        "list" => push_array(&mut out, item, "list_items"),
        _ => {
            // 未知类型：尽力而为——有文本出文本，有图出图，否则跳过（不抛：渲染是出口侧附属品）
            push_lines(&mut out, &[item.text()]);
            if let Some(img) = item.img_path()
                && !img.is_empty()
            {
                out.push(format!("![]({img})"));
            }
        }
    }
    out
}

/// items → full.md 文本。每个 item 一个块，块间空行。
pub fn render_markdown(items: &[MineruItem]) -> String {
    let mut blocks: Vec<String> = Vec::new();
    for item in items {
        let ls = render_item(item);
        if !ls.is_empty() {
            blocks.push(ls.join("\n\n"));
        }
    }
    format!("{}\n", blocks.join("\n\n"))
}
