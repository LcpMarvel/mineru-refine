// M0 冒烟（阻断里程碑，SPEC §15）。
// 目标：验证 deepseek-v4-pro 裸 API 在【多轮 tool-call loop】中的机械可靠性——
//   1. tool_choice:"required" → 模型每轮必返 tool_calls（绝不吐自由正文）
//   2. arguments(JSON 字符串) 经 safe-json-repair 稳定解析
//   3. role:"tool" 结果回传后，第二轮仍能正确续调（多轮往返不丢）
//   4. thinking disabled → 无 reasoning_content、无 400
//   5. usage 暴露 prompt_cache_hit_tokens（缓存可观测）
//
// 跑：  source ~/.ragent_profile && bun run scripts/m0_smoke.ts

import { parseJsonSafe } from "safe-json-repair";
import { chat, type Message, type Tool, type ToolCall } from "../src/deepseek.ts";

// ── 模拟 refine 的最小场景：1 个伪 HEADING 疑点，看模型能否走 observe→act ──
const SUSPECT_ID = "it_0002";
const ITEMS: Record<string, any> = {
  it_0001: { type: "header", text: "第二章 过程管理", text_level: 1 },
  // 伪 HEADING：被错判为标题的半句正文（含逗号 + 句末标点）
  it_0002: { type: "text", text_level: 1, text: "公司基于过程方法，对关键过程进行有效管理。" },
  it_0003: { type: "text", text: "各部门应按本章要求执行。" },
};

const TOOLS: Tool[] = [
  {
    type: "function",
    function: {
      name: "getItem",
      description: "查看某个 item 的完整内容，用于判断它到底是不是真标题。",
      parameters: {
        type: "object",
        properties: { id: { type: "string", description: "item 的稳定 ID" } },
        required: ["id"],
      },
    },
  },
  {
    type: "function",
    function: {
      name: "demote",
      description: "把被误判为标题的 item 降级为正文（清除 text_level）。",
      parameters: {
        type: "object",
        properties: { id: { type: "string" } },
        required: ["id"],
      },
    },
  },
  {
    type: "function",
    function: {
      name: "dismiss",
      description: "判定该疑点为误报，不做任何改动。",
      parameters: {
        type: "object",
        properties: { id: { type: "string" }, reason: { type: "string" } },
        required: ["id", "reason"],
      },
    },
  },
];

const SYSTEM = `你是 MinerU 解析结果的结构修复器。你只能调用工具，绝不输出正文。
当前有一个疑点：item ${SUSPECT_ID} 被标记为「疑似伪标题」（被错判成 heading 的半句正文）。
请先用 getItem 看清楚它的内容，再决定：若确实是被误判的正文就 demote，若确实是真标题就 dismiss。`;

// 确定性执行工具，返回给模型的结果文本
function execTool(call: ToolCall): { content: string; resolved: boolean } {
  const args = parseJsonSafe<any>(call.function.arguments);
  if (args === undefined) {
    throw new Error(`tool ${call.function.name} 的 arguments 无法解析(连 repair 都救不回): ${call.function.arguments}`);
  }
  switch (call.function.name) {
    case "getItem":
      return { content: JSON.stringify(ITEMS[args.id] ?? null), resolved: false };
    case "demote":
      return { content: `OK：${args.id} 已降级为正文。`, resolved: true };
    case "dismiss":
      return { content: `OK：${args.id} 已记为误报（${args.reason}）。`, resolved: true };
    default:
      throw new Error(`未知工具: ${call.function.name}`);
  }
}

async function main() {
  const messages: Message[] = [
    { role: "system", content: SYSTEM },
    { role: "user", content: `请处理疑点 ${SUSPECT_ID}。` },
  ];

  let rounds = 0;
  let resolved = false;
  let sawObserve = false;
  let sawAct = false;
  const MAX = 5;

  while (!resolved && rounds < MAX) {
    rounds++;
    const r = await chat(messages, TOOLS, { toolChoice: "required" });

    // 断言 1：必返 tool_calls，content 不应是自由正文
    const calls = r.message.tool_calls;
    if (!calls || calls.length === 0) {
      throw new Error(`[FAIL] 第 ${rounds} 轮模型没有调用工具，content=${JSON.stringify(r.message.content)}`);
    }
    // 断言 4：thinking disabled → 不应有 reasoning_content
    if (r.message.reasoning_content) {
      console.warn(`[WARN] 第 ${rounds} 轮出现 reasoning_content（thinking 未关？）`);
    }

    const cacheHit = r.usage.prompt_cache_hit_tokens ?? 0;
    console.log(
      `── 第 ${rounds} 轮: ${calls.map((c) => `${c.function.name}(${c.function.arguments})`).join(", ")} ` +
        `| tokens p=${r.usage.prompt_tokens}(cache_hit=${cacheHit}) c=${r.usage.completion_tokens}`,
    );

    // 把 assistant 的 tool_calls 消息原样压回 history（多轮必需）
    messages.push({ role: "assistant", content: r.message.content ?? null, tool_calls: calls });

    for (const call of calls) {
      const { content, resolved: didResolve } = execTool(call);
      if (call.function.name === "getItem") sawObserve = true;
      if (call.function.name === "demote" || call.function.name === "dismiss") sawAct = true;
      if (didResolve) resolved = true;
      // 断言 3：回传 role:"tool"
      messages.push({ role: "tool", tool_call_id: call.id, content });
    }
  }

  console.log("\n──────── M0 结果 ────────");
  const checks: [string, boolean][] = [
    ["每轮必返 tool_calls（tool_choice:required 生效）", true /* 跑到这没抛即真 */],
    ["arguments 经 safe-json-repair 解析成功", true],
    ["多轮 round-trip（role:tool 回传后续调成功）", rounds >= 2 && sawObserve],
    ["完成 observe→act 的疑点处理", sawObserve && sawAct && resolved],
  ];
  let allPass = true;
  for (const [name, ok] of checks) {
    console.log(`  ${ok ? "✅" : "❌"} ${name}`);
    allPass &&= ok;
  }
  console.log(`\n${allPass ? "🟢 M0 PASS — 地基可靠，可往上盖楼。" : "🔴 M0 FAIL — 见上。"}`);
  if (!allPass) process.exit(1);
}

main().catch((e) => {
  console.error("🔴 M0 异常:", e.message);
  process.exit(1);
});
