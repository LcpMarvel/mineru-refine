# mineru-refine (Rust core)

MinerU 解析结果的 linter/fixer——本 crate 是核心实现，Python（PyO3）与 JS（napi-rs）绑定
都是它的薄包装。content_list 进、同 schema 出，只做削减与重组，绝不新增一个字
（机器闸门保证 `C_out ⊆ C_in`，违反即回滚/fail-open）。
设计文档（探测器/op 集/守卫/视觉裁决）见[仓库根 README](https://github.com/LcpMarvel/mineru-refine#readme)。

```toml
[dependencies]
mineru-refine = "0.7"
```

## 用法

```rust
use mineru_refine::{refine, RefineOptions};

let result = refine(items, RefineOptions {
    sha256: Some(sha),                        // 可选：启用进程内缓存
    max_iterations: None,                     // 默认自适应：clamp(2N+16, 48, 512)
    concurrency: Some(8),                     // 疑点并行裁决数；1 = 严格串行
    image_dir: Some("/abs/mineru/out".into()),// 可选：MinerU 产物目录 → 启用 split_table 视觉裁决
    ..Default::default()
}).await;
// 永不 Err / 永不 panic 外漏：fail-open 内置，看 result.report.fail_open
```

`items` 是 `Vec<MineruItem>`——`MineruItem` 底层是保序 JSON 对象（`serde_json::Map`），
未知字段原样透传、键序不变，serde 直接与 content_list.json 互转。

独立可用的工具件（都不打 LLM）：

```rust
mineru_refine::detect_items(&items);     // 探测器：疑点列表
mineru_refine::render_markdown(&items);  // items → full.md 确定性重渲染
```

测试/嵌入场景可注入假 LLM：`RefineOptions { chat, vision, load_image, log, .. }`
分别接受 `Arc<dyn ChatClient>` / `Arc<dyn VisionClient>` / `Arc<dyn LoadImage>` / `Logger`，
默认实现是 DeepSeek / Qwen-VL 裸 reqwest 客户端（环境变量 `DEEPSEEK_APIKEY`、`QWEN_APIKEY`）。

## CLI / HTTP server（`--features bin`）

```bash
cargo install mineru-refine --features bin
cat content_list.json | mineru-refine                  # stdin JSON → stdout JSON
mineru-refine-server                                    # POST /refine + GET /health，端口 8771
```

## 开发

```bash
cargo test -p mineru-refine     # 全程 mock LLM，不打网络
just --list                     # 仓库根：真实数据工作流 / 冒烟 / 发布
```
