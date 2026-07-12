# 模型抽象设计调研：解绑 DeepSeek / Qwen

> 目标：把"文本裁决必须是 DeepSeek、视觉裁决必须是 Qwen"这条硬约束抽象掉，
> 允许用户换任意模型（OpenAI 兼容端点、Anthropic、本地部署…），
> 理想情况下**不写代码，只填配置**。
>
> 本文是**设计与选型结论**，不是实现。落地前先读完"已知坑"一节。

## 1. 现状诊断

核心层（`crates/mineru-refine/src/llm.rs`）其实**已经有按角色的 trait 抽象**，
DeepSeek / Qwen 只是它们的内置默认实现，不是抽象本身：

- `ChatClient::chat(messages, tools) -> ChatResult` —— 文本推理角色（DeepSeek 占坑），
  本质是 OpenAI 风格的 tool-calling 对话接口。
- `VisionClient::{judge_split_table, transcribe_table}` —— 视觉角色（Qwen 占坑）。

`RefineOptions.chat` / `RefineOptions.vision` 已允许注入自定义实现（测试就靠它 mock）。

真正被"焊死"的只有两处：

1. **默认装配**（`refine.rs:236-247`）：不注入时硬编码 `DeepSeekClient::from_env()` /
   `QwenVlClient::from_env()`。
2. **FFI 绑定完全没暴露注入点**：Python（`bindings/python/src/lib.rs`）/ JS（`bindings/js`）
   用户只能靠环境变量（`DEEPSEEK_BASE_URL/MODEL`、`QWEN_BASE_URL/VISION_MODEL`）指向
   OpenAI 兼容端点，无法真正"实现接口"换任意后端。

## 2. 三层换模型策略

把"换模型"拆成三级，从零成本到全控制：

| 层级 | 场景 | 用户要做的 |
|---|---|---|
| **T1 配置** | 模型是 OpenAI 兼容 + 支持 tool-call | 只填 base_url / model / key，**不写代码** |
| **T2 实现接口** | 任意后端（Anthropic、本地、非兼容协议） | 实现 `ChatClient` / `VisionClient` |
| **T3 默认** | 开箱即用 | 什么都不做，落 DeepSeek / Qwen |

结论：**优先做 T1（配置驱动的多厂商）**，把 T2（用户实现接口回调）降级为逃生口。
这样工作量从"三端 × 接口桥接"塌缩成"core 一次 + 三端各加几个配置字段"。

## 3. 选型：用 `genai` crate 做默认实现底座

不该三端各手写一遍多厂商适配。Rust 生态已有成熟的统一 LLM 抽象库：

| crate | 覆盖 | tool-call | 视觉 | 适配度 |
|---|---|---|---|---|
| **genai**（jeremychone/rust-genai） | 25+ 厂商含 DeepSeek/Aliyun | ✅ 原生 | ✅ | **最贴**——轻量，就是"统一 chat 接口"，不强加 agent 框架 |
| llm（graniet/llm） | 类似 | ✅ | ✅ | 功能更杂（TTS/STT/链式），偏重 |
| rig-core | 20+ | ✅ | ✅ | 偏 agent/RAG 框架，最重 |

**推荐 genai**：它正好是我们手写的 `ChatClient`/`VisionClient` 那层的成品版，
原生认识 DeepSeek + Aliyun(Qwen/DashScope) + OpenAI 兼容自定义端点，tool-calling
跨厂商差异（OpenAI vs Anthropic）它替我们抹平。

### 设计思路：genai 作为默认实现，现有 trait 抽象层不动

```
RefineOptions
  ├─ chat: Option<Arc<dyn ChatClient>>      ← 保留！Rust 逃生口 + 测试 mock（T2）
  ├─ vision: Option<Arc<dyn VisionClient>>  ← 保留！
  └─ model_config: Option<ModelConfig>      ← 新增：{ reasoning:{provider,model,key,base_url}, vision:{...} }（T1）
```

装配优先级（改造 `refine.rs:236` 那段）：

1. 注入了 `chat`/`vision` → 直接用（Rust/测试，T2）。
2. 否则有 `model_config` → 构造 `GenaiChat`/`GenaiVision` 适配器（内部调 genai），
   一套代码通吃 DeepSeek/Anthropic/OpenAI/本地 Ollama/Qwen…（T1）。
3. 否则 → 落 env 默认（DeepSeek/Qwen，向后兼容，T3）。

**三端只需暴露 `model_config` 这一个 JSON 字段**——Python 传 dict、JS 传 object、
CLI 传 JSON，零回调桥接。

## 4. 真实验证结论（已跑通）

用临时项目真实调用 DeepSeek + DashScope（key 取自项目 `.env`，未内嵌），
验证 genai 能否承载我们的确定性约定：

| 验证项 | 结果 |
|---|---|
| genai 内置 DeepSeek / Aliyun(Qwen) adapter | ✅ 都识别 |
| DeepSeek `tool_choice=required` | ✅ 稳定吐 tool_call |
| DeepSeek `extra_body:{thinking:disabled}` 透传 | ✅ 无 400，被接受 |
| Qwen-VL 多图视觉裁决 | ✅ 回 `{"verdict":"dismiss"}` |
| Qwen `extra_body:{top_k:1}` 透传 | ✅ 被接受 |
| usage（prompt/completion tokens）回传 | ✅ 两边都拿到 |

**关键发现**：我们所有的确定性旋钮（`thinking:disabled`、`top_k:1`、`temperature:0`）
都能靠 `genai::chat::ChatOptions.extra_body: Option<Value>` 透传——这是
"Provider-specific extra request payload merged by the adapter"，openai/anthropic
adapter 都用 `x_merge` 把它并进请求体。`tool_choice=required` 是 genai 一等公民
（`ToolChoice::Required`）。选型**成立**。

## 5. ⚠️ 已知坑：genai 会剥离 Qwen 模型名的 `-max` 后缀

**这是落地时必须处理的头号地雷。**

genai 把**任何以 `-max` / `-high` / `-low` / `-min` / `-xhigh` 结尾的模型名**
当成"推理强度后缀"剥离（对齐 OpenAI 的 `-high`/`-low` 惯例，见
`genai::chat::ReasoningEffort::from_model_name`）：

- `qwen-vl-max` → 模型名被改成 `qwen-vl`（DashScope 返回 **404 model_not_found**）
  且注入 `reasoning_effort:{effort:"max"}`（DashScope 返回 **400**，只认
  `none/low/medium/high/xhigh`，不认 `max`）。
- 命中**整个 Qwen 系列**：`qwen-vl-max`、`qwen-max`…
- DeepSeek 侧（`deepseek-chat`）**无此问题**。

### 已验证可行的绕过

用 `extra_body` 在 payload 成型后强制覆盖 `model` 和 `reasoning_effort`
（`x_merge` 是浅层覆盖）：

```rust
let opts = ChatOptions::default()
    .with_temperature(0.0)
    .with_extra_body(json!({
        "top_k": 1,                              // 我们的确定性旋钮
        "model": "qwen-vl-max",                  // 覆盖被剥离的名字
        "reasoning_effort": { "effort": "none" } // 覆盖被误注入的 max
    }));
// exec_chat("aliyun::qwen-vl-max", ...) → 200 ✅
```

覆盖后 Qwen 正常 200 返回。

### 落地建议

需要一个薄封装 `GenaiVision`（以及对称的 `GenaiChat`），内部：
- 把我们的私有确定性旋钮塞进 `extra_body`；
- **对 Qwen 类模型自动补 `model` + `reasoning_effort:none` 覆盖**，把这个坑
  对上层透明掉；
- prompt 与 JSON 解析（`judge_split_table`/`transcribe_table` 的裁决逻辑）留在
  core，genai 封装只做"发 prompt+图、回文本"。

## 6. 落地时的横切约束

1. **缓存 key 必须带"模型身份"**。现在 key 里写死 `effective_deepseek_model()`
   （`refine.rs:97` / `cache_key_for`）。换成自定义模型后，key 必须取自
   `model_config` 的实际 model id，否则不同模型会互相污染进程内缓存。
2. **fail-open 不变**。自定义实现抛错时，core 现有逻辑照样搁置/原样返回
   （视觉错误被 `try_vision_verdict` 吞、文本错误冒泡到 fail-open）。
3. **确定性责任转移**。非确定性模型会削弱缓存正确性与跨运行稳定性——文档写明，
   责任在用户。裁决质量未在非默认模型上基准测试过，保真闸 + fail-open 仍兜底。

## 7. FFI 暴露（T2 逃生口，按需做）

若确需支持"用户在 Python/JS 里实现接口"（非 OpenAI 兼容后端）：

- **Python（PyO3）**：定义 `ChatModel`/`VisionModel` 两个 `Protocol`，用户传实现
  对应方法的对象；Rust 侧包一层 `impl ChatClient`，方法内 `Python::attach` 重取 GIL
  调用——跟现有 `progress` 回调（`bindings/python/src/lib.rs:50-59`）同一套路。
  先做同步回调（用户内部用 requests/httpx 阻塞）。
- **JS（napi-rs）**：用 ThreadsafeFunction 回调，天然 async（返回 Promise，
  tokio 桥接 await）。

跨界数据全用 JSON 原生形状（messages=list[dict]、返回 dict），不暴露 Rust 类型。

---

**临时验证项目**：`/tmp/genai-probe`（读 `.env` 里的 key，未内嵌任何密钥；genai 版本
`0.7.0-beta.12`）。用完可删。
