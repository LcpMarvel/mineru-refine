// 测试共享件：golden fixture 构造 + 脚本化 mock LLM（不打真 API）。
#![allow(dead_code)]

use async_trait::async_trait;
use mineru_refine::llm::{
    AssistantMessage, ChatClient, ChatResult, LlmError, LoadImage, Message, SplitTableVerdict,
    ToolCall, ToolCallFunction, Usage, VisionClient,
};
use mineru_refine::types::{MineruItem, SuspectKind};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub fn mi(v: Value) -> MineruItem {
    serde_json::from_value(v).expect("测试 fixture 必须是合法对象")
}

pub fn items_of(v: Value) -> Vec<MineruItem> {
    serde_json::from_value(v).expect("测试 fixture 必须是合法 item 数组")
}

pub fn bbox(y0: i64) -> Value {
    json!([50, y0, 550, y0 + 20])
}

/// golden fixture：一份带 5 类可处理 quirk 的"文档"。
/// it_0001 真标题 / it_0002 伪标题 / it_0003+it_0004 跨页断句 /
/// it_0005 页码混入 / it_0006 markdown 链接残留 / it_0007 干净表格。
pub fn golden_input() -> Vec<MineruItem> {
    items_of(json!([
        { "type": "text", "text": "第一章 总则", "text_level": 1, "page_idx": 0, "bbox": bbox(40) },
        {
            "type": "text",
            "text": "公司应当建立健全战略管理体系，确保战略目标的实现。",
            "text_level": 1, // 伪标题：含逗号 + 句末标点
            "page_idx": 0,
            "bbox": bbox(80),
        },
        { "type": "text", "text": "战略管理是指公司为实现长期发展目标而进行的", "page_idx": 0, "bbox": bbox(120) },
        { "type": "text", "text": "一系列计划、执行与评估活动。", "page_idx": 1, "bbox": bbox(40) },
        { "type": "text", "text": "- 3 -", "page_idx": 1, "bbox": bbox(780) },
        { "type": "text", "text": "详见[公司官网](http://example.com)发布的文件。", "page_idx": 1, "bbox": bbox(120) },
        {
            "type": "table",
            "table_body": "<table><tr><td>指标</td><td>目标值</td></tr></table>",
            "table_caption": ["表1 绩效指标"],
            "page_idx": 1,
            "bbox": bbox(200),
        },
    ]))
}

/// golden_input 经正确清洗后的期望输出（golden fixture 断言目标）。
pub fn golden_expected() -> Vec<MineruItem> {
    let input = golden_input();
    let mut demoted = input[1].clone();
    demoted.remove("text_level"); // demote 删字段，不是设 null
    let mut merged = input[2].clone();
    merged.set(
        "text",
        json!("战略管理是指公司为实现长期发展目标而进行的一系列计划、执行与评估活动。"),
    );
    merged.set("bbox", json!([50, 40, 550, 140])); // union(it3.bbox, it4.bbox)
    merged.set("page_idx", json!(0)); // 取首块
    let mut stripped = input[5].clone();
    stripped.set("text", json!("详见公司官网发布的文件。")); // strip md_link
    vec![
        input[0].clone(),
        demoted,
        merged,
        // it_0005 页码被 drop
        stripped,
        input[6].clone(),
    ]
}

// ── 脚本化"假 LLM" ──

static SUSPECT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"当前疑点：\[(\w+)\] item (it_\d+)").unwrap());
static EVIDENCE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"证据：(.+)").unwrap());
pub static NEXT_ID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"后块=(it_\d+)").unwrap());
static LEVEL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"level=(\d+)").unwrap());
static OFFSET_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"offset=(\d+)").unwrap());

pub type KindHandler = Box<dyn Fn(&str, &str) -> Result<(String, Value), LlmError> + Send + Sync>;

pub fn tool_reply(call_id: u64, name: &str, arguments: String) -> ChatResult {
    ChatResult {
        message: AssistantMessage {
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: format!("call_{call_id}"),
                call_type: "function".into(),
                function: ToolCallFunction {
                    name: name.to_string(),
                    arguments,
                },
            }]),
        },
        finish_reason: "tool_calls".into(),
        usage: Usage {
            prompt_tokens: 100,
            completion_tokens: 20,
        },
    }
}

/// 从 user 消息解析疑点 kind/id，按 kind 直接回对应 op 的 tool_call。
pub struct MockChat {
    pub calls: AtomicU64,
    call_id: AtomicU64,
    overrides: HashMap<SuspectKind, KindHandler>,
}

impl MockChat {
    pub fn new() -> Self {
        Self::with(HashMap::new())
    }

    pub fn with(overrides: HashMap<SuspectKind, KindHandler>) -> Self {
        Self {
            calls: AtomicU64::new(0),
            call_id: AtomicU64::new(0),
            overrides,
        }
    }

    pub fn call_count(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    fn default_decision(
        kind: SuspectKind,
        id: &str,
        evidence: &str,
    ) -> Result<(String, Value), LlmError> {
        let id_b = || -> Result<String, LlmError> {
            NEXT_ID_RE
                .captures(evidence)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string())
                .ok_or_else(|| LlmError(format!("mock 无法从证据解析后块 ID: {evidence}")))
        };
        Ok(match kind {
            SuspectKind::PseudoHeading => ("demote".into(), json!({ "id": id })),
            SuspectKind::CrossPageBreak => ("merge".into(), json!({ "idA": id, "idB": id_b()? })),
            SuspectKind::PageArtifact => ("drop".into(), json!({ "id": id })),
            SuspectKind::ResidualMarkup => {
                ("strip".into(), json!({ "id": id, "pattern": "md_link" }))
            }
            SuspectKind::GiantBlock => (
                "dismiss".into(),
                json!({ "id": id, "reason": "mock 默认不拆" }),
            ),
            SuspectKind::EmptyTable => ("drop".into(), json!({ "id": id })),
            // split_table 仅视觉裁决，永不落文本路径——落到这里就是实现回归了
            SuspectKind::SplitTable => {
                return Err(LlmError(format!(
                    "split_table 不应走文本路径（疑点 {id} 落到了文本 mock）"
                )));
            }
            SuspectKind::SplitList => ("mergeList".into(), json!({ "idA": id, "idB": id_b()? })),
            SuspectKind::MissedHeading => {
                let level = LEVEL_RE
                    .captures(evidence)
                    .and_then(|c| c[1].parse::<i64>().ok())
                    .unwrap_or(2);
                ("promote".into(), json!({ "id": id, "level": level }))
            }
            SuspectKind::TrailingMarker => {
                let offset = OFFSET_RE
                    .captures(evidence)
                    .and_then(|c| c[1].parse::<i64>().ok())
                    .ok_or_else(|| LlmError(format!("mock 无法从证据解析 offset: {evidence}")))?;
                ("split".into(), json!({ "id": id, "offset": offset }))
            }
            SuspectKind::SeparatedCaption => (
                "dismiss".into(),
                json!({ "id": id, "reason": "mock 默认不重排" }),
            ),
            SuspectKind::ExtraChar => {
                let offset = OFFSET_RE
                    .captures(evidence)
                    .and_then(|c| c[1].parse::<i64>().ok())
                    .ok_or_else(|| LlmError(format!("mock 无法从证据解析 offset: {evidence}")))?;
                ("deleteChar".into(), json!({ "id": id, "offset": offset }))
            }
            SuspectKind::CaptionIssue => {
                return Err(LlmError("mock 未定义 caption_issue 的处理".into()));
            }
        })
    }
}

pub fn first_user_content(messages: &[Message]) -> &str {
    messages
        .iter()
        .find_map(|m| match m {
            Message::User { content } => Some(content.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

pub fn parse_suspect(content: &str) -> Result<(SuspectKind, String, String), LlmError> {
    let caps = SUSPECT_RE.captures(content).ok_or_else(|| {
        LlmError(format!(
            "mock 无法解析疑点描述: {}",
            content.chars().take(120).collect::<String>()
        ))
    })?;
    let kind: SuspectKind = serde_json::from_value(json!(&caps[1]))
        .map_err(|_| LlmError(format!("mock 未知疑点 kind: {}", &caps[1])))?;
    let id = caps[2].to_string();
    let evidence = EVIDENCE_RE
        .captures(content)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    Ok((kind, id, evidence))
}

#[async_trait]
impl ChatClient for MockChat {
    async fn chat(&self, messages: &[Message], _tools: &Value) -> Result<ChatResult, LlmError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let (kind, id, evidence) = parse_suspect(first_user_content(messages))?;
        let (name, args) = match self.overrides.get(&kind) {
            Some(h) => h(&id, &evidence)?,
            None => Self::default_decision(kind, &id, &evidence)?,
        };
        let n = self.call_id.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(tool_reply(n, &name, args.to_string()))
    }
}

/// 一调用就炸的"假 LLM"：验证 fail-open 与"无疑点不打 LLM"。
pub struct ExplodingChat {
    pub calls: AtomicU64,
}

impl ExplodingChat {
    pub fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
        }
    }

    pub fn call_count(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl ChatClient for ExplodingChat {
    async fn chat(&self, _: &[Message], _: &Value) -> Result<ChatResult, LlmError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Err(LlmError("LLM 不可用（测试注入）".into()))
    }
}

/// 闭包式视觉裁决 mock。
pub struct FnVision<F>(pub F);

#[async_trait]
impl<F> VisionClient for FnVision<F>
where
    F: Fn(&[u8], &[u8]) -> Result<SplitTableVerdict, LlmError> + Send + Sync,
{
    async fn judge_split_table(&self, a: &[u8], b: &[u8]) -> Result<SplitTableVerdict, LlmError> {
        (self.0)(a, b)
    }
}

pub fn verdict(merge: bool, reason: &str, prompt: u64, completion: u64) -> SplitTableVerdict {
    SplitTableVerdict {
        merge,
        reason: reason.to_string(),
        usage: Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
        },
    }
}

/// 闭包式图片访问 mock。
pub struct FnLoader<F>(pub F);

#[async_trait]
impl<F> LoadImage for FnLoader<F>
where
    F: Fn(&str) -> Option<Vec<u8>> + Send + Sync,
{
    async fn load(&self, img_path: &str) -> Option<Vec<u8>> {
        (self.0)(img_path)
    }
}

pub type ScriptStep = Box<dyn Fn(&[Message]) -> Result<ChatResult, LlmError> + Send + Sync>;

/// 按脚本逐轮回放的假 LLM（超出脚本长度时停在最后一步）。
pub struct Scripted {
    steps: Vec<ScriptStep>,
    i: AtomicUsize,
}

impl Scripted {
    pub fn new(steps: Vec<ScriptStep>) -> Self {
        Self {
            steps,
            i: AtomicUsize::new(0),
        }
    }

    pub fn rounds(&self) -> usize {
        self.i.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl ChatClient for Scripted {
    async fn chat(&self, messages: &[Message], _: &Value) -> Result<ChatResult, LlmError> {
        let i = self.i.fetch_add(1, Ordering::Relaxed);
        let step = &self.steps[i.min(self.steps.len() - 1)];
        step(messages)
    }
}

/// 闭包式 chat mock（每轮同一决策函数；闭包能看到 tools——混淆层测试用它区分
/// judge/verify 调用）。自带调用计数（call_count）。
pub struct FnChat<F>(pub F, pub AtomicU64);

impl<F> FnChat<F> {
    pub fn new(f: F) -> Self {
        Self(f, AtomicU64::new(0))
    }

    pub fn call_count(&self) -> u64 {
        self.1.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl<F> ChatClient for FnChat<F>
where
    F: Fn(&[Message], &Value) -> Result<ChatResult, LlmError> + Send + Sync,
{
    async fn chat(&self, messages: &[Message], tools: &Value) -> Result<ChatResult, LlmError> {
        self.1.fetch_add(1, Ordering::Relaxed);
        (self.0)(messages, tools)
    }
}
