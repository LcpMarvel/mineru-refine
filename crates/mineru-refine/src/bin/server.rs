// HTTP transport（首选）：POST /refine 收 content_list JSON，回 { items, provenance, report }。
// 消费方在解析 content_list.json 之前调一次。fail-open 在 refine() 内已兜；
// transport 层再兜一层（坏请求 → 400）。
//
// 进度：POST /refine/stream 同样收 RefineRequest，回 text/event-stream：
//   event: progress  data: { iterations, maxIterations, worklistRemaining, inputSuspects }  （每轮迭代一帧）
//   event: result    data: { items, provenance, report }                                      （收尾一帧，即非流式 /refine 的回包）
// 适配 Web 前端「清洗中」进度条；解析中（MinerU 真页码）走调用方自己的轮询接口。
//
// 跑：  mineru-refine-server   # 默认端口 8771，MINERU_REFINE_PORT 可改

use axum::{
    Json, Router,
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event, Sse},
    },
    routing::{get, post},
};
use futures::channel::mpsc;
use futures::stream::StreamExt;
use mineru_refine::{MineruItem, Progress, RefineOptions, RefineResult, refine};
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RefineRequest {
    items: Vec<MineruItem>,
    sha256: Option<String>,
    max_iterations: Option<u64>,
    /// MinerU 产物目录绝对路径（须与本服务共享文件系统）；提供则 split_table 启用 Qwen-VL 视觉裁决。
    image_dir: Option<String>,
    /// OCR 字符混淆修正层（opt-in，默认关）。
    fix_ocr_confusion: Option<bool>,
    /// 混淆准入名单补充对：每项恰好 2 个不同字符（如 "0D"）。
    extra_confusion_pairs: Option<Vec<String>>,
    /// 重度乱码表的视觉重转写层（opt-in，默认关；需要 imageDir）。
    rewrite_garbled_tables: Option<bool>,
    /// 乱码表降级兜底（opt-in，默认关；纯机械）：仍判废且有图的表整项降级为 image。
    degrade_garbled_tables: Option<bool>,
}

/// 坏请求体的统一 400 回包（两个 handler 共用，避免状态码/文案各写一份漂移）。
fn bad_request(e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": format!("请求体不合法: {e}") })),
    )
}

/// RefineRequest → (items, RefineOptions)。progress 由调用处按需设置（流式才接线）。
fn into_parts(req: RefineRequest) -> (Vec<MineruItem>, RefineOptions) {
    let opts = RefineOptions {
        sha256: req.sha256,
        max_iterations: req.max_iterations,
        image_dir: req.image_dir.map(Into::into),
        fix_ocr_confusion: req.fix_ocr_confusion.unwrap_or(false),
        extra_confusion_pairs: req.extra_confusion_pairs.unwrap_or_default(),
        rewrite_garbled_tables: req.rewrite_garbled_tables.unwrap_or(false),
        degrade_garbled_tables: req.degrade_garbled_tables.unwrap_or(false),
        ..RefineOptions::default()
    };
    (req.items, opts)
}

async fn handle_refine(
    body: Result<Json<RefineRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Json(req) = body.map_err(bad_request)?;
    let (items, opts) = into_parts(req);
    let result = refine(items, opts).await;
    Ok(Json(
        serde_json::to_value(result).expect("RefineResult 序列化不可能失败"),
    ))
}

/// 流式清洗：progress 帧逐轮推送，末尾一帧 result。坏请求仍回 400（非 SSE）。
async fn handle_refine_stream(
    body: Result<Json<RefineRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(j) => j,
        Err(e) => return bad_request(e).into_response(),
    };
    let (items, mut opts) = into_parts(req);

    // 无界 channel：进度帧不可丢、不可阻塞清洗线程（unbounded_send 同步且不阻塞）。
    let (tx, rx) = mpsc::unbounded::<Event>();
    let tx_progress = tx.clone();
    opts.progress = Some(Arc::new(move |p: Progress| {
        let ev = Event::default()
            .event("progress")
            .json_data(&p)
            .expect("Progress 序列化不可能失败");
        let _ = tx_progress.unbounded_send(ev); // 客户端断开 → 静默丢帧，不影响清洗
    }));

    // 清洗在独立任务上跑，收尾推一帧 result 再关闭流（tx drop → rx 终止）。
    tokio::spawn(async move {
        let result: RefineResult = refine(items, opts).await;
        let ev = Event::default()
            .event("result")
            .json_data(&result)
            .expect("RefineResult 序列化不可能失败");
        let _ = tx.unbounded_send(ev);
    });

    let stream = rx.map(Ok::<Event, Infallible>);
    Sse::new(stream).into_response()
}

async fn handle_health() -> Json<Value> {
    Json(json!({ "ok": true, "service": "mineru-refine" }))
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let port: u16 = std::env::var("MINERU_REFINE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8771);

    let app = Router::new()
        .route("/refine", post(handle_refine))
        .route("/refine/stream", post(handle_refine_stream))
        .route("/health", get(handle_health));

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap_or_else(|e| panic!("端口 {port} 绑定失败: {e}"));
    eprintln!(
        "[mineru-refine] HTTP transport 启动: http://localhost:{port}  (POST /refine, POST /refine/stream, GET /health)"
    );
    axum::serve(listener, app).await.expect("server 异常退出");
}
