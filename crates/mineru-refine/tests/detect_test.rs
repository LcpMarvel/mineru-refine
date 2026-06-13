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

#[test]
fn bullet_list_tail_before_next_page_heading_not_cross_page_break() {
    // 真实数据回归（JZY-001 p38→p39，PDF 复核）：「---收割战」是五连项目符列表的末项
    //（OCR 吃掉了部分短横），下一页「防御战的核心」是图示小标题——绝不是断句，merge 了就错。
    let m = kinds_of(&items_of(json!([
        { "type": "text", "text": "侧翼战", "page_idx": 38, "bbox": bbox(700) },
        { "type": "text", "text": "--收割战", "page_idx": 38, "bbox": bbox(730) },
        { "type": "text", "text": "防御战的核心", "text_level": 2, "page_idx": 39, "bbox": bbox(0) },
    ])));
    assert!(!m.contains_key("it_0002"), "{m:?}");
}

#[test]
fn colon_intro_bullet_item_before_debulleted_list_not_cross_page_break() {
    // 真实数据回归（JZY-001 p22→p23，PDF 复核）：前块是「②…：」列表项引出下一页的
    // 子列表，子列表项的「–」项目符被 MinerU 吃掉成裸文本——仍不是断句。
    let m = kinds_of(&items_of(json!([
        { "type": "text", "text": "②行业政策分析，重点检索和分析国家出台的以下行业政策：", "page_idx": 22, "bbox": bbox(700) },
        { "type": "text", "text": "对计量行业新技术发展的鼓励或引导政策", "page_idx": 23, "bbox": bbox(0) },
    ])));
    assert!(!m.contains_key("it_0001"), "{m:?}");
}

// ── 漏标标题 / 段尾节标记 / caption 被标题隔开（hasOp）──

#[test]
fn missed_heading_flagged_when_prev_numbered_sibling_is_heading() {
    let m = kinds_of(&items_of(json!([
        { "type": "text", "text": "4.5核心组织绩效的考核方法", "text_level": 2, "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "4.6核心组织绩效的应用", "page_idx": 0, "bbox": bbox(40) },
    ])));
    assert!(!m.contains_key("it_0001"));
    assert_eq!(m["it_0002"], vec![SuspectKind::MissedHeading]);
}

#[test]
fn missed_heading_flagged_via_following_sibling_and_evidence_has_level() {
    // 4.6（正文）的最近同组前块不存在、后块 4.7 也是正文 → 不标；
    // 4.7 的最近同组后块 4.8 是标题且编号 +1 → 标。
    let wl = detect_of(&items_of(json!([
        { "type": "text", "text": "4.6核心组织绩效的应用", "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "4.7公司十大核心指标", "page_idx": 0, "bbox": bbox(40) },
        { "type": "text", "text": "4.8部门核心指标", "text_level": 3, "page_idx": 0, "bbox": bbox(80) },
    ])));
    let hits: Vec<_> = wl
        .iter()
        .filter(|w| w.kind == SuspectKind::MissedHeading)
        .collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].item_id, "it_0002");
    assert!(hits[0].evidence.contains("level=3"), "{}", hits[0].evidence);
}

#[test]
fn missed_heading_flagged_via_numbered_child_when_whole_sibling_group_unmarked() {
    // 真实数据回归（JZY-001 p41）：2.1/2.2 整组兄弟都被 MinerU 漏标，
    // ±1 兄弟信号永不触发；下一个内容块的编号以本块编号为真前缀（2.1 → 2.1.1）即标。
    let wl = detect_of(&items_of(json!([
        { "type": "text", "text": "2、雷达图分析：", "text_level": 2, "page_idx": 41, "bbox": bbox(0) },
        { "type": "text", "text": "2.1确定竞争对手：", "page_idx": 41, "bbox": bbox(40) },
        { "type": "text", "text": "2.1.1在开始竞争分析之前，首先确定本公司的标杆者和竞争对手。", "page_idx": 41, "bbox": bbox(80) },
    ])));
    let hits: Vec<_> = wl
        .iter()
        .filter(|w| w.kind == SuspectKind::MissedHeading)
        .collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].item_id, "it_0002");
    assert!(hits[0].evidence.contains("it_0003"), "{}", hits[0].evidence);
    assert!(hits[0].evidence.contains("前缀"), "{}", hits[0].evidence);
}

#[test]
fn missed_heading_numbered_child_works_across_page_furniture() {
    // 真实数据回归（JZY-001 p41→p42）：「2.2竞争情报收集：」是页末块，
    // 子项 2.2.1 在下一页、中间隔着页眉页码。跨页断句探测器也不许误标（后块编号是标题特征）。
    let m = kinds_of(&items_of(json!([
        { "type": "text", "text": "2.2竞争情报收集：", "page_idx": 41, "bbox": bbox(700) },
        { "type": "header", "text": "附件5战略管理之“看自己”", "page_idx": 41, "bbox": bbox(750) },
        { "type": "page_number", "text": "42 / 71", "page_idx": 41, "bbox": bbox(780) },
        { "type": "text", "text": "2.2.1收集与竞争对手15要素分析数据，从15要素里再选取了9个要素进行分析。", "page_idx": 42, "bbox": bbox(0) },
    ])));
    assert_eq!(m["it_0001"], vec![SuspectKind::MissedHeading]);
}

#[test]
fn numbered_child_signal_requires_true_prefix_and_same_style() {
    // 非前缀编号（3.1 后跟 4.1）不算子项；中文数制父块 + 阿拉伯子编号也不算。
    let m = kinds_of(&items_of(json!([
        { "type": "text", "text": "3.1商业画布的过程和展开", "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "4.1按战略发展部要求收集数据", "page_idx": 0, "bbox": bbox(40) },
    ])));
    assert!(m.is_empty(), "{m:?}");
    let m = kinds_of(&items_of(json!([
        { "type": "text", "text": "一、总则", "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "1.1本规范适用于全公司各部门", "page_idx": 0, "bbox": bbox(40) },
    ])));
    assert!(!m.contains_key("it_0001"), "{m:?}");
}

#[test]
fn numbered_paragraph_with_comma_or_sentence_end_not_missed_heading() {
    let m = kinds_of(&items_of(json!([
        { "type": "text", "text": "3.5根据评价结果优化", "text_level": 2, "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "3.6每季度回顾，并由管代统计汇总。", "page_idx": 0, "bbox": bbox(40) },
    ])));
    assert!(!m.contains_key("it_0002"));
}

#[test]
fn year_and_percent_prefixes_are_not_numbering() {
    let m = kinds_of(&items_of(json!([
        { "type": "text", "text": "2025总体回顾", "text_level": 2, "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "2026工作计划", "page_idx": 0, "bbox": bbox(40) },
        { "type": "text", "text": "82.36%达标线", "page_idx": 0, "bbox": bbox(80) },
    ])));
    assert!(!m.contains_key("it_0002")); // 年份不是编号
    assert!(!m.contains_key("it_0003")); // 百分数不是编号
}

#[test]
fn trailing_marker_flagged_with_suggested_offset() {
    let wl = detect_of(&items_of(json!([
        { "type": "text", "text": "负责人按任务项未完成考核（-20元）[相关文件]", "page_idx": 0, "bbox": bbox(0) },
    ])));
    assert_eq!(wl[0].kind, SuspectKind::TrailingMarker);
    assert!(wl[0].evidence.contains("offset=18"), "{}", wl[0].evidence);
}

#[test]
fn standalone_marker_not_flagged() {
    let m = kinds_of(&items_of(json!([
        { "type": "text", "text": "[相关文件]", "page_idx": 0, "bbox": bbox(0) },
    ])));
    assert!(m.is_empty());
}

#[test]
fn caption_separated_from_table_by_heading_flagged() {
    let m = kinds_of(&items_of(json!([
        { "type": "text", "text": "报告评分表", "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "4.6核心组织绩效的应用", "page_idx": 0, "bbox": bbox(40) },
        { "type": "table", "table_body": "<table><tr><td>考核项目</td></tr></table>",
          "table_caption": ["评分"], "page_idx": 0, "bbox": bbox(80) },
    ])));
    // 中间块此时还是正文，但是漏标标题候选 → 同样要标（promote 之后疑点不会凭空冒出，保证异常数单调）
    assert!(m["it_0001"].contains(&SuspectKind::SeparatedCaption));
}

#[test]
fn caption_directly_before_table_not_flagged() {
    let m = kinds_of(&items_of(json!([
        { "type": "text", "text": "报告评分表", "page_idx": 0, "bbox": bbox(0) },
        { "type": "table", "table_body": "<table><tr><td>考核项目</td></tr></table>",
          "table_caption": ["评分"], "page_idx": 0, "bbox": bbox(40) },
    ])));
    assert!(!m.contains_key("it_0001"));
}

// ── extra_char：赘字/衍字疑点 ──

#[test]
fn extra_char_flags_dup_function_words_and_isolated_radicals() {
    let m = kinds_of(&items_of(json!([
        { "type": "text", "text": "基本治理理念的的变化情况。", "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "确保目的的实现。", "page_idx": 0, "bbox": bbox(40) },
        { "type": "text", "text": "3）亻", "page_idx": 0, "bbox": bbox(80) },
    ])));
    assert!(m["it_0001"].contains(&SuspectKind::ExtraChar));
    assert!(m["it_0002"].contains(&SuspectKind::ExtraChar)); // 合法语法嫌疑也报，由 LLM 裁决
    assert!(m["it_0003"].contains(&SuspectKind::ExtraChar));
}

#[test]
fn extra_char_evidence_carries_offset_for_delete_char() {
    let ws = detect_of(&items_of(json!([
        { "type": "text", "text": "基本治理理念的的变化情况。", "page_idx": 0, "bbox": bbox(0) },
    ])));
    let w = ws
        .iter()
        .find(|w| w.kind == SuspectKind::ExtraChar)
        .expect("应有 extra_char 疑点");
    assert!(w.has_op);
    assert!(w.evidence.contains("offset=7"), "{}", w.evidence);
}

#[test]
fn extra_char_not_flagged_on_legit_reduplication() {
    let m = kinds_of(&items_of(json!([
        { "type": "text", "text": "这件事的的确确发生过。", "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "他是地地道道的本地人。", "page_idx": 0, "bbox": bbox(40) },
        { "type": "text", "text": "是是非非自有公论。", "page_idx": 0, "bbox": bbox(80) },
    ])));
    assert!(m.is_empty(), "合法叠词不许报疑点: {m:?}");
}

// ── caption_heading：被吞进 table_caption 的小节标题 ──

#[test]
fn numbered_caption_with_adjacent_heading_sibling_is_flagged() {
    // 047 实测形态：4.5 是标题，「4.6核心组织绩效的应用」被吞进评分表 caption
    let m = kinds_of(&items_of(json!([
        { "type": "text", "text": "4.5核心组织绩效的考核方法", "text_level": 2, "page_idx": 0, "bbox": bbox(0) },
        {
            "type": "table",
            "table_body": "<table><tr><td>考核项目</td><td>权重</td></tr></table>",
            "table_caption": ["报告评分表", "4.6核心组织绩效的应用"],
            "page_idx": 0,
            "bbox": bbox(40),
        },
    ])));
    let flags = &m["it_0002"];
    assert!(flags.contains(&SuspectKind::CaptionHeading), "{flags:?}");
    // 证据应给出条目下标与兄弟 level（mock/LLM 都按它出参数）
    let w = detect_of(&items_of(json!([
        { "type": "text", "text": "4.5核心组织绩效的考核方法", "text_level": 2, "page_idx": 0, "bbox": bbox(0) },
        {
            "type": "table",
            "table_body": "<table><tr><td>考核项目</td><td>权重</td></tr></table>",
            "table_caption": ["报告评分表", "4.6核心组织绩效的应用"],
            "page_idx": 0,
            "bbox": bbox(40),
        },
    ])));
    let ev = &w
        .iter()
        .find(|x| x.kind == SuspectKind::CaptionHeading)
        .unwrap()
        .evidence;
    assert!(ev.contains("captionIndex=1"), "{ev}");
    assert!(ev.contains("level=2"), "{ev}");
}

#[test]
fn caption_heading_negatives_stay_silent() {
    // 真题注（表N 前缀编号不解析为节编号）、无编号 caption、有编号但无相邻标题兄弟、
    // 含句末标点的条目——都不标
    let m = kinds_of(&items_of(json!([
        { "type": "text", "text": "3.1差距分析流程", "text_level": 2, "page_idx": 0, "bbox": bbox(0) },
        {
            "type": "table",
            "table_body": "<table><tr><td>甲</td></tr></table>",
            "table_caption": ["表3.1 差距分析模板"],
            "page_idx": 0, "bbox": bbox(40),
        },
        {
            "type": "table",
            "table_body": "<table><tr><td>乙</td></tr></table>",
            "table_caption": ["更改情况"],
            "page_idx": 0, "bbox": bbox(80),
        },
        {
            "type": "table",
            "table_body": "<table><tr><td>丙</td></tr></table>",
            "table_caption": ["7.9远端编号无兄弟"],
            "page_idx": 0, "bbox": bbox(120),
        },
        {
            "type": "table",
            "table_body": "<table><tr><td>丁</td></tr></table>",
            "table_caption": ["3.2本条目是完整句子，含逗号与句号。"],
            "page_idx": 0, "bbox": bbox(160),
        },
    ])));
    assert!(
        m.values()
            .all(|ks| !ks.contains(&SuspectKind::CaptionHeading)),
        "{m:?}"
    );
}
