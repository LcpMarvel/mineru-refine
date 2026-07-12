// genai 适配器：把项目自有的 ChatClient / VisionClient 角色抽象，桥到 genai crate
// 的统一多厂商 chat 接口（DeepSeek / Aliyun(Qwen) / OpenAI / Anthropic / Ollama…）。
//
// 定位（见 docs/model-abstraction.md）：
//   - T1「配置驱动」的默认实现底座。用户只填 ModelConfig（provider/model/key/base_url），
//     一套代码通吃多厂商，无需实现接口。
//   - trait 抽象层（llm.rs 的 ChatClient/VisionClient）不动，仍是 T2 逃生口与测试 mock。
//
// 确定性约定与 llm.rs 的裸 API 客户端对齐：文本 temperature 0 + tool_choice required
//（DeepSeek 另加 thinking disabled）；视觉 temperature 0 + top_k 1（贪婪解码）。
// 私有旋钮（thinking / top_k）经 genai 的 ChatOptions.extra_body 透传。
//
// ⚠️ Qwen `-max` 坑：genai 把以 -max/-high/-low/-min(imal)/-xhigh/-zero/-none 结尾的模型名
// 当「推理强度后缀」剥离并注入 reasoning_effort，导致 DashScope 404/400。对 Qwen 类模型
// 用 extra_body 强制覆盖 model + reasoning_effort:none 抹平（x_merge 浅层覆盖）。

use crate::llm::{
    AssistantMessage, ChatClient, ChatResult, LlmError, Message, SplitTableVerdict,
    TableTranscription, ToolCall, ToolCallFunction, Usage, VisionClient,
};
use async_trait::async_trait;
use genai::Client;
use genai::ServiceTarget;
use genai::adapter::AdapterKind;
use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, ContentPart, MessageContent, Tool, ToolChoice,
    ToolResponse,
};
use genai::resolver::{AuthData, Endpoint};
use genai::{ModelIden, ModelName};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::sync::Arc;

// ── 配置模型（T1）：三端以 JSON 原生形状传入 ──

/// 单角色的模型接入配置（文本裁决 or 视觉裁决各一份）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    /// 厂商/协议标识：deepseek / aliyun(qwen,dashscope) / openai / anthropic / gemini /
    /// ollama / groq / xai。省略时从 `model` 名推断（genai AdapterKind::from_model）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// 模型名（如 deepseek-chat、qwen-vl-max、gpt-4o、claude-3-5-sonnet…）。
    pub model: String,
    /// API key。省略时回落到该厂商在 genai 里的默认环境变量。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// OpenAI 兼容端点 base_url（私有化部署/自定义网关）。省略时用该厂商默认端点。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// 换模型的整体配置：文本裁决 + 视觉裁决各自独立。任一为 None 时该角色回落 env 默认。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    /// 文本推理角色（DeepSeek 占坑位）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ProviderConfig>,
    /// 视觉裁决角色（Qwen-VL 占坑位）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<ProviderConfig>,
}

impl ProviderConfig {
    /// 稳定的「模型身份」串（进缓存 key）：换模型必须命中不同缓存。key 本身不进（不含密钥）。
    fn identity(&self) -> String {
        format!(
            "{}::{}",
            self.provider.as_deref().unwrap_or("auto"),
            self.model
        )
    }
}

impl ModelConfig {
    /// 缓存 key 用的模型身份：文本 + 视觉都进（两者都影响清洗产物）。
    pub fn cache_identity(&self) -> String {
        let r = self.reasoning.as_ref().map(ProviderConfig::identity);
        let v = self.vision.as_ref().map(ProviderConfig::identity);
        format!(
            "{}|{}",
            r.as_deref().unwrap_or("env"),
            v.as_deref().unwrap_or("env")
        )
    }
}

// ── provider → genai AdapterKind ──

fn resolve_adapter(cfg: &ProviderConfig) -> Result<AdapterKind, LlmError> {
    match cfg.provider.as_deref() {
        Some(p) => Ok(match p.to_ascii_lowercase().as_str() {
            "deepseek" => AdapterKind::DeepSeek,
            "aliyun" | "qwen" | "dashscope" => AdapterKind::Aliyun,
            "openai" | "openai-compatible" | "custom" => AdapterKind::OpenAI,
            "anthropic" | "claude" => AdapterKind::Anthropic,
            "gemini" | "google" => AdapterKind::Gemini,
            "ollama" => AdapterKind::Ollama,
            "groq" => AdapterKind::Groq,
            "xai" | "grok" => AdapterKind::Xai,
            other => {
                return Err(LlmError(format!(
                    "未知 provider「{other}」——支持 deepseek/aliyun/openai/anthropic/gemini/ollama/groq/xai，\
                     或省略 provider 由 model 名推断"
                )));
            }
        }),
        None => AdapterKind::from_model(&cfg.model).map_err(|e| {
            LlmError(format!(
                "无法从 model 名「{}」推断 provider，请显式指定 provider: {e}",
                cfg.model
            ))
        }),
    }
}

/// base_url 归一化：genai 用 `base_url.join(\"chat/completions\")` 拼路径，末尾必须带 `/`，
/// 否则最后一段会被替换掉。用户少填 `/` 是常见坑，这里补上。
fn normalize_base_url(url: &str) -> String {
    if url.ends_with('/') {
        url.to_string()
    } else {
        format!("{url}/")
    }
}

/// 构造一个 genai Client：复用进程自建的 ring-provider reqwest client；用
/// ServiceTargetResolver 注入 key / base_url 覆盖（省略则走该厂商默认端点/env）。
fn build_client(cfg: &ProviderConfig) -> Result<(Client, ModelIden, AdapterKind), LlmError> {
    let kind = resolve_adapter(cfg)?;
    let key = cfg.key.clone();
    let base = cfg.base_url.as_deref().map(normalize_base_url);

    let mut builder = Client::builder().with_reqwest(crate::llm::http_client());
    builder = builder.with_service_target_resolver_fn(
        move |mut target: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
            if let Some(k) = &key {
                target.auth = AuthData::from_single(k.clone());
            }
            if let Some(b) = &base {
                target.endpoint = Endpoint::from_owned(b.clone());
            }
            Ok(target)
        },
    );

    let model_iden = ModelIden::new(kind, ModelName::from(cfg.model.clone()));
    Ok((builder.build(), model_iden, kind))
}

/// genai 的推理强度后缀白名单（`ReasoningEffort`/`Verbosity::from_keyword`）：
/// 以 `-<kw>` 结尾的模型名会被剥离并注入 reasoning_effort。
fn ends_with_effort_suffix(model: &str) -> bool {
    matches!(
        model.rsplit_once('-').map(|(_, last)| last),
        Some("zero" | "none" | "low" | "medium" | "high" | "xhigh" | "max" | "minimal")
    )
}

/// 是否需要 Qwen `-max` 绕过：Aliyun/DashScope 系或名字含 qwen，且命中被剥离的后缀。
/// 命中时用 extra_body 覆盖 model（还原被剥的名字）+ reasoning_effort:none（抹掉被误注入的强度）。
fn needs_qwen_effort_fix(kind: AdapterKind, model: &str) -> bool {
    let is_qwen_family = kind == AdapterKind::Aliyun || model.to_ascii_lowercase().contains("qwen");
    is_qwen_family && ends_with_effort_suffix(model)
}

fn qwen_fix_entries(map: &mut Map<String, Value>, model: &str) {
    map.insert("model".into(), Value::String(model.to_string()));
    map.insert("reasoning_effort".into(), json!({ "effort": "none" }));
}

// ── 瞬态重试（对齐 llm.rs 的裸 API 客户端节奏）──

/// genai 错误已把网络/HTTP 折叠进 Display；据文案粗判是否瞬态（4xx 业务错误不重试）。
fn is_transient(err: &str) -> bool {
    [
        "429",
        "500",
        "502",
        "503",
        "504",
        "timeout",
        "timed out",
        "connect",
        "connection",
        "reset",
    ]
    .iter()
    .any(|needle| err.contains(needle))
}

async fn exec_with_retry(
    client: &Client,
    model: &ModelIden,
    req: ChatRequest,
    opts: &ChatOptions,
    who: &str,
) -> Result<genai::chat::ChatResponse, LlmError> {
    let mut last_err: Option<LlmError> = None;
    for attempt in 1..=crate::llm::MAX_ATTEMPTS {
        if attempt > 1 {
            crate::llm::retry_backoff(attempt).await;
        }
        match client
            .exec_chat(model.clone(), req.clone(), Some(opts))
            .await
        {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                let msg = e.to_string();
                if attempt < crate::llm::MAX_ATTEMPTS && is_transient(&msg) {
                    last_err = Some(LlmError(format!(
                        "{who} 瞬态失败（第 {attempt}/{} 次）: {msg}",
                        crate::llm::MAX_ATTEMPTS
                    )));
                    continue;
                }
                return Err(LlmError(format!("{who} 调用失败: {msg}")));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| LlmError(format!("{who} 重试耗尽"))))
}

fn to_usage(u: &genai::chat::Usage) -> Usage {
    Usage {
        prompt_tokens: u.prompt_tokens.unwrap_or(0).max(0) as u64,
        completion_tokens: u.completion_tokens.unwrap_or(0).max(0) as u64,
    }
}

// ── 文本裁决适配器 ──

pub struct GenaiChat {
    client: Client,
    model: ModelIden,
    /// 每次调用透传的私有确定性旋钮（DeepSeek thinking disabled + 可能的 Qwen fix）。
    extra_body: Option<Value>,
}

impl GenaiChat {
    pub fn new(cfg: &ProviderConfig) -> Result<Arc<Self>, LlmError> {
        let (client, model, kind) = build_client(cfg)?;
        let mut extra = Map::new();
        if kind == AdapterKind::DeepSeek {
            // 绕开 reasoning_content 回传的 400 雷（与裸 API 客户端一致）
            extra.insert("thinking".into(), json!({ "type": "disabled" }));
        }
        if needs_qwen_effort_fix(kind, &cfg.model) {
            qwen_fix_entries(&mut extra, &cfg.model);
        }
        Ok(Arc::new(Self {
            client,
            model,
            extra_body: (!extra.is_empty()).then(|| Value::Object(extra)),
        }))
    }
}

/// 项目 OpenAI 风格 messages → genai ChatRequest（system 抽出、assistant tool_calls
/// / tool role 转 genai 原生形状）。
fn to_chat_request(messages: &[Message], tools: &Value) -> ChatRequest {
    let mut system: Option<String> = None;
    let mut msgs: Vec<ChatMessage> = Vec::with_capacity(messages.len());

    for m in messages {
        match m {
            Message::System { content } => {
                system = Some(match system.take() {
                    Some(prev) => format!("{prev}\n{content}"),
                    None => content.clone(),
                });
            }
            Message::User { content } => msgs.push(ChatMessage::user(content.clone())),
            Message::Assistant {
                content,
                tool_calls,
            } => match tool_calls {
                Some(tcs) if !tcs.is_empty() => {
                    let gtcs: Vec<genai::chat::ToolCall> =
                        tcs.iter().map(to_genai_tool_call).collect();
                    msgs.push(ChatMessage::assistant(MessageContent::from_tool_calls(
                        gtcs,
                    )));
                }
                _ => {
                    msgs.push(ChatMessage::assistant(content.clone().unwrap_or_default()));
                }
            },
            Message::Tool {
                tool_call_id,
                content,
            } => {
                msgs.push(ChatMessage::tool(ToolResponse::new(
                    tool_call_id.clone(),
                    content.clone(),
                )));
            }
        }
    }

    let mut req = ChatRequest::new(msgs);
    if let Some(s) = system {
        req = req.with_system(s);
    }
    if let Some(tool_list) = to_genai_tools(tools) {
        req = req.with_tools(tool_list);
    }
    req
}

fn to_genai_tool_call(tc: &ToolCall) -> genai::chat::ToolCall {
    genai::chat::ToolCall {
        call_id: tc.id.clone(),
        fn_name: tc.function.name.clone(),
        // arguments 是 JSON 字符串；解析失败退化成 null（genai 侧仍原样发出）
        fn_arguments: serde_json::from_str(&tc.function.arguments).unwrap_or(Value::Null),
        thought_signatures: None,
    }
}

/// OpenAI tools 数组 `[{type:function, function:{name,description,parameters}}]` → genai Tool。
fn to_genai_tools(tools: &Value) -> Option<Vec<Tool>> {
    let arr = tools.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(arr.len());
    for t in arr {
        let f = t.get("function").unwrap_or(t);
        let Some(name) = f.get("name").and_then(Value::as_str) else {
            continue;
        };
        let mut tool = Tool::new(name.to_string());
        if let Some(desc) = f.get("description").and_then(Value::as_str) {
            tool = tool.with_description(desc.to_string());
        }
        if let Some(params) = f.get("parameters") {
            tool = tool.with_schema(params.clone());
        }
        out.push(tool);
    }
    (!out.is_empty()).then_some(out)
}

fn chat_options(extra_body: &Option<Value>) -> ChatOptions {
    let mut opts = ChatOptions::default()
        .with_temperature(0.0)
        .with_tool_choice(ToolChoice::Required)
        .with_capture_usage(true)
        // reasoning 模型（MiniMax / DeepSeek-R1 / QwQ…）会把 <think>…</think> 混进正文，
        // 让 genai 把它剥进独立 reasoning_content，content/first_text 只留干净裁决正文。
        .with_normalize_reasoning_content(true);
    if let Some(eb) = extra_body {
        opts = opts.with_extra_body(eb.clone());
    }
    opts
}

#[async_trait]
impl ChatClient for GenaiChat {
    async fn chat(&self, messages: &[Message], tools: &Value) -> Result<ChatResult, LlmError> {
        let req = to_chat_request(messages, tools);
        let opts = chat_options(&self.extra_body);
        let resp = exec_with_retry(&self.client, &self.model, req, &opts, "genai/chat").await?;

        let usage = to_usage(&resp.usage);
        let finish_reason = resp
            .stop_reason
            .as_ref()
            .map(|s| s.raw().to_string())
            .unwrap_or_default();
        let content = resp.first_text().map(str::to_string);
        let tool_calls: Vec<ToolCall> = resp
            .into_tool_calls()
            .into_iter()
            .map(from_genai_tool_call)
            .collect();

        Ok(ChatResult {
            message: AssistantMessage {
                content,
                tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            },
            finish_reason,
            usage,
        })
    }
}

fn from_genai_tool_call(tc: genai::chat::ToolCall) -> ToolCall {
    ToolCall {
        id: tc.call_id,
        call_type: "function".into(),
        function: ToolCallFunction {
            name: tc.fn_name,
            // 回项目侧统一是 JSON 字符串
            arguments: serde_json::to_string(&tc.fn_arguments).unwrap_or_else(|_| "{}".into()),
        },
    }
}

// ── 视觉裁决适配器 ──

pub struct GenaiVision {
    client: Client,
    model: ModelIden,
    /// 视觉私有旋钮：top_k=1（贪婪解码，钉死跨运行漂移）+ 可能的 Qwen fix。
    extra_body: Value,
}

impl GenaiVision {
    pub fn new(cfg: &ProviderConfig) -> Result<Arc<Self>, LlmError> {
        let (client, model, kind) = build_client(cfg)?;
        let mut extra = Map::new();
        extra.insert("top_k".into(), json!(1));
        if needs_qwen_effort_fix(kind, &cfg.model) {
            qwen_fix_entries(&mut extra, &cfg.model);
        }
        Ok(Arc::new(Self {
            client,
            model,
            extra_body: Value::Object(extra),
        }))
    }

    fn options(&self, max_tokens: Option<u32>) -> ChatOptions {
        let mut opts = ChatOptions::default()
            .with_temperature(0.0)
            .with_capture_usage(true)
            .with_normalize_reasoning_content(true)
            .with_extra_body(self.extra_body.clone());
        if let Some(mt) = max_tokens {
            opts = opts.with_max_tokens(mt);
        }
        opts
    }
}

fn image_part(img: &[u8]) -> ContentPart {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(img);
    ContentPart::from_binary_base64("image/jpeg", b64, None)
}

#[async_trait]
impl VisionClient for GenaiVision {
    async fn judge_split_table(
        &self,
        img_a: &[u8],
        img_b: &[u8],
    ) -> Result<SplitTableVerdict, LlmError> {
        let user = ChatMessage::user(MessageContent::from_parts(vec![
            ContentPart::from_text(crate::llm::QWEN_PROMPT),
            image_part(img_a),
            image_part(img_b),
        ]));
        let req = ChatRequest::new(vec![user]);
        let opts = self.options(None);
        let resp = exec_with_retry(&self.client, &self.model, req, &opts, "genai/vision").await?;
        let usage = to_usage(&resp.usage);
        let content = resp.first_text().unwrap_or("").to_string();
        let Some((verdict, reason)) = crate::llm::extract_verdict_json(&content) else {
            return Err(LlmError(format!(
                "视觉裁决回复不是合法裁决 JSON: {}",
                content.chars().take(200).collect::<String>()
            )));
        };
        Ok(SplitTableVerdict {
            merge: verdict == "merge",
            reason,
            usage,
        })
    }

    async fn transcribe_table(
        &self,
        img: &[u8],
        cells_render: &str,
    ) -> Result<TableTranscription, LlmError> {
        let prompt = format!("{}\n\n{cells_render}", crate::llm::QWEN_TRANSCRIBE_PROMPT);
        let user = ChatMessage::user(MessageContent::from_parts(vec![
            ContentPart::from_text(prompt),
            image_part(img),
        ]));
        let req = ChatRequest::new(vec![user]);
        let opts = self.options(Some(4096));
        let resp = exec_with_retry(&self.client, &self.model, req, &opts, "genai/vision").await?;
        let usage = to_usage(&resp.usage);
        let content = resp.first_text().unwrap_or("").to_string();
        let Some(mut t) = crate::llm::extract_transcription_json(&content) else {
            return Err(LlmError(format!(
                "视觉重转写回复不是合法 JSON: {}",
                content.chars().take(200).collect::<String>()
            )));
        };
        t.usage = usage;
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Message, ToolCall, ToolCallFunction};
    use genai::adapter::AdapterKind;
    use serde_json::json;

    #[test]
    fn provider_maps_to_adapter_kind() {
        let cases = [
            ("deepseek", AdapterKind::DeepSeek),
            ("aliyun", AdapterKind::Aliyun),
            ("qwen", AdapterKind::Aliyun),
            ("dashscope", AdapterKind::Aliyun),
            ("openai", AdapterKind::OpenAI),
            ("anthropic", AdapterKind::Anthropic),
            ("ollama", AdapterKind::Ollama),
        ];
        for (p, want) in cases {
            let cfg = ProviderConfig {
                provider: Some(p.into()),
                model: "m".into(),
                ..Default::default()
            };
            assert_eq!(resolve_adapter(&cfg).unwrap(), want, "provider {p}");
        }
    }

    #[test]
    fn unknown_provider_errors() {
        let cfg = ProviderConfig {
            provider: Some("nope".into()),
            model: "m".into(),
            ..Default::default()
        };
        assert!(resolve_adapter(&cfg).is_err());
    }

    #[test]
    fn absent_provider_infers_from_model_name() {
        let cfg = ProviderConfig {
            provider: None,
            model: "deepseek-chat".into(),
            ..Default::default()
        };
        assert_eq!(resolve_adapter(&cfg).unwrap(), AdapterKind::DeepSeek);
    }

    #[test]
    fn qwen_effort_fix_gating() {
        // Qwen 家族 + 被剥后缀 → 需要绕过
        assert!(needs_qwen_effort_fix(AdapterKind::Aliyun, "qwen-vl-max"));
        assert!(needs_qwen_effort_fix(AdapterKind::Aliyun, "qwen-max"));
        // 用 openai 兼容端点跑 qwen，名字含 qwen 也命中
        assert!(needs_qwen_effort_fix(AdapterKind::OpenAI, "qwen-vl-max"));
        // DeepSeek 无此问题
        assert!(!needs_qwen_effort_fix(
            AdapterKind::DeepSeek,
            "deepseek-chat"
        ));
        // 合法的 OpenAI 推理后缀不该被 Qwen 绕过误伤
        assert!(!needs_qwen_effort_fix(AdapterKind::OpenAI, "o3-high"));
        // Qwen 但无被剥后缀
        assert!(!needs_qwen_effort_fix(AdapterKind::Aliyun, "qwen-plus"));
    }

    #[test]
    fn effort_suffix_detection() {
        for s in [
            "a-max",
            "a-high",
            "a-low",
            "a-xhigh",
            "a-minimal",
            "a-zero",
            "a-none",
        ] {
            assert!(ends_with_effort_suffix(s), "{s}");
        }
        for s in ["qwen-plus", "deepseek-chat", "gpt-4o"] {
            assert!(!ends_with_effort_suffix(s), "{s}");
        }
    }

    #[test]
    fn base_url_gets_trailing_slash() {
        assert_eq!(normalize_base_url("https://x/v1"), "https://x/v1/");
        assert_eq!(normalize_base_url("https://x/v1/"), "https://x/v1/");
    }

    #[test]
    fn cache_identity_reflects_models() {
        let mc = ModelConfig {
            reasoning: Some(ProviderConfig {
                provider: Some("deepseek".into()),
                model: "deepseek-chat".into(),
                ..Default::default()
            }),
            vision: Some(ProviderConfig {
                provider: Some("aliyun".into()),
                model: "qwen-vl-max".into(),
                ..Default::default()
            }),
        };
        assert_eq!(
            mc.cache_identity(),
            "deepseek::deepseek-chat|aliyun::qwen-vl-max"
        );
        // 只配文本时视觉段落 env
        let mc2 = ModelConfig {
            reasoning: Some(ProviderConfig {
                provider: None,
                model: "gpt-4o".into(),
                ..Default::default()
            }),
            vision: None,
        };
        assert_eq!(mc2.cache_identity(), "auto::gpt-4o|env");
    }

    #[test]
    fn chat_request_conversion_covers_roles_and_tools() {
        let messages = vec![
            Message::System {
                content: "sys".into(),
            },
            Message::User {
                content: "u".into(),
            },
            Message::Assistant {
                content: None,
                tool_calls: Some(vec![ToolCall {
                    id: "c1".into(),
                    call_type: "function".into(),
                    function: ToolCallFunction {
                        name: "dismiss".into(),
                        arguments: "{\"reason\":\"x\"}".into(),
                    },
                }]),
            },
            Message::Tool {
                tool_call_id: "c1".into(),
                content: "ok".into(),
            },
        ];
        let tools = json!([{
            "type": "function",
            "function": { "name": "dismiss", "description": "d", "parameters": {"type":"object"} }
        }]);
        let req = to_chat_request(&messages, &tools);
        assert_eq!(req.system.as_deref(), Some("sys"));
        assert_eq!(req.messages.len(), 3); // user + assistant(tool_calls) + tool
        let tools = req.tools.expect("tools present");
        assert_eq!(tools.len(), 1);
    }

    #[test]
    fn tool_call_roundtrips() {
        let ours = ToolCall {
            id: "c9".into(),
            call_type: "function".into(),
            function: ToolCallFunction {
                name: "drop".into(),
                arguments: "{\"id\":3}".into(),
            },
        };
        let g = to_genai_tool_call(&ours);
        assert_eq!(g.call_id, "c9");
        assert_eq!(g.fn_name, "drop");
        assert_eq!(g.fn_arguments, json!({"id": 3}));
        let back = from_genai_tool_call(g);
        assert_eq!(back.function.name, "drop");
        assert_eq!(back.function.arguments, "{\"id\":3}");
    }

    #[test]
    fn empty_tools_omitted() {
        assert!(to_genai_tools(&json!([])).is_none());
        assert!(to_genai_tools(&json!(null)).is_none());
    }
}
