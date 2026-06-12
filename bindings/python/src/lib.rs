// PyO3 绑定：mineru_refine.refine(items, ...) -> dict。
// items 收/发普通 Python 对象（list[dict]），经 pythonize 与 Rust 核心互转。
// LLM 调用在独立 tokio runtime 上跑，期间释放 GIL（不卡住调用方解释器）。

use mineru_refine_core::types::MineruItem;
use mineru_refine_core::{RefineOptions, render_markdown as core_render_markdown};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pythonize::{depythonize, pythonize};
use std::sync::LazyLock;

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime 构建失败")
});

fn parse_items(items: &Bound<'_, PyAny>) -> PyResult<Vec<MineruItem>> {
    depythonize(items)
        .map_err(|e| PyValueError::new_err(format!("items 必须是 content_list（对象数组）: {e}")))
}

/// MinerU content_list 清洗。fail-open：任何异常原样返回输入（report.failOpen=true）。
///
/// 返回 dict：{ "items": [...], "provenance": [], "report": {...} }，
/// 字段与 TS/HTTP 版完全一致（camelCase report）。
#[pyfunction]
#[pyo3(signature = (items, *, sha256=None, max_iterations=None, concurrency=None, image_dir=None, fix_ocr_confusion=false, extra_confusion_pairs=None, rewrite_garbled_tables=false))]
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
) -> PyResult<Py<PyAny>> {
    let items = parse_items(&items)?;
    let opts = RefineOptions {
        sha256,
        max_iterations,
        concurrency,
        image_dir: image_dir.map(Into::into),
        fix_ocr_confusion,
        extra_confusion_pairs: extra_confusion_pairs.unwrap_or_default(),
        rewrite_garbled_tables,
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
