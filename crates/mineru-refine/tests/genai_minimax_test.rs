//! 真 LLM 冒烟测试：拿 MiniMax-M3（OpenAI 兼容 + 原生多模态）验证 genai 底座
//! 的文本裁决 + 视觉裁决两条链路真的能连通、能拿到合法产物。
//!
//! 默认 `#[ignore]`（要网络 + 真 key），手动跑：
//!   cargo test -p mineru-refine --test genai_minimax_test -- --ignored --nocapture
//!
//! Key 从仓库根 `.env` 的 `MINIMAX_APIKEY` 读（或同名环境变量已导出时直接用）。

use mineru_refine::llm::{ChatClient, Message, VisionClient};
use mineru_refine::types::MineruItem;
use mineru_refine::{
    GenaiChat, GenaiVision, ModelConfig, ProviderConfig, RefineOptions, RefineResult, refine,
};
use serde_json::json;

/// MiniMax OpenAI 兼容端点。
const MINIMAX_BASE_URL: &str = "https://api.minimaxi.com/v1";
const MINIMAX_MODEL: &str = "MiniMax-M3";

/// 从环境变量或仓库根 `.env` 里取 `MINIMAX_APIKEY`。
fn minimax_key() -> Option<String> {
    if let Ok(k) = std::env::var("MINIMAX_APIKEY") {
        if !k.trim().is_empty() {
            return Some(k.trim().to_string());
        }
    }
    // crate 目录：<repo>/crates/mineru-refine → 回两级找 .env
    let manifest = env!("CARGO_MANIFEST_DIR");
    let env_path = std::path::Path::new(manifest).join("../../.env");
    let text = std::fs::read_to_string(env_path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("MINIMAX_APIKEY") {
            let val = rest.trim_start_matches([' ', '=']).trim();
            let val = val.trim_matches(['"', '\'']);
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

fn minimax_provider() -> ProviderConfig {
    ProviderConfig {
        provider: Some("openai".into()),
        model: MINIMAX_MODEL.into(),
        key: minimax_key(),
        base_url: Some(MINIMAX_BASE_URL.into()),
    }
}

/// 从 test_data 里随便取两张表格图（真 JPEG）喂视觉裁决。
fn two_table_images() -> (Vec<u8>, Vec<u8>) {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let dir = std::path::Path::new(manifest)
        .join("../../test_data/mineru/MN-JZY-001_战略管理规范/images");
    let mut jpgs: Vec<_> = std::fs::read_dir(&dir)
        .expect("test_data images dir 应存在")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jpg"))
        .collect();
    jpgs.sort();
    assert!(jpgs.len() >= 2, "至少要两张图做视觉冒烟");
    let a = std::fs::read(&jpgs[0]).expect("读图1");
    let b = std::fs::read(&jpgs[1]).expect("读图2");
    (a, b)
}

#[tokio::test]
#[ignore = "真 LLM 冒烟，需 MINIMAX_APIKEY + 网络；跑：--ignored --nocapture"]
async fn minimax_chat_roundtrip() {
    let Some(_) = minimax_key() else {
        panic!("未找到 MINIMAX_APIKEY（环境变量或 .env）");
    };
    let chat = GenaiChat::new(&minimax_provider()).expect("构造 GenaiChat");

    let messages = vec![
        Message::System {
            content: "你是一个只回答数字的助手。".into(),
        },
        Message::User {
            content: "1 加 1 等于几？只回一个阿拉伯数字。".into(),
        },
    ];
    let tools = json!([]);

    let res = chat
        .chat(&messages, &tools)
        .await
        .expect("MiniMax chat 调用应成功");

    let content = res.message.content.unwrap_or_default();
    println!(
        "[minimax chat] finish={} usage(prompt={}, completion={}) content={:?}",
        res.finish_reason, res.usage.prompt_tokens, res.usage.completion_tokens, content
    );
    assert!(!content.trim().is_empty(), "回复应有非空文本内容");
    assert!(
        !content.contains("<think>") && !content.contains("</think>"),
        "content 不应残留 <think> 推理块（应被 genai normalize 剥离），实得: {content}"
    );
    assert!(
        content.contains('2'),
        "1+1 的回复里应含 '2'，实得: {content}"
    );
}

#[tokio::test]
#[ignore = "真 LLM 冒烟，需 MINIMAX_APIKEY + 网络；跑：--ignored --nocapture"]
async fn minimax_vision_judge_split_table() {
    let Some(_) = minimax_key() else {
        panic!("未找到 MINIMAX_APIKEY（环境变量或 .env）");
    };
    let vision = GenaiVision::new(&minimax_provider()).expect("构造 GenaiVision");

    let (img_a, img_b) = two_table_images();
    let verdict = vision
        .judge_split_table(&img_a, &img_b)
        .await
        .expect("MiniMax 视觉裁决应返回合法裁决 JSON");

    println!(
        "[minimax vision] merge={} reason={:?} usage(prompt={}, completion={})",
        verdict.merge, verdict.reason, verdict.usage.prompt_tokens, verdict.usage.completion_tokens
    );
    assert!(!verdict.reason.trim().is_empty(), "裁决 reason 应非空");
}

// ── e2e：拿 MiniMax 当文本+视觉两个角色，跑一份真实 mineru 文档的完整 refine ──

/// MiniMax 同时充当文本裁决 + 视觉裁决角色（M3 原生多模态，一个模型两用）。
fn minimax_model_config() -> ModelConfig {
    ModelConfig {
        reasoning: Some(minimax_provider()),
        vision: Some(minimax_provider()),
    }
}

/// 待处理文档根目录（含 content_list.json 与 images/）。可用 MINERU_DOC_DIR 覆盖。
fn doc_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("MINERU_DOC_DIR") {
        return std::path::PathBuf::from(d);
    }
    let manifest = env!("CARGO_MANIFEST_DIR");
    std::path::Path::new(manifest).join("../../test_data/mineru/MN-ZBZ-003_管理评审程序")
}

fn load_doc_items(dir: &std::path::Path) -> Vec<MineruItem> {
    let text =
        std::fs::read_to_string(dir.join("content_list.json")).expect("读 content_list.json");
    serde_json::from_str(&text).expect("content_list.json 应是合法 MineruItem 数组")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "真 LLM e2e，需 MINIMAX_APIKEY + 网络 + 分钟级耗时；跑：--ignored --nocapture"]
async fn minimax_refine_document_e2e() {
    let Some(_) = minimax_key() else {
        panic!("未找到 MINIMAX_APIKEY（环境变量或 .env）");
    };
    let dir = doc_dir();
    let items = load_doc_items(&dir);
    let before = items.len();
    println!("[e2e] 文档: {} | 输入 {before} 个 item", dir.display());

    let started = std::time::Instant::now();
    let RefineResult {
        items: out,
        provenance,
        report,
    } = refine(
        items,
        RefineOptions {
            model_config: Some(minimax_model_config()),
            image_dir: Some(dir.clone()),
            ..Default::default()
        },
    )
    .await;
    let elapsed = started.elapsed();

    println!("[e2e] 耗时 {:.1}s", elapsed.as_secs_f64());
    println!(
        "[e2e] item: {before} → {} | provenance {} 条",
        out.len(),
        provenance.len()
    );
    println!(
        "[e2e] 迭代 {} 轮 | 搁置 {} | 保真回滚 {} | removedSpans {} | fail_open={}",
        report.iterations,
        report.dismissed,
        report.violations,
        report.removed_spans.len(),
        report.fail_open,
    );
    println!("[e2e] op_counts = {:?}", report.op_counts);
    println!(
        "[e2e] token: prompt {} / completion {}",
        report.token_usage.prompt, report.token_usage.completion
    );

    assert!(
        !report.fail_open,
        "整篇 fail_open 说明底座调用崩了（配置/网络/解析），不算处理成功"
    );
    // 输出不该凭空长出 item（核心不变量：只删不增，表降级除外）。
    assert!(out.len() <= before, "输出 item 数不应超过输入");
}
