// 各 op 的纯函数语义 + 固定 op 序列 replay + 保真闸回滚（不接 LLM）。

mod common;

use common::{bbox, golden_input, items_of, mi};
use mineru_refine::id::{IdGen, assign_ids};
use mineru_refine::invariant::{check_table_bodies, input_pages};
use mineru_refine::ops::{ApplyContext, ApplyResult, RejectKind, apply_op_checked};
use mineru_refine::types::{MineruItem, OpCall, RefItem, RemovedSpan, StripPattern};
use serde_json::{Value, json};
use std::collections::HashSet;

struct Setup {
    ref_items: Vec<RefItem>,
    next_id: IdGen,
    valid_pages: HashSet<i64>,
}

impl Setup {
    fn ctx(&self) -> ApplyContext<'_> {
        ApplyContext {
            next_id: &self.next_id,
            droppable_ids: None,
            valid_pages: &self.valid_pages,
        }
    }
}

fn setup(items: &[MineruItem]) -> Setup {
    let (ref_items, next_id) = assign_ids(items);
    let valid_pages = input_pages(&ref_items.iter().map(|r| &r.item).collect::<Vec<_>>());
    Setup {
        ref_items,
        next_id,
        valid_pages,
    }
}

struct Applied {
    items: Vec<RefItem>,
    removed_spans: Vec<RemovedSpan>,
    new_ids: Vec<String>,
}

fn must_apply(items: &[RefItem], call: OpCall, ctx: &ApplyContext) -> Applied {
    match apply_op_checked(items, &call, ctx) {
        ApplyResult::Ok {
            items,
            removed_spans,
            new_ids,
        } => Applied {
            items,
            removed_spans,
            new_ids,
        },
        ApplyResult::Rejected { reason, .. } => panic!("op 应成功却失败: {reason}"),
    }
}

fn is_rejected(items: &[RefItem], call: OpCall, ctx: &ApplyContext) -> bool {
    matches!(
        apply_op_checked(items, &call, ctx),
        ApplyResult::Rejected { .. }
    )
}

fn span(item_id: &str, text: &str, reason: &str) -> Value {
    json!({ "itemId": item_id, "text": text, "reason": reason })
}

fn spans_json(spans: &[RemovedSpan]) -> Value {
    serde_json::to_value(spans).unwrap()
}

// ── 单 op 语义 ──

#[test]
fn demote_clears_text_level_keeps_id_promote_reverses() {
    let s = setup(&items_of(
        json!([{ "type": "text", "text": "第一章", "text_level": 1, "page_idx": 0, "bbox": bbox(0) }]),
    ));
    let d = must_apply(
        &s.ref_items,
        OpCall::Demote {
            id: "it_0001".into(),
        },
        &s.ctx(),
    );
    assert_eq!(d.items[0].id, "it_0001");
    assert!(d.items[0].item.text_level().is_none());
    assert!(!d.items[0].item.0.contains_key("text_level")); // 删字段，不是设 null

    let p = must_apply(
        &d.items,
        OpCall::Promote {
            id: "it_0001".into(),
            level: 2,
        },
        &s.ctx(),
    );
    assert_eq!(p.items[0].item.text_level(), Some(2));
    // 入参未被突变
    assert_eq!(s.ref_items[0].item.text_level(), Some(1));
}

#[test]
fn merge_adjacent_text_new_id_bbox_union_first_page() {
    let s = setup(&items_of(json!([
        { "type": "text", "text": "前半段未完", "page_idx": 0, "bbox": [10, 700, 500, 720] },
        { "type": "text", "text": "后半段收尾。", "page_idx": 1, "bbox": [10, 40, 480, 60] },
    ])));
    let r = must_apply(
        &s.ref_items,
        OpCall::Merge {
            id_a: "it_0001".into(),
            id_b: "it_0002".into(),
        },
        &s.ctx(),
    );
    assert_eq!(r.items.len(), 1);
    assert_eq!(r.new_ids, vec!["it_0003"]);
    assert_eq!(r.items[0].item.text(), Some("前半段未完后半段收尾。"));
    assert_eq!(r.items[0].item.page_idx(), Some(0));
    assert_eq!(r.items[0].item.0["bbox"], json!([10, 40, 500, 720]));
}

#[test]
fn merge_inserts_space_at_english_word_boundary() {
    let s = setup(&items_of(json!([
        { "type": "text", "text": "the quick brown", "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "fox jumps.", "page_idx": 1, "bbox": bbox(0) },
    ])));
    let r = must_apply(
        &s.ref_items,
        OpCall::Merge {
            id_a: "it_0001".into(),
            id_b: "it_0002".into(),
        },
        &s.ctx(),
    );
    assert_eq!(r.items[0].item.text(), Some("the quick brown fox jumps."));
}

#[test]
fn merge_rejects_non_adjacent_or_non_text() {
    let s = setup(&items_of(json!([
        { "type": "text", "text": "a", "page_idx": 0, "bbox": bbox(0) },
        { "type": "table", "table_body": "<table></table>", "table_caption": ["t"], "page_idx": 0, "bbox": bbox(20) },
        { "type": "text", "text": "b", "page_idx": 0, "bbox": bbox(40) },
    ])));
    assert!(is_rejected(
        &s.ref_items,
        OpCall::Merge {
            id_a: "it_0001".into(),
            id_b: "it_0003".into()
        },
        &s.ctx()
    ));
    assert!(is_rejected(
        &s.ref_items,
        OpCall::Merge {
            id_a: "it_0001".into(),
            id_b: "it_0002".into()
        },
        &s.ctx()
    ));
}

#[test]
fn split_two_new_ids_children_inherit_geometry_tail_loses_level() {
    let text = "1.1 范围。本规范适用于全公司。1.2 术语。下列术语适用本文件。";
    let offset = text.chars().count() - "1.2 术语。下列术语适用本文件。".chars().count();
    let s = setup(&[mi(
        json!({ "type": "text", "text": text, "text_level": 1, "page_idx": 3, "bbox": bbox(100) }),
    )]);
    let r = must_apply(
        &s.ref_items,
        OpCall::Split {
            id: "it_0001".into(),
            offset: offset as i64,
        },
        &s.ctx(),
    );
    assert_eq!(r.items.len(), 2);
    assert_eq!(r.new_ids, vec!["it_0002", "it_0003"]);
    assert_eq!(
        r.items[0].item.text(),
        Some("1.1 范围。本规范适用于全公司。")
    );
    assert_eq!(
        r.items[1].item.text(),
        Some("1.2 术语。下列术语适用本文件。")
    );
    assert_eq!(r.items[0].item.0["bbox"], bbox(100));
    assert_eq!(r.items[1].item.page_idx(), Some(3));
    assert!(r.items[1].item.text_level().is_none());
}

#[test]
fn split_rejects_out_of_range_or_empty_half() {
    let s = setup(&[mi(
        json!({ "type": "text", "text": "甲乙  丙", "page_idx": 0, "bbox": bbox(0) }),
    )]);
    assert!(is_rejected(
        &s.ref_items,
        OpCall::Split {
            id: "it_0001".into(),
            offset: 0
        },
        &s.ctx()
    ));
    assert!(is_rejected(
        &s.ref_items,
        OpCall::Split {
            id: "it_0001".into(),
            offset: 99
        },
        &s.ctx()
    ));
}

#[test]
fn reorder_contiguous_range_only_ids_unchanged() {
    let s = setup(&items_of(json!([
        { "type": "text", "text": "a", "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "b", "page_idx": 0, "bbox": bbox(20) },
        { "type": "text", "text": "c", "page_idx": 1, "bbox": bbox(0) },
        { "type": "text", "text": "d", "page_idx": 1, "bbox": bbox(20) },
    ])));
    let r = must_apply(
        &s.ref_items,
        OpCall::Reorder {
            ids_in_order: vec!["it_0003".into(), "it_0002".into()],
        },
        &s.ctx(),
    );
    assert_eq!(
        r.items.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
        vec!["it_0001", "it_0003", "it_0002", "it_0004"]
    );
    // 非连续区间被拒
    assert!(is_rejected(
        &s.ref_items,
        OpCall::Reorder {
            ids_in_order: vec!["it_0004".into(), "it_0001".into()]
        },
        &s.ctx()
    ));
}

#[test]
fn drop_whitelist_page_number_short_text_only() {
    let s = setup(&items_of(json!([
        { "type": "page_number", "text": "3", "page_idx": 0, "bbox": bbox(780) },
        { "type": "text", "text": "很长的正文。".repeat(50), "page_idx": 0, "bbox": bbox(100) },
        { "type": "table", "table_body": "<table></table>", "table_caption": ["t"], "page_idx": 0, "bbox": bbox(300) },
    ])));
    let r = must_apply(
        &s.ref_items,
        OpCall::Drop {
            id: "it_0001".into(),
        },
        &s.ctx(),
    );
    assert_eq!(r.items.len(), 2);
    assert_eq!(
        spans_json(&r.removed_spans),
        json!([span("it_0001", "3", "drop")])
    );
    assert!(is_rejected(
        &s.ref_items,
        OpCall::Drop {
            id: "it_0002".into()
        },
        &s.ctx()
    ));
    assert!(is_rejected(
        &s.ref_items,
        OpCall::Drop {
            id: "it_0003".into()
        },
        &s.ctx()
    ));
}

#[test]
fn drop_must_hit_droppable_ids_when_provided() {
    let s = setup(&items_of(
        json!([{ "type": "page_number", "text": "3", "page_idx": 0, "bbox": bbox(780) }]),
    ));
    let empty: HashSet<String> = HashSet::new();
    let ctx = ApplyContext {
        next_id: &s.next_id,
        droppable_ids: Some(&empty),
        valid_pages: &s.valid_pages,
    };
    assert!(is_rejected(
        &s.ref_items,
        OpCall::Drop {
            id: "it_0001".into()
        },
        &ctx
    ));
}

#[test]
fn strip_whitelist_patterns_with_audit_trail() {
    let s = setup(&items_of(json!([
        { "type": "text", "text": "见[附件](http://a.b/c)与$x+y$及<b>加粗</b>。", "page_idx": 0, "bbox": bbox(0) },
    ])));
    let r1 = must_apply(
        &s.ref_items,
        OpCall::Strip {
            id: "it_0001".into(),
            pattern: StripPattern::MdLink,
        },
        &s.ctx(),
    );
    assert_eq!(
        r1.items[0].item.text(),
        Some("见附件与$x+y$及<b>加粗</b>。")
    );
    assert_eq!(
        spans_json(&r1.removed_spans[..1]),
        json!([span("it_0001", "[附件](http://a.b/c)", "strip:md_link")])
    );

    let r2 = must_apply(
        &r1.items,
        OpCall::Strip {
            id: "it_0001".into(),
            pattern: StripPattern::LatexDollar,
        },
        &s.ctx(),
    );
    assert_eq!(r2.items[0].item.text(), Some("见附件与x+y及<b>加粗</b>。"));

    let r3 = must_apply(
        &r2.items,
        OpCall::Strip {
            id: "it_0001".into(),
            pattern: StripPattern::HtmlTag,
        },
        &s.ctx(),
    );
    assert_eq!(r3.items[0].item.text(), Some("见附件与x+y及加粗。"));

    // 已无可匹配 → 拒绝空操作
    assert!(is_rejected(
        &r3.items,
        OpCall::Strip {
            id: "it_0001".into(),
            pattern: StripPattern::MdLink
        },
        &s.ctx()
    ));
}

#[test]
fn strip_latex_dollar_also_strips_command_residue() {
    let s = setup(&items_of(json!([
        { "type": "text", "text": "每个元素均大于零，且 $\\mathsf { A i j } ^ { * } \\mathsf { A j i } { = } 1$ 。", "page_idx": 0, "bbox": bbox(0) },
    ])));
    let r = must_apply(
        &s.ref_items,
        OpCall::Strip {
            id: "it_0001".into(),
            pattern: StripPattern::LatexDollar,
        },
        &s.ctx(),
    );
    assert_eq!(
        r.items[0].item.text(),
        Some("每个元素均大于零，且 Aij^*Aji=1 。") // 公式体内空白是 OCR 残骸，剥离后整体删除
    );
    assert_eq!(
        r.removed_spans[0].text,
        "$\\mathsf { A i j } ^ { * } \\mathsf { A j i } { = } 1$"
    );
}

#[test]
fn strip_latex_command_removes_bare_commands_and_braces() {
    let s = setup(&items_of(json!([
        { "type": "text", "text": "一般，如果 { \\mathsf { C R } } { < } 0 . 1 ，则通过一致性检验。", "page_idx": 0, "bbox": bbox(0) },
    ])));
    let r = must_apply(
        &s.ref_items,
        OpCall::Strip {
            id: "it_0001".into(),
            pattern: StripPattern::LatexCommand,
        },
        &s.ctx(),
    );
    let text = r.items[0].item.text().unwrap();
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    assert_eq!(compact, "一般，如果CR<0.1，则通过一致性检验。");
    assert!(!text.contains('\\') && !text.contains('{') && !text.contains('}'));
}

#[test]
fn strip_escaped_dollar() {
    let s = setup(&items_of(json!([
        { "type": "text", "text": "通过\\$APPEALS客户需求分析工具了解客户需求。", "page_idx": 0, "bbox": bbox(0) },
    ])));
    let r = must_apply(
        &s.ref_items,
        OpCall::Strip {
            id: "it_0001".into(),
            pattern: StripPattern::EscapedDollar,
        },
        &s.ctx(),
    );
    assert_eq!(
        r.items[0].item.text(),
        Some("通过$APPEALS客户需求分析工具了解客户需求。")
    );
    assert_eq!(
        spans_json(&r.removed_spans),
        json!([span("it_0001", "\\$", "strip:escaped_dollar")])
    );
}

#[test]
fn strip_latex_block_removes_whole_formula_with_trail() {
    let s = setup(&items_of(
        json!([{ "type": "text", "text": "推导 $\\frac{a}{b}=c$ 略。", "page_idx": 0, "bbox": bbox(0) }]),
    ));
    let r = must_apply(
        &s.ref_items,
        OpCall::Strip {
            id: "it_0001".into(),
            pattern: StripPattern::LatexBlock,
        },
        &s.ctx(),
    );
    assert_eq!(r.items[0].item.text(), Some("推导  略。"));
    assert_eq!(r.removed_spans[0].text, "$\\frac{a}{b}=c$");
}

// ── 保真闸回滚（fidelity_violation）──

#[test]
fn geometry_gate_rolls_back_any_op() {
    let s = setup(&items_of(
        json!([{ "type": "text", "text": "x", "text_level": 1, "page_idx": 0, "bbox": bbox(0) }]),
    ));
    let other_pages = HashSet::from([99]);
    let ctx = ApplyContext {
        next_id: &s.next_id,
        droppable_ids: None,
        valid_pages: &other_pages,
    };
    match apply_op_checked(
        &s.ref_items,
        &OpCall::Demote {
            id: "it_0001".into(),
        },
        &ctx,
    ) {
        ApplyResult::Rejected { kind, .. } => assert_eq!(kind, RejectKind::FidelityViolation),
        ApplyResult::Ok { .. } => panic!("应被几何闸回滚"),
    }
    assert_eq!(s.ref_items[0].item.text_level(), Some(1)); // 未被突变
}

// ── 固定 op 序列 replay（不接 LLM）──

#[test]
fn fixed_op_sequence_replay_full_chain() {
    let s = setup(&golden_input());
    let mut items = s.ref_items.clone();
    let seq = vec![
        OpCall::Demote {
            id: "it_0002".into(),
        },
        OpCall::Merge {
            id_a: "it_0003".into(),
            id_b: "it_0004".into(),
        },
        OpCall::Drop {
            id: "it_0005".into(),
        },
        OpCall::Strip {
            id: "it_0006".into(),
            pattern: StripPattern::MdLink,
        },
    ];
    for call in seq {
        items = must_apply(&items, call, &s.ctx()).items;
    }
    assert_eq!(items.len(), 5); // 7 项：merge 并掉 1、drop 删掉 1
    assert!(items[1].item.text_level().is_none());
    assert_eq!(
        items[2].item.text(),
        Some("战略管理是指公司为实现长期发展目标而进行的一系列计划、执行与评估活动。")
    );
    assert!(!items.iter().any(|x| x.item.text() == Some("- 3 -")));
    assert_eq!(items[3].item.text(), Some("详见公司官网发布的文件。"));
    // 旧 ID 已失效（merge 产新 ID）→ 再用旧 ID 是 invalid_args，不是错位执行
    assert!(is_rejected(
        &items,
        OpCall::Demote {
            id: "it_0003".into()
        },
        &s.ctx()
    ));
}

// ── merge 跨页面家具 ──

#[test]
fn merge_across_furniture_keeps_furniture_in_place() {
    let s = setup(&items_of(json!([
        { "type": "text", "text": "前半句没说完", "page_idx": 0, "bbox": [50, 700, 550, 720] },
        { "type": "header", "text": "XX公司 版本K", "page_idx": 0, "bbox": [50, 10, 550, 30] },
        { "type": "page_number", "text": "第1页共9页", "page_idx": 0, "bbox": [50, 780, 550, 800] },
        { "type": "text", "text": "后半句收尾。", "page_idx": 1, "bbox": [50, 40, 550, 60] },
    ])));
    let r = must_apply(
        &s.ref_items,
        OpCall::Merge {
            id_a: "it_0001".into(),
            id_b: "it_0004".into(),
        },
        &s.ctx(),
    );
    assert_eq!(
        r.items
            .iter()
            .map(|x| x.item.item_type().to_string())
            .collect::<Vec<_>>(),
        vec!["text", "header", "page_number"]
    );
    assert_eq!(r.items[0].item.text(), Some("前半句没说完后半句收尾。"));

    // 中间隔着内容块（text）→ 拒绝
    let s2 = setup(&items_of(json!([
        { "type": "text", "text": "前半句没说完", "page_idx": 0, "bbox": [50, 700, 550, 720] },
        { "type": "text", "text": "插入的另一段。", "page_idx": 0, "bbox": [50, 740, 550, 760] },
        { "type": "text", "text": "后半句收尾。", "page_idx": 1, "bbox": [50, 40, 550, 60] },
    ])));
    assert!(is_rejected(
        &s2.ref_items,
        OpCall::Merge {
            id_a: "it_0001".into(),
            id_b: "it_0003".into()
        },
        &s2.ctx()
    ));
}

// ── html_tag 白名单（真实数据回归）──

#[test]
fn form_reference_in_angle_brackets_not_stripped() {
    let s = setup(&items_of(json!([
        { "type": "text", "text": "提交评审记录（表格<MB-ZZ-155 部门OGSMT>）。", "page_idx": 0, "bbox": bbox(0) },
    ])));
    // 无已知标签可匹配 → 拒绝空操作
    assert!(is_rejected(
        &s.ref_items,
        OpCall::Strip {
            id: "it_0001".into(),
            pattern: StripPattern::HtmlTag
        },
        &s.ctx()
    ));
}

// ── mergeTable（跨页拆表合并）──

fn table_a() -> Value {
    json!({
        "type": "table",
        "table_body": "<table><tbody>\n<tr><td>表头</td><td>列2</td></tr>\n<tr><td>甲</td><td>1</td></tr>\n</tbody></table>",
        "table_caption": ["表1 示例"],
        "table_footnote": ["注：A 的脚注"],
        "page_idx": 0,
        "bbox": [50, 100, 550, 800],
    })
}

fn table_b() -> Value {
    json!({
        "type": "table",
        "table_body": "<table><tbody><tr><td>乙</td><td>2</td></tr><tr><td>丙</td><td>3</td></tr></tbody></table>",
        "table_caption": ["（续）"],
        "page_idx": 1,
        "bbox": [50, 80, 550, 300],
    })
}

fn with_body(template: Value, body: &str) -> MineruItem {
    let mut v = template;
    v["table_body"] = json!(body);
    mi(v)
}

#[test]
fn merge_table_appends_b_rows_after_a_last_row() {
    let s = setup(&[
        mi(table_a()),
        mi(json!({ "type": "page_number", "text": "1", "page_idx": 0, "bbox": bbox(780) })),
        mi(table_b()),
    ]);
    let r = must_apply(
        &s.ref_items,
        OpCall::MergeTable {
            id_a: "it_0001".into(),
            id_b: "it_0003".into(),
        },
        &s.ctx(),
    );
    assert_eq!(r.items.len(), 2); // 合并块 + 原位保留的页码
    let merged = &r.items[0].item;
    assert_eq!(r.new_ids, vec![r.items[0].id.clone()]);
    assert_eq!(
        merged.table_body(),
        Some(
            "<table><tbody>\n<tr><td>表头</td><td>列2</td></tr>\n<tr><td>甲</td><td>1</td></tr><tr><td>乙</td><td>2</td></tr><tr><td>丙</td><td>3</td></tr>\n</tbody></table>"
        )
    );
    assert_eq!(merged.0["table_caption"], json!(["表1 示例", "（续）"]));
    assert_eq!(merged.0["table_footnote"], json!(["注：A 的脚注"]));
    assert_eq!(merged.page_idx(), Some(0));
    assert_eq!(merged.0["bbox"], json!([50, 80, 550, 800]));
    assert_eq!(r.items[1].item.item_type(), "page_number"); // 家具原位保留
}

#[test]
fn merge_table_dedups_byte_identical_reprinted_header_only() {
    let dup_b = with_body(
        table_b(),
        "<table><tbody><tr><td>表头</td><td>列2</td></tr><tr><td>乙</td><td>2</td></tr></tbody></table>",
    );
    let s = setup(&[mi(table_a()), dup_b]);
    let r = must_apply(
        &s.ref_items,
        OpCall::MergeTable {
            id_a: "it_0001".into(),
            id_b: "it_0002".into(),
        },
        &s.ctx(),
    );
    let body = r.items[0].item.table_body().unwrap();
    assert!(body.contains("<tr><td>甲</td><td>1</td></tr><tr><td>乙</td><td>2</td></tr>"));
    assert_eq!(body.matches("表头").count(), 1);
    assert_eq!(
        spans_json(&r.removed_spans),
        json!([span(
            "it_0002",
            "<tr><td>表头</td><td>列2</td></tr>",
            "mergeTable:dup_header"
        )])
    );

    // 近似但不逐字节相等（多一个空格）→ 不去重
    let near_b = with_body(
        table_b(),
        "<table><tbody><tr><td>表头 </td><td>列2</td></tr><tr><td>乙</td><td>2</td></tr></tbody></table>",
    );
    let s2 = setup(&[mi(table_a()), near_b]);
    let r2 = must_apply(
        &s2.ref_items,
        OpCall::MergeTable {
            id_a: "it_0001".into(),
            id_b: "it_0002".into(),
        },
        &s2.ctx(),
    );
    assert!(r2.removed_spans.is_empty());
    assert_eq!(
        r2.items[0]
            .item
            .table_body()
            .unwrap()
            .matches("表头")
            .count(),
        2
    );
}

#[test]
fn merge_table_allows_ragged_column_counts() {
    let ragged = with_body(
        table_b(),
        "<table><tbody><tr><td>乙</td><td>2</td><td>新列</td></tr></tbody></table>",
    );
    let s = setup(&[mi(table_a()), ragged]);
    let r = must_apply(
        &s.ref_items,
        OpCall::MergeTable {
            id_a: "it_0001".into(),
            id_b: "it_0002".into(),
        },
        &s.ctx(),
    );
    assert!(
        r.items[0]
            .item
            .table_body()
            .unwrap()
            .contains("<tr><td>乙</td><td>2</td><td>新列</td></tr>")
    );
}

#[test]
fn merge_table_rejects_husk_non_table_or_content_between() {
    let husk = mi(
        json!({ "type": "table", "img_path": "", "table_caption": [], "page_idx": 1, "bbox": bbox(0) }),
    );
    let s = setup(&[mi(table_a()), husk]);
    match apply_op_checked(
        &s.ref_items,
        &OpCall::MergeTable {
            id_a: "it_0001".into(),
            id_b: "it_0002".into(),
        },
        &s.ctx(),
    ) {
        ApplyResult::Rejected { kind, .. } => assert_eq!(kind, RejectKind::InvalidArgs),
        ApplyResult::Ok { .. } => panic!("空壳表应被拒"),
    }

    let s2 = setup(&[
        mi(table_a()),
        mi(json!({ "type": "text", "text": "中间的正文", "page_idx": 0, "bbox": bbox(500) })),
        mi(table_b()),
    ]);
    assert!(is_rejected(
        &s2.ref_items,
        OpCall::MergeTable {
            id_a: "it_0001".into(),
            id_b: "it_0003".into()
        },
        &s2.ctx()
    ));

    let s3 = setup(&[
        mi(table_a()),
        mi(json!({ "type": "text", "text": "x", "page_idx": 1, "bbox": bbox(0) })),
    ]);
    assert!(is_rejected(
        &s3.ref_items,
        OpCall::MergeTable {
            id_a: "it_0001".into(),
            id_b: "it_0002".into()
        },
        &s3.ctx()
    ));
}

// ── mergeList（跨页拆列表合并）──

fn list_a() -> MineruItem {
    mi(
        json!({ "type": "list", "list_items": ["第一项", "第二项未完"], "page_idx": 0, "bbox": [50, 600, 550, 800] }),
    )
}

fn list_b() -> MineruItem {
    mi(
        json!({ "type": "list", "list_items": ["的后半句。", "第三项"], "page_idx": 1, "bbox": [50, 80, 550, 200] }),
    )
}

#[test]
fn merge_list_concat_or_join_seam() {
    let s = setup(&[list_a(), list_b()]);
    let r = must_apply(
        &s.ref_items,
        OpCall::MergeList {
            id_a: "it_0001".into(),
            id_b: "it_0002".into(),
            join_seam: None,
        },
        &s.ctx(),
    );
    assert_eq!(r.items.len(), 1);
    assert_eq!(
        r.items[0].item.0["list_items"],
        json!(["第一项", "第二项未完", "的后半句。", "第三项"])
    );

    let s2 = setup(&[list_a(), list_b()]);
    let r2 = must_apply(
        &s2.ref_items,
        OpCall::MergeList {
            id_a: "it_0001".into(),
            id_b: "it_0002".into(),
            join_seam: Some(true),
        },
        &s2.ctx(),
    );
    let merged = &r2.items[0].item;
    assert_eq!(
        merged.0["list_items"],
        json!(["第一项", "第二项未完的后半句。", "第三项"])
    );
    assert_eq!(merged.page_idx(), Some(0));
    assert_eq!(merged.0["bbox"], json!([50, 80, 550, 800]));
}

#[test]
fn merge_list_join_seam_inserts_space_at_english_boundary() {
    let s = setup(&items_of(json!([
        { "type": "list", "list_items": ["item one and"], "page_idx": 0, "bbox": bbox(700) },
        { "type": "list", "list_items": ["item two"], "page_idx": 1, "bbox": bbox(80) },
    ])));
    let r = must_apply(
        &s.ref_items,
        OpCall::MergeList {
            id_a: "it_0001".into(),
            id_b: "it_0002".into(),
            join_seam: Some(true),
        },
        &s.ctx(),
    );
    assert_eq!(
        r.items[0].item.0["list_items"],
        json!(["item one and item two"])
    );
}

#[test]
fn merge_list_rejects_non_list_or_empty_items() {
    let s = setup(&[
        list_a(),
        mi(json!({ "type": "text", "text": "x", "page_idx": 1, "bbox": bbox(0) })),
    ]);
    assert!(is_rejected(
        &s.ref_items,
        OpCall::MergeList {
            id_a: "it_0001".into(),
            id_b: "it_0002".into(),
            join_seam: None
        },
        &s.ctx()
    ));
    let s2 = setup(&[
        list_a(),
        mi(json!({ "type": "list", "list_items": [], "page_idx": 1, "bbox": bbox(0) })),
    ]);
    assert!(is_rejected(
        &s2.ref_items,
        OpCall::MergeList {
            id_a: "it_0001".into(),
            id_b: "it_0002".into(),
            join_seam: None
        },
        &s2.ctx()
    ));
}

// ── drop 空壳表（白名单扩展）──

#[test]
fn drop_allows_husk_rejects_table_with_rows() {
    let s = setup(&items_of(json!([
        { "type": "table", "img_path": "", "table_caption": [], "table_footnote": [], "page_idx": 0, "bbox": bbox(0) },
        { "type": "table", "table_body": "<table><tr><td>有内容</td></tr></table>", "table_caption": [], "page_idx": 0, "bbox": bbox(300) },
    ])));
    let r = must_apply(
        &s.ref_items,
        OpCall::Drop {
            id: "it_0001".into(),
        },
        &s.ctx(),
    );
    assert_eq!(r.items.len(), 1);
    assert_eq!(
        spans_json(&r.removed_spans),
        json!([span("it_0001", "[table]", "drop")])
    );
    assert!(is_rejected(
        &s.ref_items,
        OpCall::Drop {
            id: "it_0002".into()
        },
        &s.ctx()
    ));
}

#[test]
fn drop_husk_must_hit_droppable_ids_when_provided() {
    let s = setup(&items_of(json!([
        { "type": "table", "img_path": "", "table_caption": [], "page_idx": 0, "bbox": bbox(0) },
    ])));
    let empty: HashSet<String> = HashSet::new();
    let denied_ctx = ApplyContext {
        next_id: &s.next_id,
        droppable_ids: Some(&empty),
        valid_pages: &s.valid_pages,
    };
    assert!(is_rejected(
        &s.ref_items,
        OpCall::Drop {
            id: "it_0001".into()
        },
        &denied_ctx
    ));
    let allowed: HashSet<String> = HashSet::from(["it_0001".to_string()]);
    let allowed_ctx = ApplyContext {
        next_id: &s.next_id,
        droppable_ids: Some(&allowed),
        valid_pages: &s.valid_pages,
    };
    assert!(!is_rejected(
        &s.ref_items,
        OpCall::Drop {
            id: "it_0001".into()
        },
        &allowed_ctx
    ));
}

// ── mergeTable 列参差矩阵（空列被 MinerU 略去/保留的各种形态）──

/// 3 列逻辑表的第一页：尾列全空被 MinerU 略去 → 识别成 2 列。
fn a_tail_dropped() -> Value {
    json!({
        "type": "table",
        "table_body": "<table><tbody><tr><td>名称</td><td>数量</td></tr><tr><td>甲</td><td>1</td></tr></tbody></table>",
        "table_caption": ["表X"],
        "page_idx": 0,
        "bbox": [50, 100, 550, 800],
    })
}

fn merge2(a: MineruItem, b: MineruItem) -> Applied {
    let s = setup(&[a, b]);
    must_apply(
        &s.ref_items,
        OpCall::MergeTable {
            id_a: "it_0001".into(),
            id_b: "it_0002".into(),
        },
        &s.ctx(),
    )
}

fn plain_table(body: &str) -> MineruItem {
    mi(
        json!({ "type": "table", "table_body": body, "table_caption": [], "page_idx": 1, "bbox": [50, 80, 550, 200] }),
    )
}

#[test]
fn ragged_merge_2col_plus_3col_no_padding_invented() {
    let b =
        plain_table("<table><tbody><tr><td>乙</td><td>2</td><td>备注B</td></tr></tbody></table>");
    let r = merge2(mi(a_tail_dropped()), b);
    let body = r.items[0].item.table_body().unwrap();
    assert!(body.contains("<tr><td>甲</td><td>1</td></tr>")); // A 的 2 列行原样
    assert!(body.contains("<tr><td>乙</td><td>2</td><td>备注B</td></tr>")); // B 的 3 列行原样
    assert!(!body.contains("<td></td>")); // 绝不发明空单元格去"对齐"
}

#[test]
fn empty_leading_cell_preserved_byte_for_byte() {
    let b = plain_table(
        "<table><tbody><tr><td></td><td>2</td></tr><tr><td>丙</td><td>3</td></tr></tbody></table>",
    );
    let r = merge2(mi(a_tail_dropped()), b);
    assert!(
        r.items[0]
            .item
            .table_body()
            .unwrap()
            .contains("<tr><td>甲</td><td>1</td></tr><tr><td></td><td>2</td></tr>")
    );
}

#[test]
fn dropped_leading_cell_preserved_as_is() {
    // 逻辑上是「(空), 2」但 MinerU 只吐了一格
    let b = plain_table("<table><tbody><tr><td>2</td></tr></tbody></table>");
    let r = merge2(mi(a_tail_dropped()), b);
    assert!(
        r.items[0]
            .item
            .table_body()
            .unwrap()
            .contains("<tr><td>甲</td><td>1</td></tr><tr><td>2</td></tr>")
    );
}

#[test]
fn rowspan_carryover_with_unequal_columns_merges() {
    let a = mi(json!({
        "type": "table",
        "table_body": "<table><tbody><tr><td rowspan=1 colspan=1>考核项目</td><td rowspan=1 colspan=1>权重</td><td rowspan=1 colspan=1>维度编号</td><td rowspan=1 colspan=1>评分标准</td><td rowspan=1 colspan=1>得分</td></tr><tr><td rowspan=1 colspan=1>评分依据：所直接关联的上级战略指标的达成情况。</td></tr></tbody></table>",
        "table_caption": ["报告评分表"],
        "page_idx": 13,
        "bbox": [50, 100, 550, 800],
    }));
    let b = mi(json!({
        "type": "table",
        "table_body": "<table><tbody><tr><td rowspan=8 colspan=1>指标的战略协同与支撑</td><td rowspan=8 colspan=1></td><td rowspan=2 colspan=1></td><td></td></tr></tbody></table>",
        "table_caption": [],
        "page_idx": 14,
        "bbox": [50, 80, 550, 300],
    }));
    let r = merge2(a.clone(), b.clone());
    let merged = &r.items[0].item;
    assert!(
        merged
            .table_body()
            .unwrap()
            .contains("评分依据：所直接关联的上级战略指标的达成情况。</td></tr><tr><td rowspan=8")
    );
    assert_eq!(merged.0["table_caption"], json!(["报告评分表"]));
    // 行级保真：合并体能通过出口闸门
    assert!(check_table_bodies(&[&a, &b], &[merged]).is_ok());
}

#[test]
fn colspan_row_preserved() {
    let b =
        plain_table("<table><tbody><tr><td colspan=\"2\">文件状态：受控</td></tr></tbody></table>");
    let r = merge2(mi(a_tail_dropped()), b);
    assert!(
        r.items[0]
            .item
            .table_body()
            .unwrap()
            .contains("<tr><td colspan=\"2\">文件状态：受控</td></tr>")
    );
}

#[test]
fn padded_repair_attempt_fails_row_level_gate() {
    let a = mi(a_tail_dropped());
    let b =
        plain_table("<table><tbody><tr><td>乙</td><td>2</td><td>备注B</td></tr></tbody></table>");
    // 假想某实现把 A 的行补齐成 3 列：<tr><td>甲</td><td>1</td><td></td></tr>
    let padded = mi(json!({
        "type": "table",
        "table_body": "<table><tbody><tr><td>名称</td><td>数量</td></tr><tr><td>甲</td><td>1</td><td></td></tr><tr><td>乙</td><td>2</td><td>备注B</td></tr></tbody></table>",
    }));
    assert!(check_table_bodies(&[&a, &b], &[&padded]).is_err());
}

// ── deleteChar ──

#[test]
fn delete_char_removes_dup_word_and_radical_keeps_id() {
    let s = setup(&items_of(json!([
        { "type": "text", "text": "基本治理理念的的变化情况。", "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "3）亻残留", "page_idx": 0, "bbox": bbox(40) },
    ])));
    let a = must_apply(
        &s.ref_items,
        OpCall::DeleteChar {
            id: "it_0001".into(),
            offset: 7,
        },
        &s.ctx(),
    );
    assert_eq!(a.items[0].item.text().unwrap(), "基本治理理念的变化情况。");
    assert_eq!(a.items[0].id, "it_0001"); // 继承原 ID
    assert!(a.new_ids.is_empty());
    assert_eq!(
        spans_json(&a.removed_spans),
        json!([span("it_0001", "的", "deleteChar:dup_char")])
    );

    let b = must_apply(
        &a.items,
        OpCall::DeleteChar {
            id: "it_0002".into(),
            offset: 2,
        },
        &s.ctx(),
    );
    assert_eq!(b.items[1].item.text().unwrap(), "3）残留");
    assert_eq!(
        spans_json(&b.removed_spans),
        json!([span("it_0002", "亻", "deleteChar:radical")])
    );
}

#[test]
fn delete_char_rejects_out_of_whitelist_and_idioms() {
    let s = setup(&items_of(json!([
        { "type": "text", "text": "的的确确发生过", "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "普通正文内容", "page_idx": 0, "bbox": bbox(40) },
    ])));
    // 成语构造性保护
    assert!(is_rejected(
        &s.ref_items,
        OpCall::DeleteChar {
            id: "it_0001".into(),
            offset: 1,
        },
        &s.ctx(),
    ));
    // 非白名单字符
    assert!(is_rejected(
        &s.ref_items,
        OpCall::DeleteChar {
            id: "it_0002".into(),
            offset: 0,
        },
        &s.ctx(),
    ));
    // 越界
    assert!(is_rejected(
        &s.ref_items,
        OpCall::DeleteChar {
            id: "it_0001".into(),
            offset: 99,
        },
        &s.ctx(),
    ));
}

// ── extractCaption ──

fn doc_with_swallowed_heading() -> Vec<MineruItem> {
    items_of(json!([
        { "type": "text", "text": "4.5核心组织绩效的考核方法", "text_level": 2, "page_idx": 0, "bbox": bbox(0) },
        {
            "type": "table",
            "table_body": "<table><tr><td>考核项目</td><td>权重</td></tr></table>",
            "table_caption": ["报告评分表", "4.6核心组织绩效的应用"],
            "page_idx": 0,
            "bbox": bbox(40),
        },
        { "type": "text", "text": "核心指标结果将运用于薪酬计算。", "page_idx": 0, "bbox": bbox(80) },
    ]))
}

#[test]
fn extract_caption_after_with_level_moves_entry_verbatim() {
    use mineru_refine::types::ExtractPosition;
    let s = setup(&doc_with_swallowed_heading());
    let a = must_apply(
        &s.ref_items,
        OpCall::ExtractCaption {
            id: "it_0002".into(),
            caption_index: 1,
            position: ExtractPosition::After,
            level: Some(2),
        },
        &s.ctx(),
    );
    assert_eq!(a.items.len(), 4);
    assert!(a.removed_spans.is_empty()); // 纯移动，无削减
    // 表格继承原 ID，caption 只剩真题注，body 不动
    assert_eq!(a.items[1].id, "it_0002");
    assert_eq!(
        a.items[1].item.str_array("table_caption").unwrap(),
        vec!["报告评分表"]
    );
    assert_eq!(
        a.items[1].item.table_body().unwrap(),
        "<table><tr><td>考核项目</td><td>权重</td></tr></table>"
    );
    // 抽出块在表格之后：新 ID、text_level=2、几何继承表格
    let extracted = &a.items[2];
    assert_eq!(a.new_ids, vec![extracted.id.clone()]);
    assert_eq!(extracted.item.item_type(), "text");
    assert_eq!(extracted.item.text().unwrap(), "4.6核心组织绩效的应用");
    assert_eq!(extracted.item.text_level(), Some(2));
    assert_eq!(extracted.item.page_idx(), Some(0));
    assert_eq!(
        serde_json::to_value(extracted.item.bbox().unwrap()).unwrap(),
        json!([50.0, 40.0, 550.0, 60.0])
    );
}

#[test]
fn extract_caption_before_without_level_yields_plain_text() {
    use mineru_refine::types::ExtractPosition;
    let s = setup(&items_of(json!([
        {
            "type": "table",
            "table_body": "<table><tr><td>日期</td><td>修改内容</td></tr></table>",
            "table_caption": ["更改情况"],
            "page_idx": 0,
            "bbox": bbox(0),
        },
    ])));
    let a = must_apply(
        &s.ref_items,
        OpCall::ExtractCaption {
            id: "it_0001".into(),
            caption_index: 0,
            position: ExtractPosition::Before,
            level: None,
        },
        &s.ctx(),
    );
    assert_eq!(a.items.len(), 2);
    assert_eq!(a.items[0].item.text().unwrap(), "更改情况");
    assert_eq!(a.items[0].item.text_level(), None);
    assert_eq!(a.items[1].id, "it_0001");
    assert_eq!(a.items[1].item.str_array("table_caption").unwrap().len(), 0);
}

#[test]
fn extract_caption_rejects_bad_args() {
    use mineru_refine::types::ExtractPosition;
    let s = setup(&doc_with_swallowed_heading());
    let call = |id: &str, ci: i64, level: Option<i64>| OpCall::ExtractCaption {
        id: id.into(),
        caption_index: ci,
        position: ExtractPosition::After,
        level,
    };
    assert!(is_rejected(
        &s.ref_items,
        call("it_0001", 0, Some(2)),
        &s.ctx()
    )); // 非 table
    assert!(is_rejected(
        &s.ref_items,
        call("it_0002", 2, Some(2)),
        &s.ctx()
    )); // index 越界
    assert!(is_rejected(
        &s.ref_items,
        call("it_0002", -1, Some(2)),
        &s.ctx()
    ));
    assert!(is_rejected(
        &s.ref_items,
        call("it_0002", 1, Some(0)),
        &s.ctx()
    )); // level 非法
}
