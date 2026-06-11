// 保真不变式单测。

mod common;

use common::items_of;
use mineru_refine::id::assign_ids;
use mineru_refine::invariant::{
    check_char_subset, check_fidelity, check_geometry, check_table_bodies, content_chars,
    input_pages,
};
use mineru_refine::types::MineruItem;
use serde_json::json;
use std::collections::HashSet;

fn refs(items: &[MineruItem]) -> Vec<&MineruItem> {
    items.iter().collect()
}

#[test]
fn content_chars_counts_text_list_items_caption_only() {
    let items = items_of(json!([
        { "type": "text", "text": "甲 乙\n甲" },
        { "type": "list", "list_items": ["丙", "丁 丁"] },
        { "type": "table", "table_body": "<table>不计入</table>", "table_caption": ["表1"] },
        { "type": "image", "img_path": "images/x.jpg" },
    ]));
    let c = content_chars(&refs(&items));
    assert_eq!(c.get(&'甲'), Some(&2));
    assert_eq!(c.get(&'乙'), Some(&1));
    assert_eq!(c.get(&'丁'), Some(&2));
    assert_eq!(c.get(&'表'), Some(&1));
    assert_eq!(c.get(&'不'), None); // table_body 不计
    assert_eq!(c.get(&' '), None); // 空白不计
}

// ── checkCharSubset (C_out ⊆ C_in) ──

fn cin() -> Vec<MineruItem> {
    items_of(json!([{ "type": "text", "text": "天地玄黄，宇宙洪荒。" }]))
}

fn text_items(t: &str) -> Vec<MineruItem> {
    items_of(json!([{ "type": "text", "text": t }]))
}

#[test]
fn reduction_is_legal() {
    assert!(check_char_subset(&refs(&cin()), &refs(&text_items("天地玄黄。"))).is_ok());
}

#[test]
fn reorder_is_legal() {
    assert!(check_char_subset(&refs(&cin()), &refs(&text_items("宇宙洪荒，天地玄黄。"))).is_ok());
}

#[test]
fn new_chars_violate() {
    let r = check_char_subset(
        &refs(&cin()),
        &refs(&text_items("天地玄黄，宇宙洪荒，日月盈昃。")),
    );
    assert!(r.unwrap_err().contains("C_out ⊄ C_in"));
}

#[test]
fn multiset_overflow_violates() {
    assert!(check_char_subset(&refs(&cin()), &refs(&text_items("天天"))).is_err());
}

#[test]
fn whitespace_changes_never_violate() {
    assert!(
        check_char_subset(
            &refs(&cin()),
            &refs(&text_items("天 地\n玄\t黄，宇宙洪荒。"))
        )
        .is_ok()
    );
}

// ── checkTableBodies ──

fn tin() -> Vec<MineruItem> {
    items_of(json!([
        { "type": "table", "table_body": "<table>A</table>" },
        { "type": "table", "table_body": "<table>B</table>" },
    ]))
}

#[test]
fn byte_equal_and_dropped_tables_pass() {
    let t = tin();
    assert!(check_table_bodies(&refs(&t), &refs(&t)).is_ok());
    let dropped = vec![t[0].clone()];
    assert!(check_table_bodies(&refs(&t), &refs(&dropped)).is_ok());
}

#[test]
fn single_byte_tamper_violates() {
    let out = items_of(json!([{ "type": "table", "table_body": "<table>a</table>" }]));
    assert!(check_table_bodies(&refs(&tin()), &refs(&out)).is_err());
}

// ── checkGeometry ──

#[test]
fn invalid_bbox_or_foreign_page_fails() {
    let (good, _) = assign_ids(&items_of(
        json!([{ "type": "text", "text": "x", "page_idx": 0, "bbox": [0, 0, 1, 1] }]),
    ));
    assert!(check_geometry(&good, &HashSet::from([0])).is_ok());
    assert!(check_geometry(&good, &HashSet::from([5])).is_err());
    let (bad, _) = assign_ids(&items_of(
        json!([{ "type": "text", "text": "x", "page_idx": 0, "bbox": [0, 0, 1] }]),
    ));
    assert!(check_geometry(&bad, &HashSet::from([0])).is_err());
}

// ── checkFidelity 组合 ──

#[test]
fn skips_geometry_when_input_lacks_bbox() {
    let (before, _) = assign_ids(&items_of(json!([{ "type": "text", "text": "甲乙丙" }])));
    let (after, _) = assign_ids(&items_of(json!([{ "type": "text", "text": "甲乙" }])));
    assert!(check_fidelity(&before, &after, None).is_ok());
}

#[test]
fn enforces_geometry_when_input_has_it() {
    let (before, _) = assign_ids(&items_of(
        json!([{ "type": "text", "text": "甲乙丙", "page_idx": 0, "bbox": [0, 0, 1, 1] }]),
    ));
    let (after_bad, _) = assign_ids(&items_of(
        json!([{ "type": "text", "text": "甲乙", "page_idx": 9 }]),
    ));
    assert!(check_fidelity(&before, &after_bad, None).is_err());
}

#[test]
fn input_pages_collects_page_set() {
    let items =
        items_of(json!([{ "type": "text", "page_idx": 0 }, { "type": "text", "page_idx": 2 }]));
    let mut pages: Vec<i64> = input_pages(&refs(&items)).into_iter().collect();
    pages.sort_unstable();
    assert_eq!(pages, vec![0, 2]);
}

// ── checkTableBodies 行级路径（mergeTable 产物）──

const BODY_A: &str =
    "<table><tbody>\n<tr><td>表头</td></tr>\n<tr><td>甲</td></tr>\n</tbody></table>";
const BODY_B: &str = "<table><tbody><tr><td>乙</td></tr></tbody></table>";

fn row_tin() -> Vec<MineruItem> {
    items_of(json!([
        { "type": "table", "table_body": BODY_A },
        { "type": "table", "table_body": BODY_B },
    ]))
}

fn table(body: &str) -> Vec<MineruItem> {
    items_of(json!([{ "type": "table", "table_body": body }]))
}

#[test]
fn legal_merge_and_row_subset_pass() {
    let merged = "<table><tbody>\n<tr><td>表头</td></tr>\n<tr><td>甲</td></tr><tr><td>乙</td></tr>\n</tbody></table>";
    assert!(check_table_bodies(&refs(&row_tin()), &refs(&table(merged))).is_ok());
    // 少一行（如重复表头被去）仍是子集 → pass
    let fewer = "<table><tbody>\n<tr><td>表头</td></tr>\n<tr><td>乙</td></tr>\n</tbody></table>";
    assert!(check_table_bodies(&refs(&row_tin()), &refs(&table(fewer))).is_ok());
}

#[test]
fn tampered_row_shell_or_double_consumption_fail() {
    let tampered_row = "<table><tbody>\n<tr><td>表头！</td></tr>\n<tr><td>甲</td></tr><tr><td>乙</td></tr>\n</tbody></table>";
    assert!(check_table_bodies(&refs(&row_tin()), &refs(&table(tampered_row))).is_err());
    let tampered_shell = "<table class=x><tbody>\n<tr><td>表头</td></tr>\n<tr><td>甲</td></tr><tr><td>乙</td></tr>\n</tbody></table>";
    assert!(check_table_bodies(&refs(&row_tin()), &refs(&table(tampered_shell))).is_err());
    // 同一输入行被两个输出表消费 → 第二次 fail
    let dup_use = items_of(json!([
        { "type": "table", "table_body": BODY_B },
        { "type": "table", "table_body": "<table><tbody><tr><td>乙</td></tr><tr><td>甲</td></tr></tbody></table>" },
    ]));
    assert!(check_table_bodies(&refs(&row_tin()), &refs(&dup_use)).is_err());
}
