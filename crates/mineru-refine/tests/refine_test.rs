// eval 六件套 + fail-open + 缓存 + schema 透明性。LLM 全程 mock，不打真 API。

mod common;

use common::{
    ExplodingChat, FnLoader, FnVision, KindHandler, MockChat, NEXT_ID_RE, bbox, golden_expected,
    golden_input, items_of, verdict,
};
use mineru_refine::agent_loop::Logger;
use mineru_refine::detect::detect_items;
use mineru_refine::invariant::{check_char_subset, check_table_bodies};
use mineru_refine::llm::{ChatClient, LlmError, LoadImage, VisionClient};
use mineru_refine::types::{MineruItem, RefineResult, SuspectKind};
use mineru_refine::{RefineOptions, adaptive_max_iterations, refine};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

fn silent() -> Option<Logger> {
    Some(Arc::new(|_| {}))
}

fn refs(items: &[MineruItem]) -> Vec<&MineruItem> {
    items.iter().collect()
}

async fn run(items: Vec<MineruItem>, chat: Arc<dyn ChatClient>) -> RefineResult {
    refine(
        items,
        RefineOptions {
            chat: Some(chat),
            log: silent(),
            ..Default::default()
        },
    )
    .await
}

fn op_counts_json(r: &RefineResult) -> Value {
    serde_json::to_value(&r.report.op_counts).unwrap()
}

fn dismiss_handler() -> KindHandler {
    Box::new(|id, _| Ok(("dismiss".into(), json!({ "id": id, "reason": "测试" }))))
}

// ── ① golden fixtures ──

#[tokio::test]
async fn golden_fixture_produces_expected_output() {
    let r = run(golden_input(), Arc::new(MockChat::new())).await;
    assert!(!r.report.fail_open);
    assert_eq!(
        serde_json::to_value(&r.items).unwrap(),
        serde_json::to_value(golden_expected()).unwrap()
    );
    assert_eq!(
        op_counts_json(&r),
        json!({ "demote": 1, "merge": 1, "drop": 1, "strip": 1 })
    );
    let mut reasons: Vec<&str> = r
        .report
        .removed_spans
        .iter()
        .map(|s| s.reason.as_str())
        .collect();
    reasons.sort_unstable();
    assert_eq!(reasons, vec!["drop", "strip:md_link"]);
    assert!(r.provenance.is_empty()); // 纯削减模式下恒为空
}

// ── ② 保真不变式 C_out ⊆ C_in ──

#[tokio::test]
async fn output_has_no_new_non_whitespace_chars() {
    let input = golden_input();
    let r = run(input.clone(), Arc::new(MockChat::new())).await;
    assert!(check_char_subset(&refs(&input), &refs(&r.items)).is_ok());
}

// ── ③ table_body 不变 ──

#[tokio::test]
async fn table_bodies_byte_identical() {
    let input = golden_input();
    let r = run(input.clone(), Arc::new(MockChat::new())).await;
    assert!(check_table_bodies(&refs(&input), &refs(&r.items)).is_ok());
    let table = r.items.iter().find(|it| it.item_type() == "table").unwrap();
    let orig = input.iter().find(|it| it.item_type() == "table").unwrap();
    assert_eq!(table.table_body(), orig.table_body());
}

// ── ④ 异常数单调 ──

#[tokio::test]
async fn actionable_suspects_monotonically_decrease() {
    let input = golden_input();
    let before = detect_items(&input).iter().filter(|w| w.has_op).count();
    let r = run(input, Arc::new(MockChat::new())).await;
    let after = detect_items(&r.items).iter().filter(|w| w.has_op).count();
    assert_eq!(before, 4);
    assert_eq!(after, 0); // golden 文档应被清干净
}

// ── ⑤ 几何可定位 ──

#[tokio::test]
async fn geometry_resolvable_for_every_output_item() {
    let input = golden_input();
    let in_pages: Vec<i64> = input.iter().filter_map(|it| it.page_idx()).collect();
    let r = run(input, Arc::new(MockChat::new())).await;
    for it in &r.items {
        assert!(it.bbox().is_some());
        assert!(in_pages.contains(&it.page_idx().unwrap()));
    }
}

// ── ⑥ 幂等 ──

#[tokio::test]
async fn idempotent_second_run_is_noop_with_zero_llm_calls() {
    let first = run(golden_input(), Arc::new(MockChat::new())).await;
    let boom = Arc::new(ExplodingChat::new());
    let second = run(first.items.clone(), boom.clone()).await;
    assert_eq!(
        serde_json::to_value(&second.items).unwrap(),
        serde_json::to_value(&first.items).unwrap()
    );
    assert_eq!(second.report.iterations, 0);
    assert_eq!(boom.call_count(), 0); // worklist 为空 → 一次 LLM 都不调
    assert!(!second.report.fail_open);
}

// ── fail-open（异常时原样返回输入）──

#[tokio::test]
async fn llm_unavailable_fails_open_returning_input() {
    let input = golden_input();
    let r = run(input.clone(), Arc::new(ExplodingChat::new())).await;
    assert!(r.report.fail_open);
    assert_eq!(
        serde_json::to_value(&r.items).unwrap(),
        serde_json::to_value(&input).unwrap()
    );
}

#[tokio::test]
async fn all_ops_rejected_suspends_suspects_without_crashing() {
    let mut overrides: HashMap<SuspectKind, KindHandler> = HashMap::new();
    overrides.insert(
        SuspectKind::PseudoHeading,
        Box::new(|_, _| Ok(("demote".into(), json!({ "id": "it_9999" })))), // 不存在的 ID
    );
    overrides.insert(SuspectKind::CrossPageBreak, dismiss_handler());
    overrides.insert(SuspectKind::PageArtifact, dismiss_handler());
    overrides.insert(SuspectKind::ResidualMarkup, dismiss_handler());
    let r = run(golden_input(), Arc::new(MockChat::with(overrides))).await;
    assert!(!r.report.fail_open);
    assert_eq!(
        serde_json::to_value(&r.items).unwrap(),
        serde_json::to_value(golden_input()).unwrap()
    ); // 什么都没改
    assert_eq!(r.report.dismissed, 4);
}

// ── 缓存（sha256 + 逻辑/模型/prompt 版本）──

#[tokio::test]
async fn cache_hits_on_same_sha256_skips_loop() {
    let mock1 = Arc::new(MockChat::new());
    let r1 = refine(
        golden_input(),
        RefineOptions {
            chat: Some(mock1.clone()),
            sha256: Some("rust-cache-abc123".into()),
            log: silent(),
            ..Default::default()
        },
    )
    .await;
    assert!(mock1.call_count() > 0);

    let boom = Arc::new(ExplodingChat::new());
    let r2 = refine(
        golden_input(),
        RefineOptions {
            chat: Some(boom.clone()),
            sha256: Some("rust-cache-abc123".into()),
            log: silent(),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(boom.call_count(), 0);
    assert_eq!(
        serde_json::to_value(&r2.items).unwrap(),
        serde_json::to_value(&r1.items).unwrap()
    );

    // 不同 sha256 不命中
    let mock3 = Arc::new(MockChat::new());
    refine(
        golden_input(),
        RefineOptions {
            chat: Some(mock3.clone()),
            sha256: Some("rust-cache-def456".into()),
            log: silent(),
            ..Default::default()
        },
    )
    .await;
    assert!(mock3.call_count() > 0);
}

// ── schema 透明性 ──

#[tokio::test]
async fn schema_transparent_unknown_fields_pass_through() {
    let mut input = golden_input();
    input[0].set("some_future_field", json!({ "x": 1 })); // MinerU 未来新增字段
    let frozen = input.clone();
    let r = run(input.clone(), Arc::new(MockChat::new())).await;
    assert_eq!(
        serde_json::to_value(&input).unwrap(),
        serde_json::to_value(&frozen).unwrap()
    ); // 入参零突变
    for it in &r.items {
        assert!(!it.0.contains_key("id")); // 内部稳定 ID 不外漏
    }
    assert_eq!(r.items[0].0["some_future_field"], json!({ "x": 1 }));
}

#[tokio::test]
async fn empty_or_clean_input_passes_without_llm() {
    let boom = Arc::new(ExplodingChat::new());
    let r = run(vec![], boom.clone()).await;
    assert!(r.items.is_empty());
    let clean = items_of(
        json!([{ "type": "text", "text": "干净的一句话。", "page_idx": 0, "bbox": bbox(0) }]),
    );
    let r = run(clean.clone(), boom.clone()).await;
    assert_eq!(
        serde_json::to_value(&r.items).unwrap(),
        serde_json::to_value(&clean).unwrap()
    );
    assert_eq!(boom.call_count(), 0);
}

// ── 守卫 ──

#[tokio::test]
async fn max_iterations_hard_stop_still_passes_exit_gate() {
    let r = refine(
        golden_input(),
        RefineOptions {
            chat: Some(Arc::new(MockChat::new())),
            max_iterations: Some(2),
            log: silent(),
            ..Default::default()
        },
    )
    .await;
    assert!(r.report.iterations <= 2);
    assert!(!r.report.fail_open);
}

// ── 并发容错 ──

#[tokio::test]
async fn single_suspect_failure_only_suspends_that_suspect() {
    let mut overrides: HashMap<SuspectKind, KindHandler> = HashMap::new();
    // cross_page_break 的对话永远炸；其它 kind 正常裁决
    overrides.insert(
        SuspectKind::CrossPageBreak,
        Box::new(|_, _| Err(LlmError("注入的单点故障".into()))),
    );
    let r = run(golden_input(), Arc::new(MockChat::with(overrides))).await;
    assert!(!r.report.fail_open);
    assert_eq!(
        op_counts_json(&r),
        json!({ "demote": 1, "drop": 1, "strip": 1 })
    ); // merge 缺席
    assert_eq!(r.report.dismissed, 1); // 故障疑点被搁置
}

// ── 跨页表格/列表（端到端，mock LLM）──

/// 真实形态复刻：前表(p0) + 家具 + 续表(p1) + 空壳(p2) + 拆开的 list(p2/p3)。
fn split_doc_input() -> Vec<MineruItem> {
    items_of(json!([
        {
            "type": "table",
            "table_body": "<table><tbody><tr><td>序号</td><td>事项</td></tr><tr><td>1</td><td>启动</td></tr></tbody></table>",
            "table_caption": ["表1 安排"],
            "img_path": "images/t1.jpg",
            "page_idx": 0,
            "bbox": [50, 100, 550, 800],
        },
        { "type": "page_number", "text": "1", "page_idx": 0, "bbox": bbox(820) },
        { "type": "header", "text": "页眉", "page_idx": 1, "bbox": bbox(10) },
        {
            "type": "table",
            "table_body": "<table><tbody><tr><td>2</td><td>评审</td></tr></tbody></table>",
            "table_caption": [],
            "img_path": "images/t2.jpg",
            "page_idx": 1,
            "bbox": [50, 80, 550, 300],
        },
        { "type": "table", "img_path": "", "table_caption": [], "table_footnote": [], "page_idx": 2, "bbox": bbox(80) }, // 空壳
        { "type": "list", "list_items": ["甲", "乙"], "page_idx": 2, "bbox": [50, 200, 550, 800] },
        { "type": "list", "list_items": ["丙"], "page_idx": 3, "bbox": [50, 80, 550, 160] },
    ]))
}

// split_table 仅视觉裁决：loadImage + visionFn 是 mergeTable 的唯一通路
fn load_any_image() -> Arc<dyn LoadImage> {
    Arc::new(FnLoader(|_: &str| Some(vec![9, 9, 9])))
}

fn vision_merge() -> Arc<dyn VisionClient> {
    Arc::new(FnVision(|_: &[u8], _: &[u8]| {
        Ok(verdict(true, "同一张表", 1, 1))
    }))
}

async fn run_vision(
    items: Vec<MineruItem>,
    chat: Arc<dyn ChatClient>,
    load_image: Option<Arc<dyn LoadImage>>,
    vision: Arc<dyn VisionClient>,
) -> RefineResult {
    refine(
        items,
        RefineOptions {
            chat: Some(chat),
            load_image,
            vision: Some(vision),
            log: silent(),
            ..Default::default()
        },
    )
    .await
}

#[tokio::test]
async fn merge_table_drop_husk_merge_list_full_chain() {
    let input = split_doc_input();
    let r = run_vision(
        input.clone(),
        Arc::new(MockChat::new()),
        Some(load_any_image()),
        vision_merge(),
    )
    .await;
    assert!(!r.report.fail_open);
    assert_eq!(
        op_counts_json(&r),
        json!({ "mergeTable": 1, "drop": 1, "mergeList": 1 })
    );

    let tables: Vec<&MineruItem> = r
        .items
        .iter()
        .filter(|it| it.item_type() == "table")
        .collect();
    assert_eq!(tables.len(), 1);
    assert_eq!(
        tables[0].table_body(),
        Some(
            "<table><tbody><tr><td>序号</td><td>事项</td></tr><tr><td>1</td><td>启动</td></tr><tr><td>2</td><td>评审</td></tr></tbody></table>"
        )
    );
    assert_eq!(tables[0].0["table_caption"], json!(["表1 安排"]));
    assert_eq!(tables[0].page_idx(), Some(0));

    let lists: Vec<&MineruItem> = r
        .items
        .iter()
        .filter(|it| it.item_type() == "list")
        .collect();
    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0].0["list_items"], json!(["甲", "乙", "丙"]));

    // 家具原位保留；空壳 drop 留痕
    assert_eq!(
        r.items
            .iter()
            .filter(|it| it.item_type() == "page_number" || it.item_type() == "header")
            .count(),
        2
    );
    assert!(
        r.report
            .removed_spans
            .iter()
            .any(|s| s.item_id == "it_0005" && s.text == "[table]" && s.reason == "drop")
    );

    assert!(check_char_subset(&refs(&input), &refs(&r.items)).is_ok());
    assert!(check_table_bodies(&refs(&input), &refs(&r.items)).is_ok());
}

#[tokio::test]
async fn split_doc_idempotent_with_zero_llm_calls() {
    let first = run_vision(
        split_doc_input(),
        Arc::new(MockChat::new()),
        Some(load_any_image()),
        vision_merge(),
    )
    .await;
    let second = Arc::new(MockChat::new());
    let r2 = run_vision(
        first.items.clone(),
        second.clone(),
        Some(load_any_image()),
        vision_merge(),
    )
    .await;
    assert_eq!(
        serde_json::to_value(&r2.items).unwrap(),
        serde_json::to_value(&first.items).unwrap()
    );
    assert_eq!(op_counts_json(&r2), json!({}));
    assert_eq!(second.call_count(), 0);
}

#[tokio::test]
async fn vision_dismiss_leaves_document_untouched() {
    let mut overrides: HashMap<SuspectKind, KindHandler> = HashMap::new();
    overrides.insert(
        SuspectKind::SplitList,
        Box::new(|id, _| {
            Ok((
                "dismiss".into(),
                json!({ "id": id, "reason": "两个独立列表" }),
            ))
        }),
    );
    let r = run_vision(
        split_doc_input(),
        Arc::new(MockChat::with(overrides)),
        Some(load_any_image()),
        Arc::new(FnVision(|_: &[u8], _: &[u8]| {
            Ok(verdict(false, "两张不同的表", 1, 1))
        })),
    )
    .await;
    assert!(!r.report.fail_open);
    assert_eq!(op_counts_json(&r), json!({ "drop": 1 })); // 只剩空壳被删
    assert_eq!(
        r.items
            .iter()
            .filter(|it| it.item_type() == "table")
            .count(),
        2
    );
    assert_eq!(r.report.dismissed, 2);
}

// ── split_table 视觉裁决（Qwen-VL mock）──

const PNG: [u8; 3] = [1, 2, 3];

fn vision_doc_input() -> Vec<MineruItem> {
    items_of(json!([
        {
            "type": "table",
            "table_body": "<table><tbody><tr><td>表头</td></tr><tr><td>甲</td></tr></tbody></table>",
            "table_caption": ["表1"],
            "img_path": "images/a.jpg",
            "page_idx": 0,
            "bbox": [50, 100, 550, 800],
        },
        { "type": "page_number", "text": "1", "page_idx": 0, "bbox": bbox(820) },
        {
            "type": "table",
            "table_body": "<table><tbody><tr><td>乙</td></tr></tbody></table>",
            "table_caption": [],
            "img_path": "images/b.jpg",
            "page_idx": 1,
            "bbox": [50, 80, 550, 300],
        },
    ]))
}

fn load_image_png() -> Arc<dyn LoadImage> {
    Arc::new(FnLoader(|p: &str| {
        if p.starts_with("images/") {
            Some(PNG.to_vec())
        } else {
            None
        }
    }))
}

/// split_table 不该落到文本路径：默认 MockChat 的 split_table handler 即报错（实现回归即测试失败）。
fn chat_must_not_see_split_table() -> Arc<MockChat> {
    Arc::new(MockChat::new())
}

#[tokio::test]
async fn vision_merge_lands_merge_table_with_token_accounting() {
    let vision_calls = Arc::new(AtomicU64::new(0));
    let vc = vision_calls.clone();
    let r = run_vision(
        vision_doc_input(),
        chat_must_not_see_split_table(),
        Some(load_image_png()),
        Arc::new(FnVision(move |a: &[u8], b: &[u8]| {
            vc.fetch_add(1, Ordering::Relaxed);
            assert_eq!(a, PNG);
            assert_eq!(b, PNG);
            Ok(verdict(true, "同一张表", 1500, 30))
        })),
    )
    .await;
    assert_eq!(vision_calls.load(Ordering::Relaxed), 1);
    assert!(!r.report.fail_open);
    assert_eq!(op_counts_json(&r), json!({ "mergeTable": 1 }));
    assert_eq!(
        r.items
            .iter()
            .filter(|it| it.item_type() == "table")
            .count(),
        1
    );
    assert!(
        r.items[0]
            .table_body()
            .unwrap()
            .contains("<tr><td>甲</td></tr><tr><td>乙</td></tr>")
    );
    assert_eq!(r.report.token_usage.prompt, 1500);
}

#[tokio::test]
async fn vision_dismiss_counts_into_dismissed() {
    let r = run_vision(
        vision_doc_input(),
        chat_must_not_see_split_table(),
        Some(load_image_png()),
        Arc::new(FnVision(|_: &[u8], _: &[u8]| {
            Ok(verdict(false, "两张不同的表", 1, 1))
        })),
    )
    .await;
    assert_eq!(op_counts_json(&r), json!({}));
    assert_eq!(
        r.items
            .iter()
            .filter(|it| it.item_type() == "table")
            .count(),
        2
    );
    assert_eq!(r.report.dismissed, 1);
}

#[tokio::test]
async fn missing_image_suspends_without_text_fallback() {
    let chat = chat_must_not_see_split_table();
    let r = run_vision(
        vision_doc_input(),
        chat.clone(),
        Some(Arc::new(FnLoader(|_: &str| None))),
        Arc::new(FnVision(|_: &[u8], _: &[u8]| -> Result<_, LlmError> {
            panic!("不该被调用：图都没取到")
        })),
    )
    .await;
    assert!(!r.report.fail_open);
    assert_eq!(op_counts_json(&r), json!({}));
    assert_eq!(
        r.items
            .iter()
            .filter(|it| it.item_type() == "table")
            .count(),
        2
    );
    assert_eq!(chat.call_count(), 0);
}

#[tokio::test]
async fn vision_api_failure_suspends_without_fail_open() {
    let chat = chat_must_not_see_split_table();
    let r = run_vision(
        vision_doc_input(),
        chat.clone(),
        Some(load_image_png()),
        Arc::new(FnVision(|_: &[u8], _: &[u8]| {
            Err(LlmError("Qwen-VL 不可用（测试注入）".into()))
        })),
    )
    .await;
    assert!(!r.report.fail_open);
    assert_eq!(op_counts_json(&r), json!({}));
    assert_eq!(
        r.items
            .iter()
            .filter(|it| it.item_type() == "table")
            .count(),
        2
    );
    assert_eq!(chat.call_count(), 0);
}

#[tokio::test]
async fn no_load_image_skips_split_table_entirely() {
    let chat = chat_must_not_see_split_table();
    let r = refine(
        vision_doc_input(),
        RefineOptions {
            chat: Some(chat.clone()),
            vision: Some(Arc::new(FnVision(
                |_: &[u8], _: &[u8]| -> Result<_, LlmError> {
                    panic!("不该被调用：没有 loadImage")
                },
            ))),
            log: silent(),
            ..Default::default()
        },
    )
    .await;
    assert!(!r.report.fail_open);
    assert_eq!(op_counts_json(&r), json!({}));
    assert_eq!(r.report.iterations, 0);
    assert_eq!(chat.call_count(), 0);
    assert_eq!(
        r.items
            .iter()
            .filter(|it| it.item_type() == "table")
            .count(),
        2
    ); // 两表原样保留
}

#[tokio::test]
async fn visionless_other_suspects_still_fixed() {
    // 拆表对 + 一个空壳表：空壳照常 drop，拆表跳过；输出仍残留 split_table 疑点但不 fail-open
    let mut input = vision_doc_input();
    input.push(serde_json::from_value(json!({
        "type": "table", "img_path": "", "table_caption": [], "table_footnote": [], "page_idx": 2, "bbox": bbox(80),
    })).unwrap()); // 空壳
    let r = run(input, chat_must_not_see_split_table()).await;
    assert!(!r.report.fail_open);
    assert_eq!(op_counts_json(&r), json!({ "drop": 1 }));
    assert_eq!(
        r.items
            .iter()
            .filter(|it| it.item_type() == "table")
            .count(),
        2
    );
}

// ── maxIterations 自适应默认 ──

#[test]
fn adaptive_formula() {
    assert_eq!(adaptive_max_iterations(0), 48);
    assert_eq!(adaptive_max_iterations(16), 48);
    assert_eq!(adaptive_max_iterations(17), 50);
    assert_eq!(adaptive_max_iterations(60), 136); // JZY-001 实测 60 个初始疑点 → 136，足够其 ~100 的总工作量
    assert_eq!(adaptive_max_iterations(300), 512); // 病态文档封顶
}

#[tokio::test]
async fn explicit_max_iterations_overrides_adaptive() {
    // golden 文档有 4 个疑点；maxIterations=1 应只裁 1 个就强停
    let r = refine(
        golden_input(),
        RefineOptions {
            chat: Some(Arc::new(MockChat::new())),
            max_iterations: Some(1),
            log: silent(),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(r.report.iterations, 1);
}

// 使用 NEXT_ID_RE 防未使用告警（mock 默认 handler 内部用它）
#[allow(unused)]
fn _keep(_: &regex::Regex) {
    let _ = &*NEXT_ID_RE;
}

// ── ⑨ 新探测器端到端：漏标标题 promote + 段尾节标记 split + 机械清洗入报告 ──

#[tokio::test]
async fn missed_heading_trailing_marker_and_mechanical_cleanup_end_to_end() {
    let input = items_of(json!([
        { "type": "text", "text": "4.5核心组织绩效的考核方法", "text_level": 2, "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "4.6核心组织绩效的应用", "page_idx": 0, "bbox": bbox(40) },
        { "type": "text", "text": "各部门按要求执行并存档备查[相关文件]", "page_idx": 0, "bbox": bbox(80) },
        { "type": "text", "text": "文件编号：MN-ZY-001 《战略管理规范》", "page_idx": 0, "bbox": bbox(120) },
        { "type": "table",
          "table_body": "<table><tr><td>日期</td><td>内容</td></tr><tr><td></td><td></td></tr></table>",
          "table_caption": ["更改情况"], "page_idx": 0, "bbox": bbox(160) },
    ]));
    let r = run(input, Arc::new(MockChat::new())).await;
    assert!(!r.report.fail_open);

    // 漏标标题被 promote 成与兄弟一致的 level
    assert_eq!(r.items[1].text(), Some("4.6核心组织绩效的应用"));
    assert_eq!(r.items[1].text_level(), Some(2));
    // 段尾节标记被 split 成独立块
    assert_eq!(r.items[2].text(), Some("各部门按要求执行并存档备查"));
    assert_eq!(r.items[3].text(), Some("[相关文件]"));
    // 表格尾部空行被机械清洗删除，统计并入 opCounts
    assert_eq!(
        r.items[5].table_body(),
        Some("<table><tr><td>日期</td><td>内容</td></tr></table>")
    );
    assert_eq!(
        op_counts_json(&r),
        json!({ "promote": 1, "split": 1, "mechEmptyRow": 1 })
    );
}

// ── extra_char → deleteChar 端到端 ──

#[tokio::test]
async fn extra_char_fixed_end_to_end_via_delete_char() {
    let input = items_of(json!([
        { "type": "text", "text": "基本治理理念的的变化情况。", "page_idx": 0, "bbox": bbox(0) },
    ]));
    // MockChat 默认按证据里的 offset 回 deleteChar
    let r = run(input, Arc::new(MockChat::new())).await;
    assert!(!r.report.fail_open);
    assert_eq!(r.items[0].text().unwrap(), "基本治理理念的变化情况。");
    assert_eq!(op_counts_json(&r), json!({ "deleteChar": 1 }));
    assert_eq!(r.report.removed_spans.len(), 1);
    assert_eq!(r.report.removed_spans[0].reason, "deleteChar:dup_char");
    assert_eq!(r.report.removed_spans[0].text, "的");
}

#[tokio::test]
async fn extra_char_dismissed_leaves_text_untouched() {
    let input = items_of(json!([
        { "type": "text", "text": "确保目的的实现。", "page_idx": 0, "bbox": bbox(0) },
    ]));
    let overrides: HashMap<SuspectKind, KindHandler> =
        HashMap::from([(SuspectKind::ExtraChar, dismiss_handler())]);
    let r = run(input.clone(), Arc::new(MockChat::with(overrides))).await;
    assert!(!r.report.fail_open);
    assert_eq!(
        serde_json::to_value(&r.items).unwrap(),
        serde_json::to_value(&input).unwrap()
    );
    assert_eq!(r.report.dismissed, 1);
}
