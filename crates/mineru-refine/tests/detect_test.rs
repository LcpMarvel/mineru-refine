// 异常探测器单测。

mod common;

use common::{bbox, golden_input, items_of, mi};
use mineru_refine::detect::{detect, droppable_ids};
use mineru_refine::id::assign_ids;
use mineru_refine::types::{MineruItem, SuspectKind, WorkItem};
use serde_json::json;
use std::collections::HashMap;

fn kinds_of(items: &[MineruItem]) -> HashMap<String, Vec<SuspectKind>> {
    let (ref_items, _) = assign_ids(items);
    let mut m: HashMap<String, Vec<SuspectKind>> = HashMap::new();
    for w in detect(&ref_items) {
        m.entry(w.item_id).or_default().push(w.kind);
    }
    m
}

fn detect_of(items: &[MineruItem]) -> Vec<WorkItem> {
    let (ref_items, _) = assign_ids(items);
    detect(&ref_items)
}

// ── detect 可处理疑点（hasOp）──

#[test]
fn golden_doc_flags_4_kinds_without_false_positives() {
    let m = kinds_of(&golden_input());
    assert!(!m.contains_key("it_0001")); // 真标题
    assert_eq!(m["it_0002"], vec![SuspectKind::PseudoHeading]);
    assert_eq!(m["it_0003"], vec![SuspectKind::CrossPageBreak]);
    assert_eq!(m["it_0005"], vec![SuspectKind::PageArtifact]);
    assert_eq!(m["it_0006"], vec![SuspectKind::ResidualMarkup]);
    assert!(!m.contains_key("it_0007")); // 有 caption 的表格
}

#[test]
fn cross_page_break_not_flagged_on_sentence_end_or_heading_start() {
    let m = kinds_of(&items_of(json!([
        { "type": "text", "text": "上一句已经说完了。", "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "新起的一段。", "page_idx": 1, "bbox": bbox(0) },
    ])));
    assert!(m.is_empty());
    let m = kinds_of(&items_of(json!([
        { "type": "text", "text": "前文未完", "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "第二章 总体要求", "page_idx": 1, "bbox": bbox(0) },
    ])));
    assert!(m.is_empty());
}

#[test]
fn giant_block_flagged_when_long_with_multiple_numbered_headings() {
    let giant = format!(
        "1.1 总则\n{}\n1.2 范围\n{}",
        "正文。".repeat(300),
        "正文。".repeat(300)
    );
    let m = kinds_of(&[mi(
        json!({ "type": "text", "text": giant, "page_idx": 0, "bbox": bbox(0) }),
    )]);
    assert_eq!(m["it_0001"], vec![SuspectKind::GiantBlock]);
}

#[test]
fn repeated_short_text_on_3_pages_is_page_artifact() {
    let header = |p: i64| {
        mi(json!({ "type": "text", "text": "XX公司内部资料", "page_idx": p, "bbox": bbox(10) }))
    };
    // 页码取 0/2/4 避免同时落入跨页断句的相邻页条件，单测只聚焦高频重复规则
    let m = kinds_of(&[header(0), header(2), header(4)]);
    assert_eq!(m["it_0001"], vec![SuspectKind::PageArtifact]);
    assert_eq!(m["it_0003"], vec![SuspectKind::PageArtifact]);
}

#[test]
fn classified_furniture_types_never_enter_worklist() {
    let wl = detect_of(&items_of(json!([
        { "type": "page_number", "text": "7", "page_idx": 0, "bbox": bbox(780) },
        { "type": "header", "text": "MN-ZBZ-003 版本K", "page_idx": 0, "bbox": bbox(10) },
        { "type": "footer", "text": "内部资料", "page_idx": 0, "bbox": bbox(800) },
    ])));
    assert!(wl.is_empty());
}

#[test]
fn leaked_page_number_in_text_enters_droppable_ids() {
    let wl = detect_of(&[mi(
        json!({ "type": "text", "text": "- 7 -", "page_idx": 0, "bbox": bbox(780) }),
    )]);
    assert_eq!(wl[0].kind, SuspectKind::PageArtifact);
    assert!(droppable_ids(&wl).contains("it_0001"));
}

// ── 跨页拆表/拆列表/空壳表（hasOp）与空 caption（仅标记）──

const T1: &str = "<table><tr><td>表头</td></tr><tr><td>甲</td></tr></table>";
const T2: &str = "<table><tr><td>乙</td></tr></table>";

#[test]
fn split_table_split_list_have_op_caption_issue_is_marker_only() {
    let wl = detect_of(&items_of(json!([
        { "type": "table", "table_body": T1, "table_caption": ["表1"], "page_idx": 0, "bbox": bbox(0) },
        { "type": "table", "table_body": T2, "table_caption": [], "page_idx": 1, "bbox": bbox(0) },
        { "type": "list", "list_items": ["a"], "page_idx": 1, "bbox": bbox(100) },
        { "type": "list", "list_items": ["b"], "page_idx": 2, "bbox": bbox(0) },
        { "type": "image", "img_path": "images/x.jpg", "page_idx": 2, "bbox": bbox(200) },
    ])));
    let by_kind = |k: SuspectKind| wl.iter().filter(|w| w.kind == k).collect::<Vec<_>>();
    assert_eq!(by_kind(SuspectKind::SplitTable).len(), 1);
    assert_eq!(by_kind(SuspectKind::SplitList).len(), 1);
    assert!(by_kind(SuspectKind::SplitTable)[0].has_op);
    assert!(by_kind(SuspectKind::SplitList)[0].has_op);
    assert_eq!(
        by_kind(SuspectKind::CaptionIssue)
            .iter()
            .map(|w| w.item_id.as_str())
            .collect::<Vec<_>>(),
        vec!["it_0002", "it_0005"]
    );
    assert!(by_kind(SuspectKind::CaptionIssue).iter().all(|w| !w.has_op));
}

#[test]
fn split_table_detected_across_page_furniture() {
    let wl = detect_of(&items_of(json!([
        { "type": "table", "table_body": T1, "table_caption": ["表1"], "page_idx": 0, "bbox": bbox(0) },
        { "type": "page_number", "text": "1", "page_idx": 0, "bbox": bbox(780) },
        { "type": "header", "text": "公司内部资料", "page_idx": 1, "bbox": bbox(10) },
        { "type": "table", "table_body": T2, "table_caption": [], "page_idx": 1, "bbox": bbox(100) },
    ])));
    let st: Vec<_> = wl
        .iter()
        .filter(|w| w.kind == SuspectKind::SplitTable)
        .collect();
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].item_id, "it_0001");
    assert!(st[0].evidence.contains("后块=it_0004"));
}

#[test]
fn chained_split_table_with_page_gap_still_flagged() {
    let wl = detect_of(&items_of(json!([
        { "type": "table", "table_body": T1, "table_caption": ["表1"], "page_idx": 24, "bbox": bbox(0) },
        { "type": "header", "text": "附件3：战略管理之“看市场和客户”", "page_idx": 24, "bbox": bbox(700) },
        { "type": "page_number", "text": "25 /71", "page_idx": 24, "bbox": bbox(780) },
        { "type": "page_number", "text": "26 / 71", "page_idx": 25, "bbox": bbox(780) },
        { "type": "table", "table_body": T2, "table_caption": [], "page_idx": 26, "bbox": bbox(100) },
    ])));
    let st: Vec<_> = wl
        .iter()
        .filter(|w| w.kind == SuspectKind::SplitTable)
        .collect();
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].item_id, "it_0001");
    assert!(st[0].evidence.contains("后块=it_0005"));
    assert!(st[0].evidence.contains("中间隔 1 页"));
}

#[test]
fn empty_table_husk_flagged_and_droppable_not_split_table() {
    let wl = detect_of(&items_of(json!([
        { "type": "table", "table_body": T1, "table_caption": ["表1"], "page_idx": 0, "bbox": bbox(0) },
        // 真实形态：MinerU 跨页合并后留下的占位
        { "type": "table", "img_path": "", "table_caption": [], "table_footnote": [], "page_idx": 1, "bbox": bbox(0) },
        { "type": "table", "img_path": "", "table_caption": [], "table_footnote": [], "page_idx": 2, "bbox": bbox(0) }, // 空壳链
    ])));
    let husks: Vec<_> = wl
        .iter()
        .filter(|w| w.kind == SuspectKind::EmptyTable)
        .collect();
    assert_eq!(
        husks.iter().map(|w| w.item_id.as_str()).collect::<Vec<_>>(),
        vec!["it_0002", "it_0003"]
    );
    assert!(husks.iter().all(|w| w.has_op));
    assert_eq!(
        wl.iter()
            .filter(|w| w.kind == SuspectKind::SplitTable)
            .count(),
        0
    );
    let dr = droppable_ids(&wl);
    assert!(dr.contains("it_0002"));
    assert!(dr.contains("it_0003"));
}

#[test]
fn table_with_caption_but_no_rows_is_not_a_husk() {
    let wl = detect_of(&[mi(
        json!({ "type": "table", "table_caption": ["仅有标题的表"], "page_idx": 0, "bbox": bbox(0) }),
    )]);
    assert_eq!(
        wl.iter()
            .filter(|w| w.kind == SuspectKind::EmptyTable)
            .count(),
        0
    );
}

#[test]
fn same_page_tables_or_content_in_between_not_split_table() {
    let wl = detect_of(&items_of(json!([
        { "type": "table", "table_body": T1, "table_caption": ["表1"], "page_idx": 0, "bbox": bbox(0) },
        { "type": "table", "table_body": T2, "table_caption": ["表2"], "page_idx": 0, "bbox": bbox(300) },
    ])));
    assert_eq!(
        wl.iter()
            .filter(|w| w.kind == SuspectKind::SplitTable)
            .count(),
        0
    );
    let wl2 = detect_of(&items_of(json!([
        { "type": "table", "table_body": T1, "table_caption": ["表1"], "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "中间隔着一段正文说明。", "page_idx": 0, "bbox": bbox(300) },
        { "type": "table", "table_body": T2, "table_caption": ["表2"], "page_idx": 1, "bbox": bbox(0) },
    ])));
    assert_eq!(
        wl2.iter()
            .filter(|w| w.kind == SuspectKind::SplitTable)
            .count(),
        0
    );
}

// ── 家具同文泄漏（真实数据回归）──

#[test]
fn text_matching_2_classified_headers_is_page_artifact() {
    let m = kinds_of(&items_of(json!([
        { "type": "header", "text": "附件5战略管理之“看自己”", "page_idx": 0, "bbox": bbox(10) },
        { "type": "header", "text": "附件5战略管理之“看自己”", "page_idx": 1, "bbox": bbox(10) },
        { "type": "text", "text": "附件5战略管理之“看自己”", "page_idx": 2, "bbox": bbox(10) },
    ])));
    assert_eq!(m["it_0003"], vec![SuspectKind::PageArtifact]);
    assert!(!m.contains_key("it_0001")); // 已分类家具自身不进 worklist
}

#[test]
fn company_name_plus_version_leak_also_hits() {
    let m = kinds_of(&items_of(json!([
        { "type": "header", "text": "真诺测量仪表（上海）有限公司", "page_idx": 0, "bbox": bbox(10) },
        { "type": "header", "text": "真诺测量仪表（上海）有限公司", "page_idx": 1, "bbox": bbox(10) },
        { "type": "header", "text": "MN-ZBZ-003 版本 K-", "page_idx": 0, "bbox": bbox(30) },
        { "type": "header", "text": "MN-ZBZ-003 版本K-", "page_idx": 1, "bbox": bbox(30) },
        { "type": "text", "text": "真诺测量仪表（上海）有限公司 MN-ZBZ-003 版本 K-", "page_idx": 2, "bbox": bbox(10) },
    ])));
    assert_eq!(m["it_0005"], vec![SuspectKind::PageArtifact]);
}

#[test]
fn single_corroboration_or_extra_content_does_not_trigger() {
    let m = kinds_of(&items_of(json!([
        { "type": "header", "text": "某公司", "page_idx": 0, "bbox": bbox(10) },
        { "type": "text", "text": "某公司", "page_idx": 2, "bbox": bbox(10) },
    ])));
    assert!(m.is_empty());
    let m = kinds_of(&items_of(json!([
        { "type": "header", "text": "某公司", "page_idx": 0, "bbox": bbox(10) },
        { "type": "header", "text": "某公司", "page_idx": 1, "bbox": bbox(10) },
        { "type": "text", "text": "某公司是行业领先的供应商。", "page_idx": 3, "bbox": bbox(100) },
    ])));
    assert!(!m.contains_key("it_0003"));
}

// ── residual_markup 扩展（真实数据回归）──

#[test]
fn bare_latex_command_residue_without_dollar_detected() {
    let m = kinds_of(&[mi(json!({
        "type": "text",
        "text": "每一个元素均大于零，且 \\mathsf { A i j } ^ { * } \\mathsf { A j i } { = } 1 。",
        "page_idx": 0, "bbox": bbox(0),
    }))]);
    assert_eq!(m["it_0001"], vec![SuspectKind::ResidualMarkup]);
}

#[test]
fn escaped_dollar_detected() {
    let m = kinds_of(&[mi(json!({
        "type": "text",
        "text": "通过\\$APPEALS客户需求分析工具，从8个方面了解客户需求。",
        "page_idx": 0, "bbox": bbox(0),
    }))]);
    assert_eq!(m["it_0001"], vec![SuspectKind::ResidualMarkup]);
}

// ── 跨页断句不误伤列表（真实数据回归）──

#[test]
fn adjacent_bullet_lines_across_pages_not_cross_page_break() {
    let m = kinds_of(&items_of(json!([
        { "type": "text", "text": "-顾客期待同我们保持何种关系", "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "-目前我们建立了哪些类型的关系", "page_idx": 1, "bbox": bbox(0) },
    ])));
    assert!(m.is_empty());
    let m = kinds_of(&items_of(json!([
        { "type": "text", "text": "示例如下：", "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "①行业规模和发展趋势图+定性总结", "page_idx": 1, "bbox": bbox(0) },
    ])));
    assert!(m.is_empty());
}
