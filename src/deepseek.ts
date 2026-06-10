// DeepSeek 裸 API 客户端（无 SDK）。仅封装一次 POST /chat/completions。
// 接入约定：deepseek-v4-pro / thinking disabled / tool_choice required / temperature 0。

const ENDPOINT = "https://api.deepseek.com/chat/completions";
const MODEL = "deepseek-v4-pro";

export type Tool = {
  type: "function";
  function: { name: string; description: string; parameters: object };
};

export type ToolCall = {
  id: string;
  type: "function";
  function: { name: string; arguments: string }; // arguments 是 JSON 字符串
};

export type Message =
  | { role: "system" | "user"; content: string }
  | { role: "assistant"; content: string | null; tool_calls?: ToolCall[] }
  | { role: "tool"; tool_call_id: string; content: string };

export type Usage = {
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
  prompt_cache_hit_tokens?: number;
  prompt_cache_miss_tokens?: number;
};

export type ChatResult = {
  message: { role: "assistant"; content: string | null; tool_calls?: ToolCall[]; reasoning_content?: string };
  finish_reason: string;
  usage: Usage;
};

const MAX_ATTEMPTS = 3;
const RETRYABLE_STATUS = new Set([429, 500, 502, 503, 504]);

export async function chat(
  messages: Message[],
  tools: Tool[],
  opts?: { toolChoice?: "required" | "auto" | "none" },
): Promise<ChatResult> {
  // 早抛：缺 key 立即失败，不静默降级（项目约定）。.env 的 DEEPSEEK_APIKEY 或 ~/.ragent_profile 的 RAGENT_DEEPSEEK_APIKEY 均可。
  const key = process.env.DEEPSEEK_APIKEY ?? process.env.RAGENT_DEEPSEEK_APIKEY;
  if (!key) throw new Error("DEEPSEEK_APIKEY / RAGENT_DEEPSEEK_APIKEY 均未设置 — 在 .env 里填一个");

  const body = JSON.stringify({
    model: MODEL,
    messages,
    tools,
    tool_choice: opts?.toolChoice ?? "required",
    temperature: 0, // thinking disabled 下生效
    thinking: { type: "disabled" }, // 绕开 reasoning_content 回传的 400 雷
    stream: false,
  });

  // 瞬态失败重试：并发跑 loop 时偶发 socket 断开 / 429 / 5xx，重试兜掉；4xx 业务错误立刻抛。
  let lastErr: Error | undefined;
  for (let attempt = 1; attempt <= MAX_ATTEMPTS; attempt++) {
    if (attempt > 1) await Bun.sleep(attempt * 1500);
    let res: Response;
    try {
      res = await fetch(ENDPOINT, {
        method: "POST",
        headers: { Authorization: `Bearer ${key}`, "Content-Type": "application/json" },
        body,
      });
    } catch (e) {
      lastErr = new Error(`DeepSeek 网络错误（第 ${attempt}/${MAX_ATTEMPTS} 次）: ${(e as Error).message}`);
      continue;
    }

    if (!res.ok) {
      const text = await res.text();
      if (RETRYABLE_STATUS.has(res.status)) {
        lastErr = new Error(`DeepSeek HTTP ${res.status}（第 ${attempt}/${MAX_ATTEMPTS} 次）: ${text.slice(0, 300)}`);
        continue;
      }
      throw new Error(`DeepSeek HTTP ${res.status}: ${text}`);
    }

    const json: any = await res.json();
    const choice = json.choices?.[0];
    if (!choice) throw new Error(`DeepSeek 无 choices: ${JSON.stringify(json)}`);

    return {
      message: choice.message,
      finish_reason: choice.finish_reason,
      usage: json.usage,
    };
  }
  throw lastErr ?? new Error("DeepSeek 重试耗尽");
}
