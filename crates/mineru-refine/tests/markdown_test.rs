// full.md 重渲染：规则单测 + 与真实 MinerU full.md 对拍（产物存在时）。

mod common;

use common::items_of;
use mineru_refine::markdown::render_markdown;
use mineru_refine::types::MineruItem;
use serde_json::json;
use std::path::PathBuf;

#[test]
fn renders_headings_paragraphs_furniture_tables_images_equations_lists() {
    let items = items_of(json!([
        { "type": "text", "text": "组织绩效管理规范", "text_level": 1 },
        { "type": "text", "text": "正文一段。" },
        { "type": "header", "text": "MN-ZBZ-047 版本 Ed" }, // 页面家具不进 md
        { "type": "page_number", "text": "第1页共17页" },
        { "type": "footer", "text": "内部资料" },
        {
            "type": "table",
            "table_caption": ["表1 安排"],
            "table_body": "<table><tr><td>A</td></tr></table>",
            "table_footnote": ["注：略"],
        },
        { "type": "image", "img_path": "images/a.jpg", "image_caption": ["图1：流程"], "image_footnote": [] },
        { "type": "chart", "img_path": "images/b.jpg" },
        { "type": "equation", "text": "$$\nE=mc^2\n$$" },
        { "type": "list", "list_items": ["第一项", "第二项"] },
    ]));
    let expected = [
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
    ]
    .join("\n\n")
        + "\n";
    assert_eq!(render_markdown(&items), expected);
}

#[test]
fn empty_text_and_caption_skipped_unknown_type_best_effort() {
    let items = items_of(json!([
        { "type": "text", "text": "  " },
        { "type": "table", "table_caption": [], "table_body": "<table></table>" },
        { "type": "weird_future_type", "text": "未知类型的文本" },
    ]));
    assert_eq!(
        render_markdown(&items),
        "<table></table>\n\n未知类型的文本\n"
    );
}

// 对拍：render_markdown(MinerU 原始 content_list) ≈ MinerU 原版 full.md（忽略行尾空白与空行）。
// 该文档（MN-ZBZ-003）实测逐行一致；产物目录被 gitignore，不存在时跳过。
#[test]
fn matches_real_mineru_full_md_when_available() {
    let real = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../test_data/mineru/MN-ZBZ-003_管理评审程序");
    let content_list = real.join("content_list.json");
    if !content_list.exists() {
        eprintln!("（跳过：真实 MinerU 产物不存在）");
        return;
    }
    let items: Vec<MineruItem> =
        serde_json::from_str(&std::fs::read_to_string(&content_list).unwrap()).unwrap();
    let orig = std::fs::read_to_string(real.join("full.md")).unwrap();
    let norm = |s: &str| {
        s.lines()
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(norm(&render_markdown(&items)), norm(&orig));
}
