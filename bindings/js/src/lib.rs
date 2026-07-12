// napi-rs 绑定：Bun/Node 直接 import 原生插件。
// items 收/发普通 JS 对象（serde-json 桥接），refine 是真 async（napi tokio runtime）。

use mineru_refine_core::types::MineruItem;
use mineru_refine_core::{
    AssistantMessage, ChatClient, ChatResult, LlmError, Message, ModelConfig, Progress,
    RefineOptions, SplitTableVerdict, TableTranscription, TranscribedCell, Usage, VisionClient,
};
use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

// ── T2 逃生口：用户在 JS 里实现接口（非 OpenAI 兼容后端）──
// 回调经 ThreadsafeFunction 在原生线程投递、返回 Promise（tokio 桥接 await）。
// chat 收 (messages, tools)（JSON 原生形状）；视觉收 (Buffer, ...)；均返回 dict。

/// chat：`(messages, tools) => Promise<reply>`。
type ChatTsfn = ThreadsafeFunction<(Value, Value), Promise<Value>, (Value, Value), Status, false>;
/// 视觉裁决：`(imgA, imgB) => Promise<{verdict, reason}>`。
type VisionJudgeTsfn =
    ThreadsafeFunction<(Buffer, Buffer), Promise<Value>, (Buffer, Buffer), Status, false>;
/// 视觉重转写：`(img, cellsRender) => Promise<{cells}>`。
type VisionTranscribeTsfn =
    ThreadsafeFunction<(Buffer, String), Promise<Value>, (Buffer, String), Status, false>;

#[derive(Deserialize)]
struct ChatReplyDto {
    #[serde(flatten)]
    message: AssistantMessage,
    #[serde(default, alias = "finishReason")]
    finish_reason: String,
    #[serde(default)]
    usage: Usage,
}

struct JsChatClient {
    tsfn: ChatTsfn,
}

#[async_trait::async_trait]
impl ChatClient for JsChatClient {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &Value,
    ) -> std::result::Result<ChatResult, LlmError> {
        let msgs = serde_json::to_value(messages)
            .map_err(|e| LlmError(format!("messages 序列化失败: {e}")))?;
        let promise = self
            .tsfn
            .call_async((msgs, tools.clone()))
            .await
            .map_err(|e| LlmError(format!("JS chat 回调投递失败: {e}")))?;
        let ret = promise
            .await
            .map_err(|e| LlmError(format!("JS chat 回调抛错: {e}")))?;
        let reply: ChatReplyDto =
            serde_json::from_value(ret).map_err(|e| LlmError(format!("JS chat 返回非法: {e}")))?;
        Ok(ChatResult {
            message: reply.message,
            finish_reason: reply.finish_reason,
            usage: reply.usage,
        })
    }
}

#[derive(Deserialize)]
struct VerdictDto {
    verdict: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    usage: Usage,
}

#[derive(Deserialize)]
struct CellDto {
    row: usize,
    col: usize,
    text: String,
}

#[derive(Deserialize, Default)]
struct TranscribeDto {
    #[serde(default)]
    cells: Vec<CellDto>,
    #[serde(default)]
    usage: Usage,
}

struct JsVisionClient {
    judge: VisionJudgeTsfn,
    transcribe: Option<VisionTranscribeTsfn>,
}

#[async_trait::async_trait]
impl VisionClient for JsVisionClient {
    async fn judge_split_table(
        &self,
        img_a: &[u8],
        img_b: &[u8],
    ) -> std::result::Result<SplitTableVerdict, LlmError> {
        let promise = self
            .judge
            .call_async((Buffer::from(img_a.to_vec()), Buffer::from(img_b.to_vec())))
            .await
            .map_err(|e| LlmError(format!("JS judge 回调投递失败: {e}")))?;
        let ret = promise
            .await
            .map_err(|e| LlmError(format!("JS judge 回调抛错: {e}")))?;
        let dto: VerdictDto =
            serde_json::from_value(ret).map_err(|e| LlmError(format!("judge 返回非法: {e}")))?;
        if dto.verdict != "merge" && dto.verdict != "dismiss" {
            return Err(LlmError(format!(
                "verdict 必须是 merge/dismiss: {}",
                dto.verdict
            )));
        }
        Ok(SplitTableVerdict {
            merge: dto.verdict == "merge",
            reason: dto.reason,
            usage: dto.usage,
        })
    }

    async fn transcribe_table(
        &self,
        img: &[u8],
        cells_render: &str,
    ) -> std::result::Result<TableTranscription, LlmError> {
        let Some(tsfn) = &self.transcribe else {
            return Err(LlmError("未提供 visionTranscribe 回调".into()));
        };
        let promise = tsfn
            .call_async((Buffer::from(img.to_vec()), cells_render.to_string()))
            .await
            .map_err(|e| LlmError(format!("JS transcribe 回调投递失败: {e}")))?;
        let ret = promise
            .await
            .map_err(|e| LlmError(format!("JS transcribe 回调抛错: {e}")))?;
        let dto: TranscribeDto = serde_json::from_value(ret)
            .map_err(|e| LlmError(format!("transcribe 返回非法: {e}")))?;
        Ok(TableTranscription {
            cells: dto
                .cells
                .into_iter()
                .map(|c| TranscribedCell {
                    row: c.row,
                    col: c.col,
                    text: c.text,
                })
                .collect(),
            invalid: 0,
            usage: dto.usage,
        })
    }
}

fn parse_items(items: Value) -> Result<Vec<MineruItem>> {
    serde_json::from_value(items)
        .map_err(|e| Error::from_reason(format!("items 必须是 content_list（对象数组）: {e}")))
}

#[napi(object)]
#[derive(Default)]
pub struct RefineOpts {
    /// 源文件 SHA256；提供时启用进程内缓存
    pub sha256: Option<String>,
    /// 外层循环硬上限；不传则自适应（随疑点数 48~512）
    pub max_iterations: Option<u32>,
    /// 疑点并行裁决数，默认 8；1 = 严格串行
    pub concurrency: Option<u32>,
    /// MinerU 产物目录绝对路径；提供则 split_table 启用 Qwen-VL 视觉裁决
    pub image_dir: Option<String>,
    /// OCR 字符混淆修正层（opt-in，默认关）。开启后输出契约变为：
    /// 核心层只删不增 + 混淆层在准入名单内做稀疏一换一替换（全量进 report.confusionFixes）
    pub fix_ocr_confusion: Option<bool>,
    /// 混淆准入名单补充对：每项恰好 2 个不同字符（如 "0D" 表示 0↔D 互换可直接落地）
    pub extra_confusion_pairs: Option<Vec<String>>,
    /// 重度乱码表的视觉重转写层（opt-in，默认关）。机械检测（词典覆盖率塌方）选定目标，
    /// Qwen-VL 对照 img_path 截图逐单元格重转写（全量进 report.tableRewrites，可程序化撤销）。
    /// 开启时必须提供 imageDir。
    pub rewrite_garbled_tables: Option<bool>,
    /// 乱码表降级兜底（opt-in，默认关；纯机械，不依赖 LLM/VL）。跑在重转写层之后：
    /// 仍判废且有 img_path 的表整项降级为 image（table_body 删除并进 removedSpans，
    /// report.tableDegraded 计数）。两层都开 = 先救、救不回再降。
    pub degrade_garbled_tables: Option<bool>,
    /// T1 配置驱动换模型（见 docs/model-abstraction.md）。JSON 原生形状：
    /// `{ reasoning?: { provider?, model, key?, baseUrl? }, vision?: {...} }`。
    /// 不传 chat/vision 回调时生效：有对应角色 → 走 genai 多厂商适配器，否则回落 env 默认。
    pub model_config: Option<Value>,
}

/// MinerU content_list 清洗。fail-open：任何异常原样返回输入（report.failOpen=true）。
/// 返回 { items, provenance, report }，schema 与 TS/HTTP 版完全一致。
///
/// onProgress 可选：清洗阶段每轮迭代回调一次，参数 { iterations, maxIterations,
/// worklistRemaining, inputSuspects }。回调在原生线程上以 NonBlocking 模式投递，
/// 不阻塞清洗；不传则零开销、行为与原先一致。
#[napi]
pub async fn refine(
    items: Value,
    opts: Option<RefineOpts>,
    on_progress: Option<ThreadsafeFunction<Value, (), Value, Status, false>>,
    chat: Option<ChatTsfn>,
    vision_judge: Option<VisionJudgeTsfn>,
    vision_transcribe: Option<VisionTranscribeTsfn>,
) -> Result<Value> {
    let items = parse_items(items)?;
    let opts = opts.unwrap_or_default();
    // T1：配置驱动换模型（modelConfig JSON → ModelConfig）
    let model_config: Option<ModelConfig> = match opts.model_config {
        Some(v) => Some(
            serde_json::from_value(v)
                .map_err(|e| Error::from_reason(format!("modelConfig 配置非法: {e}")))?,
        ),
        None => None,
    };
    // T2：用户实现接口逃生口（比 modelConfig 优先级更高）
    let chat_client = chat.map(|tsfn| Arc::new(JsChatClient { tsfn }) as Arc<dyn ChatClient>);
    let vision_client = vision_judge.map(|judge| {
        Arc::new(JsVisionClient {
            judge,
            transcribe: vision_transcribe,
        }) as Arc<dyn VisionClient>
    });
    // ThreadsafeFunction 本身就是 Clone + Send + Sync 的 Arc 句柄，直接 move 进闭包即可。
    let progress = on_progress.map(|cb| {
        Arc::new(move |p: Progress| {
            if let Ok(v) = serde_json::to_value(&p) {
                cb.call(v, ThreadsafeFunctionCallMode::NonBlocking);
            }
        }) as mineru_refine_core::ProgressSink
    });
    let result = mineru_refine_core::refine(
        items,
        RefineOptions {
            sha256: opts.sha256,
            max_iterations: opts.max_iterations.map(u64::from),
            concurrency: opts.concurrency.map(|c| c as usize),
            image_dir: opts.image_dir.map(Into::into),
            fix_ocr_confusion: opts.fix_ocr_confusion.unwrap_or(false),
            extra_confusion_pairs: opts.extra_confusion_pairs.unwrap_or_default(),
            rewrite_garbled_tables: opts.rewrite_garbled_tables.unwrap_or(false),
            degrade_garbled_tables: opts.degrade_garbled_tables.unwrap_or(false),
            model_config,
            chat: chat_client,
            vision: vision_client,
            progress,
            ..RefineOptions::default()
        },
    )
    .await;
    serde_json::to_value(result).map_err(|e| Error::from_reason(e.to_string()))
}

/// items → full.md 文本（确定性重渲染，与 MinerU pipeline 拼接规则对齐）。
#[napi]
pub fn render_markdown(items: Value) -> Result<String> {
    Ok(mineru_refine_core::render_markdown(&parse_items(items)?))
}

/// 探测器独立调用：返回疑点列表（kind/itemId/evidence/hasOp），不打 LLM。
#[napi]
pub fn detect_suspects(items: Value) -> Result<Value> {
    let suspects = mineru_refine_core::detect_items(&parse_items(items)?);
    serde_json::to_value(suspects).map_err(|e| Error::from_reason(e.to_string()))
}

/// 测试/运维用：清空进程内缓存。
#[napi]
pub fn clear_refine_cache() {
    mineru_refine_core::clear_refine_cache();
}

#[napi]
pub const REFINE_LOGIC_VERSION: &str = mineru_refine_core::REFINE_LOGIC_VERSION;

#[napi]
pub const MODEL_ID: &str = mineru_refine_core::MODEL_ID;

#[napi]
pub const PROMPT_VERSION: &str = mineru_refine_core::PROMPT_VERSION;

#[napi]
pub const CONFUSION_PROMPT_VERSION: &str = mineru_refine_core::CONFUSION_PROMPT_VERSION;

#[napi]
pub const GARBLED_PROMPT_VERSION: &str = mineru_refine_core::GARBLED_PROMPT_VERSION;
