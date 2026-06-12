// 乱码表视觉重转写层（rewrite_garbled_tables）：金样本、闸门防御、整表回退、
// 层级搁置、配置早抛、缓存隔离。全程 mock 视觉客户端，不打网络。

mod common;

use common::{ExplodingChat, FnLoader, FnTableVision, items_of};
use mineru_refine::llm::{LlmError, TableTranscription, TranscribedCell, Usage, VisionClient};
use mineru_refine::{RefineOptions, cache_key_for_opts, refine};
use serde_json::json;
use std::sync::Arc;

fn cell(row: usize, col: usize, text: &str) -> TranscribedCell {
    TranscribedCell {
        row,
        col,
        text: text.to_string(),
    }
}

fn transcription(cells: Vec<TranscribedCell>) -> TableTranscription {
    TableTranscription {
        cells,
        invalid: 0,
        usage: Usage {
            prompt_tokens: 500,
            completion_tokens: 80,
        },
    }
}

/// 一张重度乱码表（覆盖率实测 ≈0.47 < 0.55）+ 一张干净表（不该被送审）。
/// 零文本疑点：核心 loop 一次 LLM 都不该打（chat 用 ExplodingChat 断言）。
fn doc_with_garbled_table() -> Vec<mineru_refine::MineruItem> {
    items_of(json!([
        {
            "type": "table",
            "table_body": "<table><tr><td>代格</td><td>目择值</td><td>比校方式</td><td>数据来酒</td></tr><tr><td>数更来潭</td><td>楼心</td><td>道术率</td><td>系计数据</td></tr><tr><td>合格军</td><td>提交方式</td><td>测试合格率</td><td>81.36%</td><td></td></tr></table>",
            "table_caption": ["表1 绩效指标定义"],
            "img_path": "images/garbled.jpg",
            "page_idx": 0,
            "bbox": [50, 40, 550, 200],
        },
        {
            "type": "table",
            "table_body": "<table><tr><td>指标名称</td><td>数据来源</td><td>比较方式</td></tr><tr><td>测试合格率</td><td>累计数据</td><td>提交方式</td></tr></table>",
            "table_caption": ["表2 干净表"],
            "img_path": "images/clean.jpg",
            "page_idx": 0,
            "bbox": [50, 240, 550, 400],
        },
    ]))
}

/// 金样本修正：把 ZBZ-047 实测病例改回正写（覆盖率 0.47 → 0.89）。
fn golden_cells() -> Vec<TranscribedCell> {
    vec![
        cell(0, 0, "代码"),
        cell(0, 1, "目标值"),
        cell(0, 2, "比较方式"),
        cell(0, 3, "数据来源"),
        cell(1, 0, "数据来源"),
        cell(1, 1, "核心"),
        cell(1, 2, "直通率"),
        cell(1, 3, "累计数据"),
        cell(2, 0, "合格率"),
    ]
}

fn loader_with_images() -> Arc<dyn mineru_refine::LoadImage> {
    Arc::new(FnLoader(|path: &str| {
        path.starts_with("images/").then(|| vec![0xFF, 0xD8, 0xFF])
    }))
}

fn run<V: VisionClient + 'static>(
    items: Vec<mineru_refine::MineruItem>,
    vision: Arc<V>,
    opts: RefineOptions,
) -> mineru_refine::RefineResult {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(refine(
        items,
        RefineOptions {
            chat: Some(Arc::new(ExplodingChat::new())),
            vision: Some(vision),
            log: Some(Arc::new(|_| {})),
            ..opts
        },
    ))
}

fn on() -> RefineOptions {
    RefineOptions {
        rewrite_garbled_tables: true,
        load_image: Some(loader_with_images()),
        ..RefineOptions::default()
    }
}

// ── 金样本 ──

#[test]
fn golden_garbled_table_rewritten_end_to_end() {
    let vision = Arc::new(FnTableVision::new(|_: &[u8], render: &str| {
        // 送审渲染必须带行列坐标系与当前乱码内容
        assert!(render.contains("第0行："));
        assert!(render.contains("「数据来酒」"));
        Ok(transcription(golden_cells()))
    }));
    let result = run(doc_with_garbled_table(), vision.clone(), on());

    assert!(!result.report.fail_open);
    assert_eq!(vision.call_count(), 1, "只有乱码表送审，干净表不送");

    let body = result.items[0].table_body().unwrap();
    for fixed in [
        "代码",
        "目标值",
        "比较方式",
        "数据来源",
        "核心",
        "直通率",
        "累计数据",
        "合格率",
    ] {
        assert!(body.contains(fixed), "重转写结果应含「{fixed}」: {body}");
    }
    for wrong in ["代格", "目择值", "来酒", "楼心", "合格军"] {
        assert!(!body.contains(wrong), "乱码「{wrong}」应已被替换: {body}");
    }
    // 标签骨架原样，未提案的格不动
    assert!(body.contains("<table><tr><td>"));
    assert!(body.contains("提交方式") && body.contains("81.36%"));
    // 干净表逐字节不动
    assert_eq!(
        result.items[1].table_body().unwrap(),
        doc_with_garbled_table()[1].table_body().unwrap()
    );

    // report：9 条整格替换，含撤销凭据
    assert_eq!(result.report.table_rewrites.len(), 9);
    assert_eq!(result.report.table_rewrite_rejected, 0);
    let first = &result.report.table_rewrites[0];
    assert_eq!((first.row, first.col), (0, 0));
    assert_eq!(
        (first.before.as_str(), first.after.as_str()),
        ("代格", "代码")
    );
    // token 计入总账
    assert_eq!(result.report.token_usage.prompt, 500);
    assert_eq!(result.report.token_usage.completion, 80);

    // provenance：每条 rewriteCell 的字符区间在新 table_body 中精确指向 after
    assert_eq!(result.provenance.len(), 9);
    let body_chars: Vec<char> = body.chars().collect();
    for (p, fix) in result.provenance.iter().zip(&result.report.table_rewrites) {
        assert_eq!(p.origin, "garbled_table");
        assert_eq!(p.op, "rewriteCell");
        assert_eq!(p.field, "table_body");
        let span: String = body_chars[p.char_start..p.char_end].iter().collect();
        assert_eq!(span, fix.after);
    }
}

// ── 闸门防御 ──

#[test]
fn structural_gates_reject_bad_proposals() {
    let vision = Arc::new(FnTableVision::new(|_: &[u8], _: &str| {
        let mut cells = golden_cells();
        cells.push(cell(9, 9, "不存在的格")); // 行列越界
        cells.push(cell(2, 1, "<b>提交方式</b>")); // 标签注入
        cells.push(cell(0, 0, "代号")); // 同格重复提案（只认第一条）
        Ok(transcription(cells))
    }));
    let result = run(doc_with_garbled_table(), vision, on());

    assert!(!result.report.fail_open);
    assert_eq!(result.report.table_rewrites.len(), 9, "合法提案照常落地");
    assert_eq!(result.report.table_rewrite_rejected, 3);
    let body = result.items[0].table_body().unwrap();
    assert!(body.contains("代码") && !body.contains("<b>"));
}

#[test]
fn misalignment_gates_reject_cell_shuffling() {
    // 实测病例：视觉模型在宽乱码表上行列错位，把别格内容张冠李戴过来
    let vision = Arc::new(FnTableVision::new(|_: &[u8], _: &str| {
        Ok(transcription(vec![
            cell(1, 1, "73.21%"),                         // 文字格被改成纯数值
            cell(0, 1, "目标值的的确确超长十四个字符啦"), // 长度量级不可比（3 字 → 14 字）
            cell(2, 1, "提交方法"),                       // 原格是正常内容，无资格
            cell(2, 3, "OK"),                             // 原格是纯数值，无资格
            cell(2, 4, "凭空填上内容"),                   // 原格是空格，无资格
        ]))
    }));
    let input = doc_with_garbled_table();
    let result = run(input.clone(), vision, on());

    assert!(!result.report.fail_open);
    assert!(result.report.table_rewrites.is_empty());
    assert_eq!(result.report.table_rewrite_rejected, 5);
    assert_eq!(
        result.items[0].table_body().unwrap(),
        input[0].table_body().unwrap(),
        "全部错位提案被拒：table_body 逐字节不动"
    );
}

#[test]
fn coverage_regression_reverts_whole_table() {
    // 把可重转写的乱码格"修复"成更不成词的鬼画符 → 覆盖率不升反降，整表回退
    let vision = Arc::new(FnTableVision::new(|_: &[u8], _: &str| {
        Ok(transcription(vec![
            cell(2, 0, "鬲圭彐殳"),
            cell(1, 1, "亍弋"),
        ]))
    }));
    let input = doc_with_garbled_table();
    let result = run(input.clone(), vision, on());

    assert!(!result.report.fail_open);
    assert!(result.report.table_rewrites.is_empty());
    assert_eq!(result.report.table_rewrite_rejected, 2);
    assert_eq!(
        result.items[0].table_body().unwrap(),
        input[0].table_body().unwrap(),
        "整表回退：table_body 逐字节不动"
    );
    assert!(result.provenance.is_empty());
}

// ── 层级搁置与配置 ──

#[test]
fn flag_off_never_calls_vision() {
    let vision = Arc::new(FnTableVision::new(|_: &[u8], _: &str| {
        Ok(transcription(golden_cells()))
    }));
    let input = doc_with_garbled_table();
    let result = run(
        input.clone(),
        vision.clone(),
        RefineOptions {
            load_image: Some(loader_with_images()),
            ..RefineOptions::default()
        },
    );

    assert!(!result.report.fail_open);
    assert_eq!(vision.call_count(), 0);
    assert!(result.report.table_rewrites.is_empty());
    assert_eq!(result.items[0].table_body(), input[0].table_body());
}

#[test]
fn flag_without_image_source_fails_open() {
    let vision = Arc::new(FnTableVision::new(|_: &[u8], _: &str| {
        Ok(transcription(golden_cells()))
    }));
    let input = doc_with_garbled_table();
    let result = run(
        input.clone(),
        vision,
        RefineOptions {
            rewrite_garbled_tables: true, // 但没给 image_dir / load_image
            ..RefineOptions::default()
        },
    );

    assert!(result.report.fail_open, "配置错误必须大声失败，不静默跳过");
    assert_eq!(result.items, input);
}

#[test]
fn vision_failure_shelves_table() {
    let vision = Arc::new(FnTableVision::new(|_: &[u8], _: &str| {
        Err(LlmError("视觉服务不可用（测试注入）".into()))
    }));
    let input = doc_with_garbled_table();
    let result = run(input.clone(), vision, on());

    assert!(!result.report.fail_open, "单表搁置不毁全局");
    assert!(result.report.table_rewrites.is_empty());
    assert_eq!(result.items[0].table_body(), input[0].table_body());
}

#[test]
fn missing_image_shelves_table() {
    let vision = Arc::new(FnTableVision::new(|_: &[u8], _: &str| {
        Ok(transcription(golden_cells()))
    }));
    let input = doc_with_garbled_table();
    let result = run(
        input.clone(),
        vision.clone(),
        RefineOptions {
            rewrite_garbled_tables: true,
            load_image: Some(Arc::new(FnLoader(|_: &str| None))),
            ..RefineOptions::default()
        },
    );

    assert!(!result.report.fail_open);
    assert_eq!(vision.call_count(), 0, "取不到图就不送审");
    assert_eq!(result.items[0].table_body(), input[0].table_body());
}

// ── 缓存隔离 ──

#[test]
fn cache_key_isolates_garbled_flag() {
    let off = RefineOptions::default();
    let on = RefineOptions {
        rewrite_garbled_tables: true,
        ..RefineOptions::default()
    };
    let both = RefineOptions {
        rewrite_garbled_tables: true,
        fix_ocr_confusion: true,
        ..RefineOptions::default()
    };
    let k_off = cache_key_for_opts("abc", &off);
    let k_on = cache_key_for_opts("abc", &on);
    let k_both = cache_key_for_opts("abc", &both);
    assert_ne!(k_off, k_on);
    assert_ne!(k_on, k_both);
    assert!(k_on.contains("garbled-"));
}

// ── 真实数据标定（test_data 不进 git，本地手动跑：cargo test -- --ignored garbled）──

#[test]
#[ignore = "需要本地 test_data/（gitignored）"]
fn calibration_on_real_documents() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_data/mineru");
    let mut garbled_found = 0;
    for doc in std::fs::read_dir(&root).expect("test_data/mineru 不存在") {
        let path = doc.unwrap().path().join("content_list.json");
        let items: Vec<mineru_refine::MineruItem> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for it in &items {
            let Some(tb) = it.table_body() else { continue };
            let detected = mineru_refine::garbled::detect_garbled_table(tb);
            let is_known_garbled = tb.contains("数据来酒");
            if is_known_garbled {
                garbled_found += 1;
                assert!(detected.is_some(), "已知乱码表必须被检出");
            } else {
                assert!(detected.is_none(), "正常表误报（{path:?}）：{:?}", detected);
            }
        }
    }
    assert_eq!(garbled_found, 1, "标定语料里应恰有一张已知乱码表");
}
