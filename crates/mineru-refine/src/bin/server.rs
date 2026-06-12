// HTTP transport（首选）：POST /refine 收 content_list JSON，回 { items, provenance, report }。
// 消费方在解析 content_list.json 之前调一次。fail-open 在 refine() 内已兜；
// transport 层再兜一层（坏请求 → 400）。
//
// 跑：  mineru-refine-server   # 默认端口 8771，MINERU_REFINE_PORT 可改

use axum::{
    Json, Router,
    http::StatusCode,
    routing::{get, post},
};
use mineru_refine::{MineruItem, RefineOptions, refine};
use serde::Deserialize;
use serde_json::{Value, json};

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
}

async fn handle_refine(
    body: Result<Json<RefineRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Json(req) = body.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("请求体不合法: {e}") })),
        )
    })?;
    let result = refine(
        req.items,
        RefineOptions {
            sha256: req.sha256,
            max_iterations: req.max_iterations,
            image_dir: req.image_dir.map(Into::into),
            fix_ocr_confusion: req.fix_ocr_confusion.unwrap_or(false),
            extra_confusion_pairs: req.extra_confusion_pairs.unwrap_or_default(),
            rewrite_garbled_tables: req.rewrite_garbled_tables.unwrap_or(false),
            ..RefineOptions::default()
        },
    )
    .await;
    Ok(Json(
        serde_json::to_value(result).expect("RefineResult 序列化不可能失败"),
    ))
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
        .route("/health", get(handle_health));

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap_or_else(|e| panic!("端口 {port} 绑定失败: {e}"));
    eprintln!(
        "[mineru-refine] HTTP transport 启动: http://localhost:{port}  (POST /refine, GET /health)"
    );
    axum::serve(listener, app).await.expect("server 异常退出");
}
