# mineru-refine

> 🌏 English version: [README.md](./README.md)

[MinerU](https://github.com/opendatalab/MinerU) 解析结果的后处理器(linter / fixer)——
本 crate 是核心实现,Python(PyO3)与 JS(napi-rs)绑定都是它的薄包装。

接收 MinerU 的 `content_list`(item 对象数组),修掉解析产生的高频结构问题——伪标题、
跨页断句、跨页拆表、混入正文的页眉页脚、LaTeX / 链接残留——返回**同 schema** 的
content_list。**只做削减与重组,绝不新增一个字**:输出的每个内容字符都来自输入,由机器
逐步校验,违反即自动回滚;任何异常 / LLM 不可用都原样返回输入(fail-open),绝不外漏
panic。探测器、修复操作集、闸门的完整设计文档见
[仓库 README](https://github.com/LcpMarvel/mineru-refine#readme)。

```bash
cargo add mineru-refine
```

## 用法

```rust
use mineru_refine::{refine, RefineOptions};

let result = refine(items, RefineOptions {
    sha256: Some(sha),                        // 可选:源文件 SHA256,提供则启用进程内缓存
    max_iterations: None,                     // 可选:修复循环硬上限,默认随疑点数自适应
    concurrency: Some(8),                     // 可选:并行裁决的疑点数,1 = 严格串行
    image_dir: Some("/abs/mineru/out".into()),// 可选:MinerU 产物目录 → 启用跨页拆表的视觉裁决
    fix_ocr_confusion: false,                 // 可选:opt-in 的 OCR 字符混淆修正层
    extra_confusion_pairs: vec![],            // 可选:混淆准入名单补充对,如 ["0D"]
    rewrite_garbled_tables: false,            // 可选:opt-in 的重度乱码表视觉重转写层(需要 image_dir)
    // model_config: 配置驱动换模型——见下文「换模型」
    ..Default::default()
}).await;
// 永不 Err、panic 不外漏:fail-open 内置,看 result.report.fail_open
result.items;    // 清洗后的 content_list(同 schema)
result.report;   // 审计:iterations / op_counts / dismissed / removed_spans
                 //      / violations / token_usage / fail_open
```

`items` 是 `Vec<MineruItem>`——`MineruItem` 底层是保序 JSON 对象(`serde_json::Map`),
未知字段原样透传、键序不变,serde 直接与 `content_list.json` 互转。

环境变量:`DEEPSEEK_APIKEY`(文本裁决,必需;缺失时 refine 直接 fail-open)、
`QWEN_APIKEY`(视觉裁决需要;缺失则跨页拆表疑点跳过)。库本身不读 `.env`,
请在宿主程序里设置(CLI / server 二进制会自动加载当前目录 `.env`)。

独立可用的工具函数(都不调 LLM):

```rust
mineru_refine::detect_items(&items);     // 探测器:返回疑点列表
mineru_refine::render_markdown(&items);  // items → full.md 确定性重渲染
```

测试 / 嵌入场景可注入假 LLM:`RefineOptions` 的 `chat` / `vision` / `load_image` / `log`
分别接受 `Arc<dyn ChatClient>` / `Arc<dyn VisionClient>` / `Arc<dyn LoadImage>` / `Logger`,
默认实现是 DeepSeek / Qwen-VL 的裸 reqwest 客户端。

## 换模型（自定义 LLM）

默认文本角色是 DeepSeek、视觉角色是 Qwen-VL。两条机制可把它们指向任意其它 LLM;完整说明
见[主 README](https://github.com/LcpMarvel/mineru-refine#换模型自定义-llm)。

**1. `model_config`——配置驱动、多厂商（推荐）。** 基于 [genai](https://github.com/jeremychone/rust-genai)
crate,把文本(`reasoning`)和/或视觉(`vision`)角色设为 DeepSeek / Aliyun(Qwen) / OpenAI /
Anthropic / Gemini / Ollama / Groq / xAI / 任意 OpenAI 兼容端点之一。省略某角色则回落 env 默认。

```rust
use mineru_refine::{refine, ModelConfig, ProviderConfig, RefineOptions};

// MiniMax-M3 是 OpenAI 兼容 + 多模态,一个模型同时充当两个角色
let minimax = ProviderConfig {
    provider: Some("openai".into()),
    model: "MiniMax-M3".into(),
    key: Some(key.clone()),
    base_url: Some("https://api.minimaxi.com/v1".into()),
};
let result = refine(items, RefineOptions {
    image_dir: Some("/abs/mineru/out".into()),
    model_config: Some(ModelConfig {
        reasoning: Some(minimax.clone()),
        vision: Some(minimax),
    }),
    ..Default::default()
}).await;
```

`provider` 接受 `deepseek` / `aliyun`(`qwen`、`dashscope`) / `openai`(`openai-compatible`、
`custom`) / `anthropic`(`claude`) / `gemini`(`google`) / `ollama` / `groq` / `xai`(`grok`);
省略则从模型名推断。会吐 `<think>…</think>` 块的推理模型已妥善处理——该块被自动剥离。

**2. 自定义回调——逃生口（优先级高于 `model_config`）。** 当 `model_config` 表达不了你的
鉴权/代理/模型逻辑时,注入你自己的 `chat: Arc<dyn ChatClient>` / `vision: Arc<dyn VisionClient>`
(与测试 mock 同一套注入点)。

## CLI / HTTP server(`--features bin`)

```bash
cargo install mineru-refine --features bin

cat content_list.json | mineru-refine                  # stdin JSON → stdout JSON
mineru-refine-server                                    # POST /refine + GET /health,端口 8771
```

## 开发

```bash
cargo test -p mineru-refine     # 全程 mock LLM,不打网络
just --list                     # 仓库根:真实数据工作流 / 冒烟 / 发布
```

## License

MIT
