// tool-use loop 守卫专项（观察工具往返 / 防震荡 / 轮数耗尽强制搁置 / 坏 JSON 修复）。

mod common;

use async_trait::async_trait;
use common::{FnChat, Scripted, bbox, golden_input, items_of, parse_suspect, tool_reply};
use mineru_refine::agent_loop::{
    DEFAULT_CONCURRENCY, DEFAULT_MAX_ITERATIONS, DEFAULT_MAX_ROUNDS, Logger, LoopOptions, run_loop,
};
use mineru_refine::id::assign_ids;
use mineru_refine::llm::{ChatClient, ChatResult, LlmError, Message};
use regex::Regex;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

fn silent_log() -> Logger {
    Arc::new(|_| {})
}

fn opts(chat: Arc<dyn ChatClient>, concurrency: usize, max_rounds: u32) -> LoopOptions {
    LoopOptions {
        max_iterations: DEFAULT_MAX_ITERATIONS,
        max_rounds_per_suspect: max_rounds,
        concurrency,
        chat,
        load_image: None,
        vision: None,
        log: silent_log(),
    }
}

fn call(name: &str, args: Value) -> ChatResult {
    static N: AtomicU64 = AtomicU64::new(0);
    tool_reply(
        N.fetch_add(1, Ordering::Relaxed) + 1,
        name,
        args.to_string(),
    )
}

fn last_tool_content(messages: &[Message]) -> String {
    match messages.last() {
        Some(Message::Tool { content, .. }) => content.clone(),
        Some(Message::User { content }) => content.clone(),
        _ => String::new(),
    }
}

type Step = common::ScriptStep;

#[tokio::test]
async fn observation_round_trips_then_verdict() {
    let (ref_items, next_id) = assign_ids(&golden_input());
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let s = |v: &Arc<Mutex<Vec<String>>>| v.clone();
    let steps: Vec<Step> = vec![
        Box::new(|_| Ok(call("outline", json!({})))),
        {
            let seen = s(&seen);
            Box::new(move |m| {
                seen.lock().unwrap().push(last_tool_content(m));
                Ok(call(
                    "getItems",
                    json!({ "id": "it_0002", "before": 1, "after": 1 }),
                ))
            })
        },
        {
            let seen = s(&seen);
            Box::new(move |m| {
                seen.lock().unwrap().push(last_tool_content(m));
                Ok(call("peekPage", json!({ "id": "it_0002" })))
            })
        },
        {
            let seen = s(&seen);
            Box::new(move |m| {
                seen.lock().unwrap().push(last_tool_content(m));
                Ok(call("whyFlagged", json!({ "id": "it_0002" })))
            })
        },
        {
            let seen = s(&seen);
            Box::new(move |m| {
                seen.lock().unwrap().push(last_tool_content(m));
                Ok(call("demote", json!({ "id": "it_0002" })))
            })
        },
        // 后续疑点全 dismiss
        Box::new(|m| {
            let (_, id, _) = parse_suspect(common::first_user_content(m))?;
            Ok(call("dismiss", json!({ "id": id, "reason": "测试收尾" })))
        }),
    ];

    // 脚本按调用次序回放，必须严格串行
    let r = run_loop(
        ref_items,
        next_id,
        opts(Arc::new(Scripted::new(steps)), 1, DEFAULT_MAX_ROUNDS),
    )
    .await
    .unwrap();
    let seen = seen.lock().unwrap();
    assert!(seen[0].contains("it_0002")); // outline 含伪标题（它有 text_level）
    assert!(seen[1].contains(">>>")); // getItems 高亮目标块
    assert!(seen[2].contains("── 第 0 页 ──")); // peekPage 分页展示
    assert!(seen[3].contains("pseudo_heading")); // whyFlagged 给证据
    assert_eq!(r.op_counts.get("demote"), Some(&1));
    let it2 = r.items.iter().find(|x| x.id == "it_0002").unwrap();
    assert!(it2.item.text_level().is_none());
}

#[tokio::test]
async fn oscillation_guard_blocks_split_of_merge_product() {
    let (ref_items, next_id) = assign_ids(&golden_input());
    let rejections = Arc::new(AtomicU64::new(0));
    let next_id_re = Regex::new(r"后块=(it_\d+)").unwrap();
    // 策略：cross_page_break 疑点 → merge（产新块 it_0008）；
    // 其它疑点一律先试 split it_0008（应被防震荡拒）、收到拒绝再 dismiss。
    let rej = rejections.clone();
    let chat = FnChat::new(move |messages: &[Message], _: &Value| {
        let content = common::first_user_content(messages);
        let (kind, id, _) = parse_suspect(content)?;
        if kind == mineru_refine::types::SuspectKind::CrossPageBreak {
            let id_b = next_id_re.captures(content).unwrap()[1].to_string();
            return Ok(call("merge", json!({ "idA": id, "idB": id_b })));
        }
        let last = last_tool_content(messages);
        if last.contains("防震荡") {
            rej.fetch_add(1, Ordering::Relaxed);
            return Ok(call(
                "dismiss",
                json!({ "id": id, "reason": "被防震荡拦截" }),
            ));
        }
        Ok(call("split", json!({ "id": "it_0008", "offset": 10 })))
    });

    // split it_0008 依赖 merge 先落地，必须严格串行
    let r = run_loop(
        ref_items,
        next_id,
        opts(Arc::new(chat), 1, DEFAULT_MAX_ROUNDS),
    )
    .await
    .unwrap();
    assert_eq!(r.op_counts.get("merge"), Some(&1));
    assert!(rejections.load(Ordering::Relaxed) > 0); // 防震荡确实拦了
    assert_eq!(r.op_counts.get("split"), None);
}

#[tokio::test]
async fn max_rounds_exhausted_forces_dismissal() {
    let (ref_items, next_id) = assign_ids(&golden_input());
    // 永远只观察、从不裁决
    let steps: Vec<Step> = vec![Box::new(|_| {
        Ok(call("getItems", json!({ "id": "it_0002" })))
    })];
    let r = run_loop(
        ref_items,
        next_id,
        opts(Arc::new(Scripted::new(steps)), DEFAULT_CONCURRENCY, 3),
    )
    .await
    .unwrap();
    assert_eq!(r.dismissed, 4); // 4 个 hasOp 疑点全部轮数耗尽被搁置
    assert!(r.op_counts.is_empty());
    // 明细逐条展开，与计数一致，且类别正确、带探测器证据
    assert_eq!(r.dismissed_suspects.len(), 4);
    assert!(
        r.dismissed_suspects
            .iter()
            .all(|d| d.reason == "max_rounds_exhausted" && !d.evidence.is_empty())
    );
}

#[tokio::test]
async fn broken_json_arguments_repaired_by_safe_json_repair() {
    let (ref_items, next_id) = assign_ids(&golden_input());
    let chat = FnChat::new(move |messages: &[Message], _: &Value| {
        let (kind, id, _) = parse_suspect(common::first_user_content(messages))?;
        if kind == mineru_refine::types::SuspectKind::PseudoHeading {
            // 尾逗号坏 JSON
            return Ok(tool_reply(1, "demote", format!("{{\"id\": \"{id}\",}}")));
        }
        Ok(call("dismiss", json!({ "id": id, "reason": "skip" })))
    });
    let r = run_loop(
        ref_items,
        next_id,
        opts(Arc::new(chat), DEFAULT_CONCURRENCY, DEFAULT_MAX_ROUNDS),
    )
    .await
    .unwrap();
    assert_eq!(r.op_counts.get("demote"), Some(&1));
}

#[tokio::test]
async fn contradictory_dismiss_plus_op_rejected_then_redecided() {
    // 实测场景复现：LLM 把「应 drop」的分析写进 dismiss.reason，同响应又并行调 drop。
    // 期望：两个调用全部驳回（不静默采纳先到者），回灌矛盾反馈，模型重裁后 drop 落地。
    let (ref_items, next_id) = assign_ids(&golden_input());
    let artifact_rounds = Arc::new(AtomicU64::new(0));
    let ar = artifact_rounds.clone();
    let chat = FnChat::new(move |messages: &[Message], _: &Value| {
        let (kind, id, evidence) = parse_suspect(common::first_user_content(messages))?;
        if kind == mineru_refine::types::SuspectKind::PageArtifact {
            let round = ar.fetch_add(1, Ordering::Relaxed);
            if round == 0 {
                return Ok(common::multi_tool_reply(vec![
                    (
                        "dismiss",
                        json!({ "id": id, "reason": "……证据充分，应 drop 而非 dismiss" }),
                    ),
                    ("drop", json!({ "id": id })),
                ]));
            }
            // 第二轮：两个调用必须各收到一条「决策矛盾」反馈，缺了就是实现回归
            let feedback_ok = messages.iter().rev().take(2).all(
                |m| matches!(m, Message::Tool { content, .. } if content.contains("决策矛盾")),
            );
            if !feedback_ok {
                return Err(mineru_refine::llm::LlmError(
                    "矛盾决策未被驳回（缺少决策矛盾反馈）".into(),
                ));
            }
            return Ok(call(
                "drop",
                json!({ "id": id, "reason": "确认为页码，删除" }),
            ));
        }
        let (name, args) = common::MockChat::default_decision(kind, &id, &evidence)?;
        Ok(call(&name, args))
    });

    let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let lg = logs.clone();
    let r = run_loop(
        ref_items,
        next_id,
        LoopOptions {
            max_iterations: DEFAULT_MAX_ITERATIONS,
            max_rounds_per_suspect: DEFAULT_MAX_ROUNDS,
            concurrency: 1,
            chat: Arc::new(chat),
            load_image: None,
            vision: None,
            log: Arc::new(move |s| lg.lock().unwrap().push(s.to_string())),
        },
    )
    .await
    .unwrap();

    assert_eq!(artifact_rounds.load(Ordering::Relaxed), 2); // 驳回后确实重裁了一轮
    assert_eq!(r.op_counts.get("drop"), Some(&1)); // 重裁后 drop 落地
    assert!(!r.items.iter().any(|x| x.id == "it_0005")); // 页码真的删了
    let logs = logs.lock().unwrap();
    assert!(
        logs.iter()
            .any(|l| l.contains("决策矛盾 [page_artifact] it_0005")),
        "矛盾驳回应有日志: {logs:?}"
    );
    assert!(
        logs.iter()
            .any(|l| l.contains("drop [page_artifact] it_0005: 确认为页码，删除")),
        "op 落地应带 reason 审计日志: {logs:?}"
    );
}

// ── 兄弟组联合裁决 / promote 层级校正 / dismiss 时序竞争守卫 ──

fn capturing_log() -> (Arc<Mutex<Vec<String>>>, Logger) {
    let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let lg = logs.clone();
    (
        logs,
        Arc::new(move |s| lg.lock().unwrap().push(s.to_string())),
    )
}

#[tokio::test]
async fn sibling_group_jointly_adjudicated_in_one_unit() {
    // 三个同级编号块（1．/2．/3．）各因子项前缀证据被标 missed_heading，
    // 应合成一个联合裁决单元：一次对话、一个 iteration 槽位、组内逐成员收尾。
    let input = items_of(json!([
        { "type": "text", "text": "1．预算编制", "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "1.1编制原则", "page_idx": 0, "bbox": bbox(40) },
        { "type": "text", "text": "2．预算执行", "page_idx": 0, "bbox": bbox(80) },
        { "type": "text", "text": "2.1执行要求", "page_idx": 0, "bbox": bbox(120) },
        { "type": "text", "text": "3．预算调整", "page_idx": 0, "bbox": bbox(160) },
        { "type": "text", "text": "3.1调整流程", "page_idx": 0, "bbox": bbox(200) },
    ]));
    let (ref_items, next_id) = assign_ids(&input);
    let chat = FnChat::new(move |messages: &[Message], _: &Value| {
        let content = common::first_user_content(messages);
        assert!(
            content.contains("编号兄弟组"),
            "应是兄弟组联合裁决 prompt: {content}"
        );
        for id in ["it_0001", "it_0003", "it_0005"] {
            assert!(content.contains(id), "组 prompt 应含成员 {id}");
        }
        // 同一条回复逐成员并行裁决：dismiss(A)+promote(B) 不同 id，不是矛盾决策。
        // it_0003 故意给错 level=3，应被同级锚点（it_0001 落地后 level=2）校正。
        Ok(common::multi_tool_reply(vec![
            (
                "promote",
                json!({ "id": "it_0001", "level": 2, "reason": "子项前缀证据" }),
            ),
            (
                "promote",
                json!({ "id": "it_0003", "level": 3, "reason": "与组内一致" }),
            ),
            (
                "dismiss",
                json!({ "id": "it_0005", "reason": "测试组内例外" }),
            ),
        ]))
    });
    let (logs, log) = capturing_log();
    let r = run_loop(
        ref_items,
        next_id,
        LoopOptions {
            max_iterations: DEFAULT_MAX_ITERATIONS,
            max_rounds_per_suspect: DEFAULT_MAX_ROUNDS,
            concurrency: DEFAULT_CONCURRENCY,
            chat: Arc::new(chat),
            load_image: None,
            vision: None,
            log,
        },
    )
    .await
    .unwrap();

    assert_eq!(r.iterations, 1); // 整组只占一个 iteration 槽位（饿死修复的关键）
    assert_eq!(r.op_counts.get("promote"), Some(&2));
    assert_eq!(r.dismissed, 1);
    let level_of = |id: &str| {
        r.items
            .iter()
            .find(|x| x.id == id)
            .unwrap()
            .item
            .text_level()
    };
    assert_eq!(level_of("it_0001"), Some(2));
    assert_eq!(level_of("it_0003"), Some(2)); // 3 被校正为 2
    assert_eq!(level_of("it_0005"), None);
    let logs = logs.lock().unwrap();
    assert!(
        logs.iter()
            .any(|l| l
                .contains("兄弟组联合裁决 [missed_heading] 3 个成员: it_0001, it_0003, it_0005")),
        "缺组裁决日志: {logs:?}"
    );
    assert!(
        logs.iter()
            .any(|l| l.contains("promote level 校正 3→2 [missed_heading] it_0003")),
        "缺 level 校正日志: {logs:?}"
    );
    assert!(
        logs.iter()
            .any(|l| l.contains("dismiss [missed_heading] it_0005: 测试组内例外")),
        "缺组内 dismiss 日志: {logs:?}"
    );
}

#[tokio::test]
async fn promote_level_deterministically_corrected_to_sibling_anchor() {
    // 单疑点流程：LLM 给错 level=5，应被同数制同深度的现存编号标题锚点校正为 2。
    let input = items_of(json!([
        { "type": "text", "text": "4.5核心组织绩效的考核方法", "text_level": 2, "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "4.6核心组织绩效的应用", "page_idx": 0, "bbox": bbox(40) },
    ]));
    let (ref_items, next_id) = assign_ids(&input);
    let chat = FnChat::new(move |messages: &[Message], _: &Value| {
        let (kind, id, _) = parse_suspect(common::first_user_content(messages))?;
        assert_eq!(kind, mineru_refine::types::SuspectKind::MissedHeading);
        Ok(call(
            "promote",
            json!({ "id": id, "level": 5, "reason": "故意给错的 level" }),
        ))
    });
    let (logs, log) = capturing_log();
    let r = run_loop(
        ref_items,
        next_id,
        LoopOptions {
            max_iterations: DEFAULT_MAX_ITERATIONS,
            max_rounds_per_suspect: DEFAULT_MAX_ROUNDS,
            concurrency: 1,
            chat: Arc::new(chat),
            load_image: None,
            vision: None,
            log,
        },
    )
    .await
    .unwrap();

    assert_eq!(r.op_counts.get("promote"), Some(&1));
    let it2 = r.items.iter().find(|x| x.id == "it_0002").unwrap();
    assert_eq!(it2.item.text_level(), Some(2));
    let logs = logs.lock().unwrap();
    assert!(
        logs.iter()
            .any(|l| l.contains("promote level 校正 5→2 [missed_heading] it_0002")),
        "缺 level 校正日志: {logs:?}"
    );
}

/// 每次 chat 前让出执行权：让 join_all 里的并行对话真实交错（时序竞争测试用）。
struct Yielding<C>(C);

#[async_trait]
impl<C: ChatClient> ChatClient for Yielding<C> {
    async fn chat(&self, messages: &[Message], tools: &Value) -> Result<ChatResult, LlmError> {
        tokio::task::yield_now().await;
        self.0.chat(messages, tools).await
    }
}

#[tokio::test]
async fn stale_outline_dismissal_rechallenged_once() {
    // 时序竞争复现：pseudo_heading 对话开始后，并行 missed_heading 对话 promote 落地
    // 改变了标题结构 → 其 dismiss 应被驳回一次并回灌最新 outline，重裁后才采纳。
    let input = items_of(json!([
        { "type": "text", "text": "4.5考核方法", "text_level": 2, "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "4.6绩效应用", "page_idx": 0, "bbox": bbox(40) },
        { "type": "text", "text": "公司应当建立健全体系，确保目标实现。", "text_level": 1, "page_idx": 0, "bbox": bbox(80) },
    ]));
    let (ref_items, next_id) = assign_ids(&input);
    let chat = FnChat::new(move |messages: &[Message], _: &Value| {
        let (kind, id, _) = parse_suspect(common::first_user_content(messages))?;
        match kind {
            mineru_refine::types::SuspectKind::MissedHeading => Ok(call(
                "promote",
                json!({ "id": id, "level": 2, "reason": "兄弟标题对齐" }),
            )),
            mineru_refine::types::SuspectKind::PseudoHeading => {
                let last_tool = match messages.last() {
                    Some(Message::Tool { content, .. }) => content.clone(),
                    _ => String::new(),
                };
                if last_tool.contains("暂缓采纳") {
                    return Ok(call(
                        "dismiss",
                        json!({ "id": id, "reason": "重审最新结构后仍判误报" }),
                    ));
                }
                // 轮询目标块（不带邻居），看到并行 promote 落地后才 dismiss——
                // 保证 dismiss 必然发生在标题结构变化之后
                if last_tool.contains(">>>") && last_tool.contains("text_level=2") {
                    return Ok(call(
                        "dismiss",
                        json!({ "id": id, "reason": "基于过期结构的首次 dismiss" }),
                    ));
                }
                Ok(call(
                    "getItems",
                    json!({ "id": "it_0002", "before": 0, "after": 0 }),
                ))
            }
            k => Err(LlmError(format!("不期望的疑点 kind: {k:?}"))),
        }
    });
    let (logs, log) = capturing_log();
    let r = run_loop(
        ref_items,
        next_id,
        LoopOptions {
            max_iterations: DEFAULT_MAX_ITERATIONS,
            max_rounds_per_suspect: DEFAULT_MAX_ROUNDS,
            concurrency: 2,
            chat: Arc::new(Yielding(chat)),
            load_image: None,
            vision: None,
            log,
        },
    )
    .await
    .unwrap();

    assert_eq!(r.op_counts.get("promote"), Some(&1));
    assert_eq!(r.dismissed, 1);
    // pseudo_heading 块原样保留（dismiss 最终被采纳，未被改动）
    let it3 = r.items.iter().find(|x| x.id == "it_0003").unwrap();
    assert_eq!(it3.item.text_level(), Some(1));
    let logs = logs.lock().unwrap();
    let challenges = logs
        .iter()
        .filter(|l| l.contains("dismiss 暂缓 [pseudo_heading] it_0003"))
        .count();
    assert_eq!(challenges, 1, "应恰好驳回一次: {logs:?}");
    assert!(
        logs.iter()
            .any(|l| l.contains("dismiss [pseudo_heading] it_0003: 重审最新结构后仍判误报")),
        "重裁后的 dismiss 应被采纳: {logs:?}"
    );
}

#[tokio::test]
async fn same_text_page_artifacts_jointly_adjudicated() {
    // 同一文本「问题导向：」出现在 3 个不同页被各标 page_artifact，
    // 应合成一个同文组联合裁决单元（实测 11 处并行裁决出现 10 dismiss + 1 drop 的不一致）。
    let input = items_of(json!([
        { "type": "text", "text": "问题导向：", "page_idx": 0, "bbox": bbox(0) },
        { "type": "text", "text": "正文甲。", "page_idx": 0, "bbox": bbox(40) },
        { "type": "text", "text": "问题导向：", "page_idx": 1, "bbox": bbox(0) },
        { "type": "text", "text": "正文乙。", "page_idx": 1, "bbox": bbox(40) },
        { "type": "text", "text": "问题导向：", "page_idx": 2, "bbox": bbox(0) },
        { "type": "text", "text": "正文丙。", "page_idx": 2, "bbox": bbox(40) },
    ]));
    let (ref_items, next_id) = assign_ids(&input);
    let chat = FnChat::new(move |messages: &[Message], _: &Value| {
        let content = common::first_user_content(messages);
        assert!(
            content.contains("同文组联合裁决"),
            "应是同文组联合裁决 prompt: {content}"
        );
        for id in ["it_0001", "it_0003", "it_0005"] {
            assert!(content.contains(id), "组 prompt 应含成员 {id}");
        }
        // 一致裁决：全部 drop（同文要删都删）
        Ok(common::multi_tool_reply(vec![
            ("drop", json!({ "id": "it_0001", "reason": "同文页面家具" })),
            ("drop", json!({ "id": "it_0003", "reason": "同文页面家具" })),
            ("drop", json!({ "id": "it_0005", "reason": "同文页面家具" })),
        ]))
    });
    let (logs, log) = capturing_log();
    let r = run_loop(
        ref_items,
        next_id,
        LoopOptions {
            max_iterations: DEFAULT_MAX_ITERATIONS,
            max_rounds_per_suspect: DEFAULT_MAX_ROUNDS,
            concurrency: DEFAULT_CONCURRENCY,
            chat: Arc::new(chat),
            load_image: None,
            vision: None,
            log,
        },
    )
    .await
    .unwrap();

    assert_eq!(r.iterations, 1); // 整组一个槽位
    assert_eq!(r.op_counts.get("drop"), Some(&3));
    assert_eq!(r.items.len(), 3); // 三处同文全删，正文保留
    assert!(r.items.iter().all(|x| x.item.text() != Some("问题导向：")));
    assert_eq!(r.removed_spans.len(), 3);
    let logs = logs.lock().unwrap();
    assert!(
        logs.iter()
            .any(|l| l
                .contains("同文组联合裁决 [page_artifact] 3 个成员: it_0001, it_0003, it_0005")),
        "缺同文组日志: {logs:?}"
    );
}
