// tool-use loop 守卫专项（观察工具往返 / 防震荡 / 轮数耗尽强制搁置 / 坏 JSON 修复）。

mod common;

use common::{FnChat, Scripted, golden_input, parse_suspect, tool_reply};
use mineru_refine::agent_loop::{
    DEFAULT_CONCURRENCY, DEFAULT_MAX_ITERATIONS, DEFAULT_MAX_ROUNDS, Logger, LoopOptions, run_loop,
};
use mineru_refine::id::assign_ids;
use mineru_refine::llm::{ChatClient, ChatResult, Message};
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
            let feedback_ok = messages
                .iter()
                .rev()
                .take(2)
                .all(|m| matches!(m, Message::Tool { content, .. } if content.contains("决策矛盾")));
            if !feedback_ok {
                return Err(mineru_refine::llm::LlmError(
                    "矛盾决策未被驳回（缺少决策矛盾反馈）".into(),
                ));
            }
            return Ok(call("drop", json!({ "id": id, "reason": "确认为页码，删除" })));
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
        logs.iter().any(|l| l.contains("决策矛盾 [page_artifact] it_0005")),
        "矛盾驳回应有日志: {logs:?}"
    );
    assert!(
        logs.iter()
            .any(|l| l.contains("drop [page_artifact] it_0005: 确认为页码，删除")),
        "op 落地应带 reason 审计日志: {logs:?}"
    );
}
