// PyO3 绑定：mineru_refine.refine(items, ...) -> dict。
// items 收/发普通 Python 对象（list[dict]），经 pythonize 与 Rust 核心互转。
// LLM 调用在独立 tokio runtime 上跑，期间释放 GIL（不卡住调用方解释器）。

use mineru_refine_core::types::MineruItem;
use mineru_refine_core::{
    AssistantMessage, ChatClient, ChatResult, LlmError, Message, ModelConfig, Progress,
    RefineOptions, SplitTableVerdict, TableTranscription, TranscribedCell, Usage, VisionClient,
    render_markdown as core_render_markdown,
};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pythonize::{depythonize, pythonize};
use serde::Deserialize;
use serde_json::Value;
use std::sync::{Arc, LazyLock};

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime 构建失败")
});

// ── T2 逃生口：用户在 Python 里实现接口（非 OpenAI 兼容后端）──
// 数据全用 JSON 原生形状：messages=list[dict]、tools=list[dict]、图片=bytes，返回 dict。
// 回调在原生 RUNTIME 线程上同步触发（临时重取 GIL）——实现内部自行阻塞（requests/httpx）。

/// chat 回调返回：{ content?, toolCalls?/tool_calls?, finishReason?, usage? }。
#[derive(Deserialize)]
struct ChatReplyDto {
    #[serde(flatten)]
    message: AssistantMessage,
    #[serde(default, alias = "finishReason")]
    finish_reason: String,
    #[serde(default)]
    usage: Usage,
}

/// 用户传入的实现 `chat(messages, tools) -> dict` 的对象。
struct PyChatClient {
    callable: Py<PyAny>,
}

#[async_trait::async_trait]
impl ChatClient for PyChatClient {
    async fn chat(&self, messages: &[Message], tools: &Value) -> Result<ChatResult, LlmError> {
        Python::attach(|py| {
            let msgs = pythonize(py, messages)
                .map_err(|e| LlmError(format!("messages 序列化失败: {e}")))?;
            let tools_obj =
                pythonize(py, tools).map_err(|e| LlmError(format!("tools 序列化失败: {e}")))?;
            let ret = self
                .callable
                .bind(py)
                .call1((msgs, tools_obj))
                .map_err(|e| LlmError(format!("Python chat 回调抛错: {e}")))?;
            let reply: ChatReplyDto = depythonize(&ret)
                .map_err(|e| LlmError(format!("Python chat 回调返回非法: {e}")))?;
            Ok(ChatResult {
                message: reply.message,
                finish_reason: reply.finish_reason,
                usage: reply.usage,
            })
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

/// 用户传入的实现 `judge_split_table(img_a, img_b) -> dict` 与
/// `transcribe_table(img, cells_render) -> dict` 的对象。
struct PyVisionClient {
    callable: Py<PyAny>,
}

#[async_trait::async_trait]
impl VisionClient for PyVisionClient {
    async fn judge_split_table(
        &self,
        img_a: &[u8],
        img_b: &[u8],
    ) -> Result<SplitTableVerdict, LlmError> {
        Python::attach(|py| {
            let a = PyBytes::new(py, img_a);
            let b = PyBytes::new(py, img_b);
            let ret = self
                .callable
                .bind(py)
                .call_method1("judge_split_table", (a, b))
                .map_err(|e| LlmError(format!("Python judge_split_table 回调抛错: {e}")))?;
            let dto: VerdictDto = depythonize(&ret)
                .map_err(|e| LlmError(format!("judge_split_table 返回非法: {e}")))?;
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
        })
    }

    async fn transcribe_table(
        &self,
        img: &[u8],
        cells_render: &str,
    ) -> Result<TableTranscription, LlmError> {
        Python::attach(|py| {
            let img_obj = PyBytes::new(py, img);
            let ret = self
                .callable
                .bind(py)
                .call_method1("transcribe_table", (img_obj, cells_render))
                .map_err(|e| LlmError(format!("Python transcribe_table 回调抛错: {e}")))?;
            let dto: TranscribeDto = depythonize(&ret)
                .map_err(|e| LlmError(format!("transcribe_table 返回非法: {e}")))?;
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
        })
    }
}

fn parse_items(items: &Bound<'_, PyAny>) -> PyResult<Vec<MineruItem>> {
    depythonize(items)
        .map_err(|e| PyValueError::new_err(format!("items 必须是 content_list（对象数组）: {e}")))
}

/// MinerU content_list 清洗。fail-open：任何异常原样返回输入（report.failOpen=true）。
///
/// 返回 dict：{ "items": [...], "provenance": [], "report": {...} }，
/// 字段与 TS/HTTP 版完全一致（camelCase report）。
#[pyfunction]
/// progress 可选：清洗阶段每轮迭代回调一次，参数是一个 dict
/// `{ "iterations", "maxIterations", "worklistRemaining", "inputSuspects" }`。
/// 回调在原生工作线程上调用（临时重新获取 GIL），实现应轻量；不传则零开销、
/// 行为与原先逐字节一致。
#[pyo3(signature = (items, *, sha256=None, max_iterations=None, concurrency=None, image_dir=None, fix_ocr_confusion=false, extra_confusion_pairs=None, rewrite_garbled_tables=false, degrade_garbled_tables=false, model_config=None, chat=None, vision=None, progress=None))]
#[allow(clippy::too_many_arguments)] // PyO3 keyword-only 参数面，逐项展开是接口本体
fn refine(
    py: Python<'_>,
    items: Bound<'_, PyAny>,
    sha256: Option<String>,
    max_iterations: Option<u64>,
    concurrency: Option<usize>,
    image_dir: Option<String>,
    fix_ocr_confusion: bool,
    extra_confusion_pairs: Option<Vec<String>>,
    rewrite_garbled_tables: bool,
    degrade_garbled_tables: bool,
    model_config: Option<Bound<'_, PyAny>>,
    chat: Option<Py<PyAny>>,
    vision: Option<Py<PyAny>>,
    progress: Option<Py<PyAny>>,
) -> PyResult<Py<PyAny>> {
    let items = parse_items(&items)?;
    // T1：配置驱动换模型（dict → ModelConfig，camelCase：provider/model/key/baseUrl）
    let model_config: Option<ModelConfig> = match model_config {
        Some(mc) => Some(
            depythonize(&mc)
                .map_err(|e| PyValueError::new_err(format!("model_config 配置非法: {e}")))?,
        ),
        None => None,
    };
    // T2：用户实现接口逃生口（比 model_config 优先级更高）
    let chat_client =
        chat.map(|callable| Arc::new(PyChatClient { callable }) as Arc<dyn ChatClient>);
    let vision_client =
        vision.map(|callable| Arc::new(PyVisionClient { callable }) as Arc<dyn VisionClient>);
    // Py<PyAny> 本身就是 Send + Sync + Clone 的引用计数句柄，直接 move 进闭包即可。
    let progress = progress.map(|callable| {
        Arc::new(move |p: Progress| {
            // 回调在 RUNTIME 线程上触发，此刻 GIL 已被 detach 释放——临时取回再调用 Python。
            Python::attach(|py| {
                if let Ok(obj) = pythonize(py, &p) {
                    let _ = callable.bind(py).call1((obj,)); // 回调抛错不打断清洗
                }
            });
        }) as mineru_refine_core::ProgressSink
    });
    let opts = RefineOptions {
        sha256,
        max_iterations,
        concurrency,
        image_dir: image_dir.map(Into::into),
        fix_ocr_confusion,
        extra_confusion_pairs: extra_confusion_pairs.unwrap_or_default(),
        rewrite_garbled_tables,
        degrade_garbled_tables,
        model_config,
        chat: chat_client,
        vision: vision_client,
        progress,
        ..RefineOptions::default()
    };
    let result = py.detach(|| RUNTIME.block_on(mineru_refine_core::refine(items, opts)));
    Ok(pythonize(py, &result)?.unbind())
}

/// items → full.md 文本（确定性重渲染，与 MinerU pipeline 拼接规则对齐）。
#[pyfunction]
fn render_markdown(items: Bound<'_, PyAny>) -> PyResult<String> {
    Ok(core_render_markdown(&parse_items(&items)?))
}

/// 探测器独立调用：返回疑点列表（kind/itemId/evidence/hasOp），不打 LLM。
#[pyfunction]
fn detect_suspects(py: Python<'_>, items: Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let suspects = mineru_refine_core::detect_items(&parse_items(&items)?);
    Ok(pythonize(py, &suspects)?.unbind())
}

/// 测试/运维用：清空进程内缓存。
#[pyfunction]
fn clear_refine_cache() {
    mineru_refine_core::clear_refine_cache();
}

#[pymodule]
fn mineru_refine(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(refine, m)?)?;
    m.add_function(wrap_pyfunction!(render_markdown, m)?)?;
    m.add_function(wrap_pyfunction!(detect_suspects, m)?)?;
    m.add_function(wrap_pyfunction!(clear_refine_cache, m)?)?;
    m.add(
        "REFINE_LOGIC_VERSION",
        mineru_refine_core::REFINE_LOGIC_VERSION,
    )?;
    m.add("PROMPT_VERSION", mineru_refine_core::PROMPT_VERSION)?;
    m.add(
        "CONFUSION_PROMPT_VERSION",
        mineru_refine_core::CONFUSION_PROMPT_VERSION,
    )?;
    m.add(
        "GARBLED_PROMPT_VERSION",
        mineru_refine_core::GARBLED_PROMPT_VERSION,
    )?;
    m.add("MODEL_ID", mineru_refine_core::MODEL_ID)?;
    Ok(())
}
