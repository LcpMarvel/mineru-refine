// LLM 接入层：裸 API（无 SDK），trait 注入（测试 mock 不打网络）。
// - DeepSeek：文本裁决主力（thinking disabled / tool_choice required / temperature 0）
// - Qwen-VL：split_table 视觉裁决（DashScope OpenAI 兼容端点）

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

// ── 消息 / 工具调用模型（OpenAI 兼容） ──

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ToolCallFunction,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    /// arguments 是 JSON 字符串
    pub arguments: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ToolCall>>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AssistantMessage {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Clone, Debug)]
pub struct ChatResult {
    pub message: AssistantMessage,
    pub finish_reason: String,
    pub usage: Usage,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct LlmError(pub String);

/// 文本 LLM 裁决客户端（依赖注入，测试用 mock）。
#[async_trait]
pub trait ChatClient: Send + Sync {
    async fn chat(&self, messages: &[Message], tools: &Value) -> Result<ChatResult, LlmError>;
}

/// split_table 视觉裁决结果。
#[derive(Clone, Debug)]
pub struct SplitTableVerdict {
    /// "merge" 或 "dismiss"
    pub merge: bool,
    pub reason: String,
    pub usage: Usage,
}

/// 视觉裁决客户端（依赖注入，测试用 mock）。
#[async_trait]
pub trait VisionClient: Send + Sync {
    async fn judge_split_table(
        &self,
        img_a: &[u8],
        img_b: &[u8],
    ) -> Result<SplitTableVerdict, LlmError>;
}

/// 只读图片访问器：img_path 是 content_list 里的相对路径（如 images/xxx.jpg），取不到回 None。
#[async_trait]
pub trait LoadImage: Send + Sync {
    async fn load(&self, img_path: &str) -> Option<Vec<u8>>;
}

/// 便捷构造：以 base_dir 为根的只读图片访问器（img_path 缺失/读取失败 → None，绝不抛）。
pub struct ImageDirLoader {
    base_dir: PathBuf,
}

impl ImageDirLoader {
    pub fn new(base_dir: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            base_dir: base_dir.into(),
        })
    }
}

#[async_trait]
impl LoadImage for ImageDirLoader {
    async fn load(&self, img_path: &str) -> Option<Vec<u8>> {
        tokio::fs::read(self.base_dir.join(img_path)).await.ok()
    }
}

// ── 共用重试 ──

const MAX_ATTEMPTS: u32 = 3;

fn retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

async fn backoff(attempt: u32) {
    tokio::time::sleep(Duration::from_millis(attempt as u64 * 1500)).await;
}

// ── DeepSeek 裸 API 客户端 ──
// 接入约定：deepseek-v4-pro / thinking disabled / tool_choice required / temperature 0。

const DEEPSEEK_ENDPOINT: &str = "https://api.deepseek.com/chat/completions";
pub const DEEPSEEK_MODEL: &str = "deepseek-v4-pro";

pub struct DeepSeekClient {
    key: String,
    http: reqwest::Client,
}

impl DeepSeekClient {
    /// 早抛：缺 key 立即失败，不静默降级（项目约定）。
    /// .env 的 DEEPSEEK_APIKEY 或 RAGENT_DEEPSEEK_APIKEY 均可。
    pub fn from_env() -> Result<Arc<Self>, LlmError> {
        let key = std::env::var("DEEPSEEK_APIKEY")
            .or_else(|_| std::env::var("RAGENT_DEEPSEEK_APIKEY"))
            .map_err(|_| {
                LlmError(
                    "DEEPSEEK_APIKEY / RAGENT_DEEPSEEK_APIKEY 均未设置 — 在 .env 里填一个".into(),
                )
            })?;
        Ok(Arc::new(Self {
            key,
            http: reqwest::Client::new(),
        }))
    }
}

#[derive(Deserialize)]
struct ChatChoice {
    message: AssistantMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[async_trait]
impl ChatClient for DeepSeekClient {
    async fn chat(&self, messages: &[Message], tools: &Value) -> Result<ChatResult, LlmError> {
        let body = serde_json::json!({
            "model": DEEPSEEK_MODEL,
            "messages": messages,
            "tools": tools,
            "tool_choice": "required",
            "temperature": 0, // thinking disabled 下生效
            "thinking": { "type": "disabled" }, // 绕开 reasoning_content 回传的 400 雷
            "stream": false,
        });

        // 瞬态失败重试：并发跑 loop 时偶发 socket 断开 / 429 / 5xx，重试兜掉；4xx 业务错误立刻抛。
        let mut last_err: Option<LlmError> = None;
        for attempt in 1..=MAX_ATTEMPTS {
            if attempt > 1 {
                backoff(attempt).await;
            }
            let res = match self
                .http
                .post(DEEPSEEK_ENDPOINT)
                .bearer_auth(&self.key)
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(LlmError(format!(
                        "DeepSeek 网络错误（第 {attempt}/{MAX_ATTEMPTS} 次）: {e}"
                    )));
                    continue;
                }
            };

            let status = res.status().as_u16();
            if !res.status().is_success() {
                let text = res.text().await.unwrap_or_default();
                if retryable_status(status) {
                    last_err = Some(LlmError(format!(
                        "DeepSeek HTTP {status}（第 {attempt}/{MAX_ATTEMPTS} 次）: {}",
                        text.chars().take(300).collect::<String>()
                    )));
                    continue;
                }
                return Err(LlmError(format!("DeepSeek HTTP {status}: {text}")));
            }

            let json: ChatResponse = res
                .json()
                .await
                .map_err(|e| LlmError(format!("DeepSeek 响应解析失败: {e}")))?;
            let Some(choice) = json.choices.into_iter().next() else {
                return Err(LlmError("DeepSeek 无 choices".into()));
            };
            return Ok(ChatResult {
                message: choice.message,
                finish_reason: choice.finish_reason.unwrap_or_default(),
                usage: json.usage.unwrap_or_default(),
            });
        }
        Err(last_err.unwrap_or_else(|| LlmError("DeepSeek 重试耗尽".into())))
    }
}

// ── Qwen-VL 裸 API 客户端 ──
// DashScope OpenAI 兼容端点，仅做【判定类】视觉裁决——输出是决策（merge/dismiss），
// 不是内容字符，不碰纯削减保真红线。

const QWEN_DEFAULT_BASE_URL: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1";
const QWEN_DEFAULT_MODEL: &str = "qwen-vl-max";

const QWEN_PROMPT: &str = "图1是 PDF 某页末尾的表格，图2是紧接着的下一页开头的表格。\
判断图2是否是图1这张表被分页拆开的延续部分（看列网格是否同一套、切缝处内容/编号是否接续、图2有无自己独立的表头主题）。\
只输出 JSON：{\"verdict\":\"merge\"|\"dismiss\",\"reason\":\"一句话依据\"}，merge=同一张表的延续，dismiss=两张不同的表。";

pub struct QwenVlClient {
    key: String,
    base_url: String,
    model: String,
    http: reqwest::Client,
}

impl QwenVlClient {
    /// 缺 key 立即抛（项目约定：早抛，不静默降级——回退由调用方决定）。
    pub fn from_env() -> Result<Arc<Self>, LlmError> {
        let key = std::env::var("QWEN_APIKEY")
            .map_err(|_| LlmError("QWEN_APIKEY 未设置 — 在 .env 里填（视觉裁决需要）".into()))?;
        Ok(Arc::new(Self {
            key,
            base_url: std::env::var("QWEN_BASE_URL")
                .unwrap_or_else(|_| QWEN_DEFAULT_BASE_URL.into()),
            model: std::env::var("QWEN_VISION_MODEL").unwrap_or_else(|_| QWEN_DEFAULT_MODEL.into()),
            http: reqwest::Client::new(),
        }))
    }
}

fn data_url(img: &[u8]) -> String {
    use base64::Engine;
    format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(img)
    )
}

#[async_trait]
impl VisionClient for QwenVlClient {
    async fn judge_split_table(
        &self,
        img_a: &[u8],
        img_b: &[u8],
    ) -> Result<SplitTableVerdict, LlmError> {
        let body = serde_json::json!({
            "model": self.model,
            "temperature": 0,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": QWEN_PROMPT },
                    { "type": "image_url", "image_url": { "url": data_url(img_a) } },
                    { "type": "image_url", "image_url": { "url": data_url(img_b) } },
                ],
            }],
        });

        let mut last_err: Option<LlmError> = None;
        for attempt in 1..=MAX_ATTEMPTS {
            if attempt > 1 {
                backoff(attempt).await;
            }
            let res = match self
                .http
                .post(format!("{}/chat/completions", self.base_url))
                .bearer_auth(&self.key)
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(LlmError(format!(
                        "Qwen-VL 网络错误（第 {attempt}/{MAX_ATTEMPTS} 次）: {e}"
                    )));
                    continue;
                }
            };

            let status = res.status().as_u16();
            if !res.status().is_success() {
                let text = res.text().await.unwrap_or_default();
                if retryable_status(status) {
                    last_err = Some(LlmError(format!(
                        "Qwen-VL HTTP {status}（第 {attempt}/{MAX_ATTEMPTS} 次）: {}",
                        text.chars().take(300).collect::<String>()
                    )));
                    continue;
                }
                return Err(LlmError(format!(
                    "Qwen-VL HTTP {status}: {}",
                    text.chars().take(500).collect::<String>()
                )));
            }

            let json: Value = res
                .json()
                .await
                .map_err(|e| LlmError(format!("Qwen-VL 响应解析失败: {e}")))?;
            let content = json
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .unwrap_or("");
            let parsed = extract_verdict_json(content);
            let Some((verdict, reason)) = parsed else {
                return Err(LlmError(format!(
                    "Qwen-VL 回复不是合法裁决 JSON: {}",
                    content.chars().take(200).collect::<String>()
                )));
            };
            return Ok(SplitTableVerdict {
                merge: verdict == "merge",
                reason,
                usage: Usage {
                    prompt_tokens: json
                        .pointer("/usage/prompt_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    completion_tokens: json
                        .pointer("/usage/completion_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                },
            });
        }
        Err(last_err.unwrap_or_else(|| LlmError("Qwen-VL 重试耗尽".into())))
    }
}

/// 从回复文本中扒出 `{...}` 并过 safe-json-repair 解析裁决。
fn extract_verdict_json(content: &str) -> Option<(String, String)> {
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    if end < start {
        return None;
    }
    let parsed = parse_json_safe(&content[start..=end])?;
    let verdict = parsed.get("verdict")?.as_str()?;
    if verdict != "merge" && verdict != "dismiss" {
        return None;
    }
    let reason = parsed
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("（未给依据）")
        .to_string();
    Some((verdict.to_string(), reason))
}

/// 兜偶发坏 JSON：先 safe-json-repair 修复，再 serde 解析；修不出来回 None。
pub fn parse_json_safe(input: &str) -> Option<Value> {
    let repaired = safe_json_repair::repair(input, &safe_json_repair::Options::default());
    if !repaired.ok {
        return None;
    }
    serde_json::from_str(&repaired.json).ok()
}
