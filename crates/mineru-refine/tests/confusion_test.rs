// 混淆修正层（fix_ocr_confusion）：金样本、反例、越界防御、密度上限、
// 层级 fail-open、缓存隔离。全程 mock LLM，不打网络。

mod common;

use common::{FnChat, first_user_content, items_of, tool_reply};
use mineru_refine::llm::{ChatClient, ChatResult, LlmError, Message};
use mineru_refine::{CONFUSION_PROMPT_VERSION, RefineOptions, refine};
use regex::Regex;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

// ── 测试基建 ──

static CAND_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^候选(\d+)（字符「(.)」）：(.*)$").unwrap());
static VERIFY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"标出的「(.)」改成「(.)」").unwrap());

fn tool_name(tools: &Value) -> &str {
    tools
        .pointer("/0/function/name")
        .and_then(Value::as_str)
        .unwrap_or("")
}

type Decide = Arc<dyn Fn(char, &str) -> Option<(char, String)> + Send + Sync>;
type Verify = Arc<dyn Fn(char, char) -> bool + Send + Sync>;

/// 混淆层专用 mock：judgeConfusions 调用按 decide 闭包逐候选裁决，
/// verifyConfusion 调用按 verify 闭包给 approve/reject。其余调用直接报错
///（核心 loop 不该在这些测试里打 LLM——fixture 都是零疑点文档）。
/// 调用计数走 FnChat 自带的 call_count。
#[allow(clippy::type_complexity)]
fn confusion_chat(
    decide: Decide,
    verify: Verify,
) -> Arc<FnChat<impl Fn(&[Message], &Value) -> Result<ChatResult, LlmError> + Send + Sync>> {
    let call_id = Arc::new(AtomicU64::new(0));
    Arc::new(FnChat::new(
        move |messages: &[Message], tools: &Value| -> Result<ChatResult, LlmError> {
            let n = call_id.fetch_add(1, Ordering::Relaxed) + 1;
            let content = first_user_content(messages);
            match tool_name(tools) {
                "judgeConfusions" => {
                    let verdicts: Vec<Value> = CAND_RE
                        .captures_iter(content)
                        .map(|c| {
                            let index: u64 = c[1].parse().unwrap();
                            let ch = c[2].chars().next().unwrap();
                            match decide(ch, &c[3]) {
                                Some((after, reason)) => json!({
                                    "index": index, "action": "replace",
                                    "replaceWith": after.to_string(), "reason": reason,
                                }),
                                None => json!({ "index": index, "action": "keep" }),
                            }
                        })
                        .collect();
                    Ok(tool_reply(
                        n,
                        "judgeConfusions",
                        json!({ "verdicts": verdicts }).to_string(),
                    ))
                }
                "verifyConfusion" => {
                    let caps = VERIFY_RE
                        .captures(content)
                        .ok_or_else(|| LlmError("mock 解析不了二次裁决 prompt".into()))?;
                    let before = caps[1].chars().next().unwrap();
                    let after = caps[2].chars().next().unwrap();
                    let v = if verify(before, after) {
                        "approve"
                    } else {
                        "reject"
                    };
                    Ok(tool_reply(
                        n,
                        "verifyConfusion",
                        json!({ "verdict": v, "reason": "mock 审查" }).to_string(),
                    ))
                }
                other => Err(LlmError(format!("mock 不期望的调用: {other}"))),
            }
        },
    ))
}

fn keep_all() -> Decide {
    Arc::new(|_, _| None)
}

fn reject_all() -> Verify {
    Arc::new(|_, _| false)
}

/// 零疑点文档 + 全套混淆病例（核心 loop 一次 LLM 都不该打）。
fn confused_doc() -> Vec<mineru_refine::MineruItem> {
    items_of(json!([
        { "type": "text", "text": "公司CE0办公室启用0A系统。", "page_idx": 0, "bbox": [50, 40, 550, 60] },
        { "type": "text", "text": "当入=n时模型收敛。", "page_idx": 0, "bbox": [50, 80, 550, 100] },
        { "type": "text", "text": "公司在竟争中保持优势。", "page_idx": 0, "bbox": [50, 120, 550, 140] },
        { "type": "text", "text": "市场份额达到B1.36%。", "page_idx": 0, "bbox": [50, 160, 550, 180] },
        { "type": "list", "list_items": ["第一条 0GSMT 平台上线。"], "page_idx": 0, "bbox": [50, 200, 550, 220] },
        {
            "type": "table",
            "table_body": "<table><tr><td>CE0</td><td>姓名</td></tr></table>",
            "table_caption": ["表1 CE0 高管名单"],
            "page_idx": 0,
            "bbox": [50, 240, 550, 320],
        },
    ]))
}

/// 金样本的裁决脚本：只修五类真实病例，其余候选（数字、型号里的 S/G/T…）一律 keep。
fn golden_decide() -> Decide {
    Arc::new(|ch, context| {
        let fix = |after: char| Some((after, format!("OCR 形近误认 {ch}→{after}")));
        match ch {
            '0' if context.contains("CE«0»") => fix('O'),
            '0' if context.contains("«0»A系统") => fix('O'),
            '0' if context.contains("«0»GSMT") => fix('O'),
            '入' if context.contains("当«入»=n") => fix('λ'),
            '竟' if context.contains("«竟»争") => fix('竞'),
            'B' if context.contains("«B»1.36%") => fix('8'),
            _ => None,
        }
    })
}

fn run(
    items: Vec<mineru_refine::MineruItem>,
    chat: Arc<dyn ChatClient>,
    opts: RefineOptions,
) -> mineru_refine::RefineResult {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(refine(
        items,
        RefineOptions {
            chat: Some(chat),
            log: Some(Arc::new(|_| {})),
            ..opts
        },
    ))
}

fn on() -> RefineOptions {
    RefineOptions {
        fix_ocr_confusion: true,
        ..RefineOptions::default()
    }
}

// ── 金样本 ──

#[test]
fn golden_confusions_fixed_end_to_end() {
    let input = confused_doc();
    let result = run(input, confusion_chat(golden_decide(), reject_all()), on());

    assert!(!result.report.fail_open);
    let texts: Vec<&str> = result.items.iter().filter_map(|i| i.text()).collect();
    assert_eq!(texts[0], "公司CEO办公室启用OA系统。");
    assert_eq!(texts[1], "当λ=n时模型收敛。");
    assert_eq!(texts[2], "公司在竞争中保持优势。");
    assert_eq!(texts[3], "市场份额达到81.36%。");
    assert_eq!(
        result.items[4].str_array("list_items").unwrap(),
        vec!["第一条 OGSMT 平台上线。"]
    );
    // Phase 2：单元格文本照修，标签骨架逐字节原样；caption 照修
    assert_eq!(
        result.items[5].table_body().unwrap(),
        "<table><tr><td>CEO</td><td>姓名</td></tr></table>"
    );
    assert_eq!(
        result.items[5].str_array("table_caption").unwrap(),
        vec!["表1 CEO 高管名单"]
    );

    // 8 条替换，文档序，全部表内直落，report/provenance 一一对应
    let fixes = &result.report.confusion_fixes;
    assert_eq!(fixes.len(), 8);
    assert!(fixes.iter().all(|f| f.source == "table"));
    assert_eq!(
        fixes
            .iter()
            .map(|f| (f.before.as_str(), f.after.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("0", "O"),
            ("0", "O"),
            ("入", "λ"),
            ("竟", "竞"),
            ("B", "8"),
            ("0", "O"),
            ("0", "O"),
            ("0", "O"),
        ]
    );
    assert_eq!(fixes[5].field, "list_items[0]");
    assert_eq!(fixes[6].field, "table_caption[0]");
    // table_body 的 charOffset 是整个 HTML 字符串内的字符偏移（"<table><tr><td>CE0" 的 0 在 17）
    assert_eq!(fixes[7].field, "table_body");
    assert_eq!(fixes[7].char_offset, 17);
    assert_eq!(result.provenance.len(), 8);
    assert!(
        result
            .provenance
            .iter()
            .all(|p| p.origin == "ocr_confusion" && p.op == "fixConfusion")
    );
    assert_eq!(result.report.confusion_rejected, 0);
    assert!(result.report.token_usage.prompt > 0);
}

// ── 反例：LLM 全 keep → 逐字节不动 ──

#[test]
fn keep_verdicts_leave_document_untouched() {
    let input = confused_doc();
    let result = run(
        input.clone(),
        confusion_chat(keep_all(), reject_all()),
        on(),
    );
    assert_eq!(result.items, input);
    assert!(result.report.confusion_fixes.is_empty());
    assert!(result.provenance.is_empty());
}

// ── flag 关：零混淆调用，输出与现版本逐字节一致 ──

#[test]
fn flag_off_means_no_confusion_calls_and_clean_report() {
    let input = confused_doc();
    let chat = confusion_chat(golden_decide(), reject_all());
    let result = run(input.clone(), chat.clone(), RefineOptions::default());
    assert_eq!(chat.call_count(), 0, "flag 关不该有任何混淆层调用");
    assert_eq!(result.items, input);
    // 序列化兼容：关 flag 时 report 不长出任何 confusion 字段
    let report = serde_json::to_value(&result.report).unwrap();
    let keys: Vec<&str> = report
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert!(
        !keys.iter().any(|k| k.starts_with("confusion")),
        "多余字段: {keys:?}"
    );
}

// ── 表外提案：二次裁决两分支 ──

#[test]
fn out_of_table_proposal_lands_only_after_second_opinion() {
    let doc = || {
        items_of(json!([
            { "type": "text", "text": "编号0文件已存档备查。", "page_idx": 0, "bbox": [50, 40, 550, 60] },
        ]))
    };
    // 0→D 不在混淆表内
    let decide: Decide = Arc::new(|ch, _| (ch == '0').then(|| ('D', "表外提案".into())));

    let approved = run(
        doc(),
        confusion_chat(decide.clone(), Arc::new(|_, _| true)),
        on(),
    );
    assert_eq!(approved.items[0].text().unwrap(), "编号D文件已存档备查。");
    assert_eq!(approved.report.confusion_fixes.len(), 1);
    assert_eq!(approved.report.confusion_fixes[0].source, "second_opinion");

    let rejected = run(doc(), confusion_chat(decide, reject_all()), on());
    assert_eq!(rejected.items[0].text().unwrap(), "编号0文件已存档备查。");
    assert!(rejected.report.confusion_fixes.is_empty());
    assert_eq!(rejected.report.confusion_rejected, 1);
}

// ── extraConfusionPairs：用户补充对走表内直落 ──

#[test]
fn extra_pair_admits_directly_without_second_opinion() {
    let doc = items_of(json!([
        { "type": "text", "text": "编号0文件已存档备查。", "page_idx": 0, "bbox": [50, 40, 550, 60] },
    ]));
    let decide: Decide = Arc::new(|ch, _| (ch == '0').then(|| ('D', "补充对".into())));
    let result = run(
        doc,
        // verify 永远 reject——若走了二次裁决，这条就落不了地
        confusion_chat(decide, reject_all()),
        RefineOptions {
            extra_confusion_pairs: vec!["0D".into()],
            ..on()
        },
    );
    assert_eq!(result.items[0].text().unwrap(), "编号D文件已存档备查。");
    assert_eq!(result.report.confusion_fixes[0].source, "table");
}

// ── 越界防御：结构非法的提案在解析期被拒 ──

#[test]
fn malformed_replacements_rejected_and_counted() {
    let doc = items_of(json!([
        { "type": "text", "text": "编号0与编号l已登记。", "page_idx": 0, "bbox": [50, 40, 550, 60] },
    ]));
    // 手写回复：候选0 给多字符 replaceWith，候选1 缺 replaceWith —— 都必须被拒
    let chat: Arc<dyn ChatClient> = Arc::new(FnChat::new(
        |_messages: &[Message], tools: &Value| -> Result<ChatResult, LlmError> {
            assert_eq!(tool_name(tools), "judgeConfusions");
            Ok(tool_reply(
                1,
                "judgeConfusions",
                json!({ "verdicts": [
                    { "index": 0, "action": "replace", "replaceWith": "OO", "reason": "越界" },
                    { "index": 1, "action": "replace", "reason": "缺字符" },
                ]})
                .to_string(),
            ))
        },
    ));
    let result = run(doc.clone(), chat, on());
    assert_eq!(result.items, doc);
    assert!(result.report.confusion_fixes.is_empty());
    assert_eq!(result.report.confusion_rejected, 2);
}

// ── 密度闸门：单元内提案超稀疏上限 → 整单元拒绝 ──

#[test]
fn density_cap_rejects_whole_unit() {
    let doc = items_of(json!([
        { "type": "text", "text": "0甲0乙0丙0丁0卯。", "page_idx": 0, "bbox": [50, 40, 550, 60] },
    ]));
    let decide: Decide = Arc::new(|ch, _| (ch == '0').then(|| ('O', "全换".into())));
    let result = run(doc.clone(), confusion_chat(decide, reject_all()), on());
    assert_eq!(result.items, doc, "密度超标必须整单元回绝");
    assert!(result.report.confusion_fixes.is_empty());
    assert_eq!(result.report.confusion_rejected, 5);
}

// ── 层级 fail-open：LLM 故障只搁置，panic 只丢层，核心产物不受累 ──

#[test]
fn llm_error_shelves_batch_without_fail_open() {
    let doc = confused_doc();
    let chat: Arc<dyn ChatClient> = Arc::new(FnChat::new(
        |_: &[Message], _: &Value| -> Result<ChatResult, LlmError> {
            Err(LlmError("混淆裁决全挂（测试注入）".into()))
        },
    ));
    let result = run(doc.clone(), chat, on());
    assert!(!result.report.fail_open, "混淆层故障不许拖核心进 fail-open");
    assert_eq!(result.items, doc);
    assert!(result.report.confusion_fixes.is_empty());
}

#[test]
fn layer_panic_discards_layer_keeps_core_result() {
    let doc = confused_doc();
    let chat: Arc<dyn ChatClient> = Arc::new(FnChat::new(
        |_: &[Message], _: &Value| -> Result<ChatResult, LlmError> {
            panic!("混淆层内部炸了（测试注入）")
        },
    ));
    let result = run(doc.clone(), chat, on());
    assert!(!result.report.fail_open);
    assert_eq!(result.items, doc, "panic 时整层丢弃，核心产物原样");
    assert!(result.report.confusion_fixes.is_empty());
}

// ── 配置错误：早抛 → fail-open + 大声 log，不静默吞 ──

#[test]
fn invalid_extra_pair_fails_open_loudly() {
    let doc = confused_doc();
    let result = run(
        doc.clone(),
        confusion_chat(keep_all(), reject_all()),
        RefineOptions {
            extra_confusion_pairs: vec!["abc".into()],
            ..on()
        },
    );
    assert!(result.report.fail_open);
    assert_eq!(result.items, doc);
}

// ── 缓存隔离：同 sha256 不同 flag 绝不互相污染 ──

#[test]
fn cache_isolated_between_flag_on_and_off() {
    let off =
        mineru_refine::cache_key_for_opts("confusion-cache-isolation-test", &Default::default());
    let on = mineru_refine::cache_key_for_opts("confusion-cache-isolation-test", &on());
    assert_ne!(off, on);
    assert!(on.contains(&format!(":confusion-{CONFUSION_PROMPT_VERSION}:")));
}

// ════ Phase 2：table_body ════

// ── 标签骨架对候选漏斗不可见：纯标记候选字符（colspan=1）不触发任何调用 ──

#[test]
fn markup_chars_are_never_candidates() {
    let doc = items_of(json!([
        {
            "type": "table",
            "table_body": "<table><tr><td rowspan=1 colspan=1>甲乙</td></tr></table>",
            "page_idx": 0,
            "bbox": [50, 40, 550, 120],
        },
    ]));
    let chat = confusion_chat(golden_decide(), reject_all());
    let result = run(doc.clone(), chat.clone(), on());
    assert_eq!(chat.call_count(), 0, "标记里的 1 不许成为候选");
    assert_eq!(result.items, doc);
}

// ── HTML 实体当黑盒：&#80; 里的 8/0 不扫描 ──

#[test]
fn html_entities_are_opaque() {
    let doc = items_of(json!([
        {
            "type": "table",
            "table_body": "<table><tr><td>&#80;</td><td>&amp;</td></tr></table>",
            "page_idx": 0,
            "bbox": [50, 40, 550, 120],
        },
    ]));
    let chat = confusion_chat(golden_decide(), reject_all());
    let result = run(doc.clone(), chat.clone(), on());
    assert_eq!(chat.call_count(), 0, "实体内字符不许成为候选");
    assert_eq!(result.items, doc);
}

// ── 每表聚合密度：单格各自合规但整表提案过多 → 整表拒绝（乱码表防线）──

#[test]
fn table_aggregate_density_rejects_whole_table() {
    let doc = items_of(json!([
        {
            "type": "table",
            "table_body": "<table><tr><td>甲0</td><td>乙0</td><td>丙0</td>\
                           <td>丁0</td><td>卯0</td><td>辰0</td></tr></table>",
            "page_idx": 0,
            "bbox": [50, 40, 550, 120],
        },
    ]));
    let decide: Decide = Arc::new(|ch, _| (ch == '0').then(|| ('O', "全换".into())));
    let result = run(doc.clone(), confusion_chat(decide, reject_all()), on());
    // 单格 1 条提案都没超 max(2,3%)，但整表 6 条 > max(4, 2%·12) = 4 → 全拒
    assert_eq!(result.items, doc, "聚合密度超标必须整表回绝");
    assert!(result.report.confusion_fixes.is_empty());
    assert_eq!(result.report.confusion_rejected, 6);
}

// ── 表格候选的行列结构化上下文：标题 / 表头 / 所在行（第N列）──

#[test]
fn table_candidates_get_structured_context() {
    let doc = items_of(json!([
        {
            "type": "table",
            "table_body": "<table><tr><td>岗位</td><td>考核人</td></tr>\
                           <tr><td>CE0</td><td>董事会</td></tr></table>",
            "table_caption": ["表3 考核责任人"],
            "page_idx": 0,
            "bbox": [50, 40, 550, 120],
        },
    ]));
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(vec![]));
    let seen2 = seen.clone();
    let decide: Decide = Arc::new(move |ch, context| {
        if ch == '0' {
            seen2.lock().unwrap().push(context.to_string());
            return Some(('O', "CEO 误认".into()));
        }
        None
    });
    let result = run(doc, confusion_chat(decide, reject_all()), on());
    assert!(
        result.items[0]
            .table_body()
            .unwrap()
            .contains("<td>CEO</td>")
    );
    let contexts = seen.lock().unwrap();
    assert_eq!(contexts.len(), 1);
    let ctx = &contexts[0];
    for needle in [
        "标题：表3 考核责任人",
        "表头：岗位｜考核人",
        "CE«0»",
        "（第1列）",
    ] {
        assert!(ctx.contains(needle), "上下文缺「{needle}」: {ctx}");
    }
}

// ── 实证扩充的白名单对（扞↔杆）走表内直落 ──

#[test]
fn evidence_backed_pairs_are_in_builtin_whitelist() {
    let doc = items_of(json!([
        { "type": "text", "text": "从业界标扞入手分析。", "page_idx": 0, "bbox": [50, 40, 550, 60] },
    ]));
    let decide: Decide = Arc::new(|ch, _| (ch == '扞').then(|| ('杆', "标杆误认".into())));
    let result = run(
        doc,
        // verify 永远 reject——若走了二次裁决这条就落不了地，证明 扞杆 已在内置表
        confusion_chat(decide, reject_all()),
        on(),
    );
    assert_eq!(result.items[0].text().unwrap(), "从业界标杆入手分析。");
    assert_eq!(result.report.confusion_fixes[0].source, "table");
}

// ════ Phase 3：形近字扩充 + 频率投票 + observations 闭环 ════

// ── c3 扩充的中文形近对（校较）走表内直落，且少数派写法附频率投票注记 ──

#[test]
fn c3_pairs_in_table_and_minority_gets_vote_note() {
    let doc = items_of(json!([
        { "type": "text", "text": "比较方式甲", "page_idx": 0, "bbox": [50, 40, 550, 60] },
        { "type": "text", "text": "比较方式乙", "page_idx": 0, "bbox": [50, 80, 550, 100] },
        { "type": "text", "text": "比较方式丙", "page_idx": 0, "bbox": [50, 120, 550, 140] },
        { "type": "text", "text": "比校方式丁", "page_idx": 0, "bbox": [50, 160, 550, 180] },
    ]));
    // 只在带「频率投票」注记时才修：证明少数派候选确实携带了注记
    let decide: Decide = Arc::new(|ch, ctx| {
        (ch == '校' && ctx.contains("频率投票") && ctx.contains("「比较」×3"))
            .then(|| ('较', "少数派写法".into()))
    });
    let result = run(doc, confusion_chat(decide, reject_all()), on());
    assert_eq!(result.items[3].text().unwrap(), "比较方式丁");
    assert_eq!(result.report.confusion_fixes.len(), 1);
    // 校↔较 在 c3 内置表内 → 免二次裁决直落（verify 永远 reject 证明没走那条路）
    assert_eq!(result.report.confusion_fixes[0].source, "table");
}

// ── 频率加白（排误）：全文一致的高频写法跳过送审，零调用 ──

#[test]
fn consistent_frequent_word_is_whitelisted_no_calls() {
    let items: Vec<serde_json::Value> = (0..5)
        .map(|i| {
            json!({ "type": "text", "text": "学校管理制度", "page_idx": 0,
                    "bbox": [50, 40 + i * 40, 550, 60 + i * 40] })
        })
        .collect();
    let doc = items_of(Value::Array(items));
    let chat = confusion_chat(
        Arc::new(|ch, _| (ch == '校').then(|| ('较', "不该被问到".into()))),
        reject_all(),
    );
    let result = run(doc.clone(), chat.clone(), on());
    assert_eq!(
        chat.call_count(),
        0,
        "「学校」×5 且无「学较」变体 → 全部加白，不该有任何调用"
    );
    assert_eq!(result.items, doc);
}

// ── 拉丁 token 频率投票：少数派换位写法生成定点候选，命中多数派免二次裁决 ──

#[test]
fn latin_token_vote_fixes_transposition_as_frequency_vote() {
    let doc = items_of(json!([
        { "type": "text", "text": "平台 OGSMT 上线", "page_idx": 0, "bbox": [50, 40, 550, 60] },
        { "type": "text", "text": "OGSMT 模块联调", "page_idx": 0, "bbox": [50, 80, 550, 100] },
        { "type": "text", "text": "OGSMT 验收通过", "page_idx": 0, "bbox": [50, 120, 550, 140] },
        { "type": "text", "text": "OGSMT 培训完成", "page_idx": 0, "bbox": [50, 160, 550, 180] },
        { "type": "text", "text": "OGSTM 运维移交", "page_idx": 0, "bbox": [50, 200, 550, 220] },
    ]));
    let decide: Decide = Arc::new(|ch, ctx| {
        if !ctx.contains("频率投票") {
            return None; // O/G/S 的常规类内候选一律 keep
        }
        match ch {
            'T' => Some(('M', "OGSMT 多数派".into())),
            'M' => Some(('T', "OGSMT 多数派".into())),
            _ => None,
        }
    });
    // verify 永远 reject：投票命中多数派的修复必须免二次裁决
    let result = run(doc, confusion_chat(decide, reject_all()), on());
    assert_eq!(result.items[4].text().unwrap(), "OGSMT 运维移交");
    let fixes = &result.report.confusion_fixes;
    assert_eq!(fixes.len(), 2);
    assert!(fixes.iter().all(|f| f.source == "frequency_vote"));
    assert_eq!(
        fixes
            .iter()
            .map(|f| (f.before.as_str(), f.after.as_str()))
            .collect::<Vec<_>>(),
        vec![("T", "M"), ("M", "T")]
    );
}

// ── observations 闭环：「X 应为 Y」回灌成定点候选，二轮裁决 + 二次审查后落地；
//    第二轮的 observations 只记录不再回灌（防循环）──

#[test]
fn observation_feedback_generates_second_round_then_stops() {
    let doc = items_of(json!([
        { "type": "text", "text": "公司在竟争中保持优势。", "page_idx": 0, "bbox": [50, 40, 550, 60] },
        { "type": "text", "text": "数据来潭统计表", "page_idx": 0, "bbox": [50, 80, 550, 100] },
    ]));
    let judge_calls = Arc::new(AtomicU64::new(0));
    let jc = judge_calls.clone();
    let call_id = Arc::new(AtomicU64::new(0));
    let chat: Arc<dyn ChatClient> = Arc::new(FnChat::new(
        move |messages: &[Message], tools: &Value| -> Result<ChatResult, LlmError> {
            let n = call_id.fetch_add(1, Ordering::Relaxed) + 1;
            let content = first_user_content(messages);
            match tools.pointer("/0/function/name").and_then(Value::as_str) {
                Some("judgeConfusions") => {
                    let round = jc.fetch_add(1, Ordering::Relaxed) + 1;
                    let verdicts: Vec<Value> = CAND_RE
                        .captures_iter(content)
                        .map(|c| {
                            let index: u64 = c[1].parse().unwrap();
                            if &c[2] == "潭" {
                                assert!(
                                    c[3].contains("前轮观察"),
                                    "回灌候选应带前轮观察注记: {}",
                                    &c[3]
                                );
                                json!({ "index": index, "action": "replace",
                                        "replaceWith": "源", "reason": "来源误认" })
                            } else {
                                json!({ "index": index, "action": "keep" })
                            }
                        })
                        .collect();
                    // 两轮都报 observations：第二轮的必须只记录、不再触发第三轮
                    let obs = if round == 1 {
                        json!(["表格中「数据来潭」应为「数据来源」"])
                    } else {
                        json!(["「保持优」应为「保持忧」"])
                    };
                    Ok(tool_reply(
                        n,
                        "judgeConfusions",
                        json!({ "verdicts": verdicts, "observations": obs }).to_string(),
                    ))
                }
                Some("verifyConfusion") => Ok(tool_reply(
                    n,
                    "verifyConfusion",
                    json!({ "verdict": "approve", "reason": "前轮观察证据充分" }).to_string(),
                )),
                other => Err(LlmError(format!("不期望的调用: {other:?}"))),
            }
        },
    ));
    let result = run(doc, chat, on());
    assert_eq!(
        judge_calls.load(Ordering::Relaxed),
        2,
        "恰好两轮裁决：第二轮 observations 不再回灌"
    );
    assert_eq!(result.items[1].text().unwrap(), "数据来源统计表");
    let fixes = &result.report.confusion_fixes;
    assert_eq!(fixes.len(), 1);
    assert_eq!(
        (fixes[0].before.as_str(), fixes[0].after.as_str()),
        ("潭", "源")
    );
    assert_eq!(fixes[0].source, "second_opinion");
    // 两轮 observations 都进报告
    assert_eq!(result.report.confusion_observations.len(), 2);
}

// ── observations 回灌的排误：全文一致的高频术语（烟感反例）不回灌 ──

#[test]
fn feedback_skips_frequency_whitelisted_terms() {
    let mut items: Vec<serde_json::Value> = (0..5)
        .map(|i| {
            json!({ "type": "text", "text": "烟感探测器安装", "page_idx": 0,
                    "bbox": [50, 40 + i * 40, 550, 60 + i * 40] })
        })
        .collect();
    items.push(json!({ "type": "text", "text": "公司在竟争中保持优势。",
                       "page_idx": 0, "bbox": [50, 240, 550, 260] }));
    let doc = items_of(Value::Array(items));
    let judge_calls = Arc::new(AtomicU64::new(0));
    let jc = judge_calls.clone();
    let call_id = Arc::new(AtomicU64::new(0));
    let chat: Arc<dyn ChatClient> = Arc::new(FnChat::new(
        move |messages: &[Message], tools: &Value| -> Result<ChatResult, LlmError> {
            let n = call_id.fetch_add(1, Ordering::Relaxed) + 1;
            let content = first_user_content(messages);
            match tools.pointer("/0/function/name").and_then(Value::as_str) {
                Some("judgeConfusions") => {
                    jc.fetch_add(1, Ordering::Relaxed);
                    let verdicts: Vec<Value> = CAND_RE
                        .captures_iter(content)
                        .map(|c| json!({ "index": c[1].parse::<u64>().unwrap(), "action": "keep" }))
                        .collect();
                    Ok(tool_reply(
                        n,
                        "judgeConfusions",
                        json!({ "verdicts": verdicts,
                                "observations": ["「烟感」应为「灶感」"] })
                        .to_string(),
                    ))
                }
                other => Err(LlmError(format!("不期望的调用: {other:?}"))),
            }
        },
    ));
    let result = run(doc.clone(), chat, on());
    assert_eq!(
        judge_calls.load(Ordering::Relaxed),
        1,
        "「烟感」×5 全文一致 → 频率加白，观察不回灌、无第二轮"
    );
    assert_eq!(result.items, doc);
    assert!(result.report.confusion_fixes.is_empty());
}
