// 机械清洗 pass 单测：确定性、自校验、不打 LLM。

mod common;

use common::{bbox, items_of};
use mineru_refine::agent_loop::Logger;
use mineru_refine::id::assign_ids;
use mineru_refine::invariant::table_shell;
use mineru_refine::mechanical::{MechOutcome, mechanical_clean};
use mineru_refine::types::{MineruItem, RefItem};
use serde_json::json;
use std::sync::Arc;

fn clean(items: Vec<MineruItem>) -> (Vec<RefItem>, MechOutcome) {
    let (mut ref_items, _) = assign_ids(&items);
    let log: Logger = Arc::new(|_| {});
    let out = mechanical_clean(&mut ref_items, &log);
    (ref_items, out)
}

fn count(o: &MechOutcome, key: &str) -> u64 {
    o.counts.get(key).copied().unwrap_or(0)
}

#[test]
fn trailing_empty_rows_removed() {
    let body = "<table><tr><td>日期</td><td>内容</td></tr>\
                <tr><td></td><td></td></tr><tr><td></td><td></td></tr></table>";
    let (items, o) = clean(items_of(json!([
        { "type": "table", "table_body": body, "table_caption": ["更改情况"], "page_idx": 0, "bbox": bbox(0) },
    ])));
    let new_body = items[0].item.table_body().unwrap();
    assert_eq!(
        new_body,
        "<table><tr><td>日期</td><td>内容</td></tr></table>"
    );
    assert_eq!(count(&o, "mechEmptyRow"), 2);
    assert_eq!(table_shell(body), table_shell(new_body)); // shell 逐字节不变
}

#[test]
fn continuation_rows_merged_into_previous_record() {
    // 一条记录的长 cell 被跨页切成 3 个 <tr>：后两行除长列外全空
    let body = "<table>\
        <tr><td>2026.03.24</td><td>Ed</td><td>1、更新要求：报告须由部门经理编写，其他可委托主管或者其他人员</td></tr>\
        <tr><td></td><td></td><td>填写。 2、更新策略标注规则：新增系统任务责任归属须为部门经理或总监对应的系统</td></tr>\
        <tr><td></td><td></td><td>任务，责任主体必须为部门经理或总监。</td></tr>\
        </table>";
    let (items, o) = clean(items_of(json!([
        { "type": "table", "table_body": body, "page_idx": 0, "bbox": bbox(0) },
    ])));
    let new_body = items[0].item.table_body().unwrap();
    assert_eq!(count(&o, "mechRowMerge"), 2);
    assert!(new_body.contains("其他人员填写。"));
    assert!(new_body.contains("系统任务，责任主体"));
    assert_eq!(new_body.matches("<tr>").count(), 1);
}

#[test]
fn continuation_not_merged_when_prev_cell_ends_sentence_or_columns_differ() {
    // 上一行长列以句号收尾 → 不是续行
    let ended = "<table>\
        <tr><td>1</td><td>本条记录的修改内容已经写完并经过审批确认发布，到此完整收尾。</td></tr>\
        <tr><td></td><td>新内容</td></tr></table>";
    let (items, o) = clean(items_of(json!([
        { "type": "table", "table_body": ended, "page_idx": 0, "bbox": bbox(0) },
    ])));
    assert_eq!(count(&o, "mechRowMerge"), 0);
    assert_eq!(items[0].item.table_body().unwrap(), ended);

    // 列数不等（rowspan 携带等情形）→ 不动
    let ragged = "<table>\
        <tr><td>甲</td><td>乙</td><td>未完</td></tr>\
        <tr><td></td><td>续文</td></tr></table>";
    let (items, o) = clean(items_of(json!([
        { "type": "table", "table_body": ragged, "page_idx": 0, "bbox": bbox(0) },
    ])));
    assert_eq!(count(&o, "mechRowMerge"), 0);
    assert_eq!(items[0].item.table_body().unwrap(), ragged);
}

#[test]
fn template_table_with_label_rows_not_merged() {
    // SWOT 空表：每行只有首列标签、其余为空——"恰一个非空 cell"命中但不是末列，
    // 且上一行同列是短标签 → 绝不能并行（真实数据踩过，靠末列+长度门槛挡住）
    let body = "<table>\
        <tr><td>要素类型</td><td>第一</td><td>第二</td><td>第三</td></tr>\
        <tr><td>S</td><td></td><td></td><td></td></tr>\
        <tr><td>W</td><td></td><td></td><td></td></tr></table>";
    let (items, o) = clean(items_of(json!([
        { "type": "table", "table_body": body, "page_idx": 0, "bbox": bbox(0) },
    ])));
    assert!(o.counts.is_empty(), "{:?}", o.counts); // 不合并、不删行、不回退
    assert_eq!(items[0].item.table_body().unwrap(), body);
}

#[test]
fn rowspan_table_only_trims_trailing_empty_rows_and_skips_row_merge() {
    // 第 2 行的"空行"其实被 rowspan=2 覆盖 → 必须保留；尾部真空行可删
    for attr in ["rowspan=2", "rowspan=\"2\"", "rowspan='2'"] {
        let body = format!(
            "<table>\
            <tr><td {attr}>A</td><td>x</td></tr>\
            <tr><td></td></tr>\
            <tr><td>B</td><td>未完</td></tr>\
            <tr><td></td><td>续文</td></tr>\
            <tr><td></td><td></td></tr></table>"
        );
        let (items, o) = clean(items_of(json!([
            { "type": "table", "table_body": body, "page_idx": 0, "bbox": bbox(0) },
        ])));
        let new_body = items[0].item.table_body().unwrap();
        assert_eq!(count(&o, "mechEmptyRow"), 1, "{attr}"); // 只删了最后的真空行
        assert_eq!(count(&o, "mechRowMerge"), 0, "{attr}"); // 含 rowspan>1 的表不做续行合并
        assert!(new_body.contains("<tr><td></td></tr>"), "{attr}"); // rowspan 覆盖行保留
        assert!(new_body.contains("续文"), "{attr}");
    }
}

#[test]
fn cell_whitespace_tightened_between_cjk() {
    let body =
        "<table><tr><td colspan=\"4\">批    准</td><td>部  门</td><td>a  b</td></tr></table>";
    let (items, o) = clean(items_of(json!([
        { "type": "table", "table_body": body, "page_idx": 0, "bbox": bbox(0) },
    ])));
    let new_body = items[0].item.table_body().unwrap();
    assert!(new_body.contains(">批准<"));
    assert!(new_body.contains(">部门<"));
    assert!(new_body.contains(">a b<")); // 非 CJK 两侧收为单个空格
    assert_eq!(count(&o, "mechCellWs"), 3);
}

#[test]
fn url_inner_spaces_removed_in_text_and_cells() {
    let (items, o) = clean(items_of(json!([
        { "type": "text", "text": "详见 https://data.stats. gov. cn/easyquery. htm?cn=C01 查询入口", "page_idx": 0, "bbox": bbox(0) },
        { "type": "table", "table_body": "<table><tr><td>https://www. meteringchina. com</td></tr></table>", "page_idx": 0, "bbox": bbox(40) },
    ])));
    assert_eq!(
        items[0].item.text().unwrap(),
        "详见 https://data.stats.gov.cn/easyquery.htm?cn=C01 查询入口"
    );
    assert!(
        items[1]
            .item
            .table_body()
            .unwrap()
            .contains(">https://www.meteringchina.com<")
    );
    assert_eq!(count(&o, "mechUrlWs"), 5);
}

#[test]
fn escaped_dollar_and_star_unescaped_with_audit_trail() {
    let (items, o) = clean(items_of(json!([
        { "type": "text", "text": "表3.1\\$APPEALS指标综合评分", "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "数量／基数\\*100%", "page_idx": 0, "bbox": bbox(40) },
        { "type": "table", "table_body": "<table><tr><td>x</td></tr></table>",
          "table_caption": ["表3.2\\$APPEALS评分"], "page_idx": 0, "bbox": bbox(80) },
    ])));
    assert_eq!(items[0].item.text().unwrap(), "表3.1$APPEALS指标综合评分");
    assert_eq!(items[1].item.text().unwrap(), "数量／基数*100%");
    assert_eq!(
        items[2].item.str_array("table_caption").unwrap(),
        vec!["表3.2$APPEALS评分"]
    );
    assert_eq!(count(&o, "mechUnescape"), 3);
    assert!(
        o.removed_spans
            .iter()
            .all(|s| s.reason == "mech:unescape" && (s.text == "\\$" || s.text == "\\*"))
    );
}

#[test]
fn clean_input_is_untouched_with_empty_counts() {
    let input = items_of(json!([
        { "type": "text", "text": "正常段落，无需清理。", "page_idx": 0, "bbox": bbox(0) },
        { "type": "table", "table_body": "<table><tr><td>指标</td><td>目标值</td></tr></table>",
          "table_caption": ["表1"], "page_idx": 0, "bbox": bbox(40) },
    ]));
    let (items, o) = clean(input.clone());
    assert!(o.counts.is_empty());
    assert_eq!(
        serde_json::to_value(items.iter().map(|r| &r.item).collect::<Vec<_>>()).unwrap(),
        serde_json::to_value(&input).unwrap()
    );
}
