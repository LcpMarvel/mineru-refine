// M4：tool-use loop 守卫专项（观察工具往返 / 防震荡 / 轮数耗尽强制搁置 / 坏 JSON 修复）。

import { describe, expect, test } from "bun:test";
import type { ChatResult, Message, Tool, ToolCall } from "../src/deepseek.ts";
import { assignIds } from "../src/id.ts";
import { runLoop } from "../src/loop.ts";
import { goldenInput } from "./helpers.ts";

function call(name: string, args: unknown, id = "c1"): ToolCall {
  return { id, type: "function", function: { name, arguments: typeof args === "string" ? args : JSON.stringify(args) } };
}

function reply(calls: ToolCall[]): ChatResult {
  return {
    message: { role: "assistant", content: null, tool_calls: calls },
    finish_reason: "tool_calls",
    usage: { prompt_tokens: 50, completion_tokens: 10, total_tokens: 60 },
  };
}

/** 按脚本逐轮回放的假 LLM；transcript 收集每轮收到的 tool 结果便于断言。 */
function scripted(steps: ((messages: Message[]) => ChatResult)[]): {
  fn: (messages: Message[], tools: Tool[]) => Promise<ChatResult>;
  rounds: () => number;
} {
  let i = 0;
  return {
    fn: async (messages) => {
      const step = steps[Math.min(i, steps.length - 1)]!;
      i++;
      return step(messages);
    },
    rounds: () => i,
  };
}

describe("观察工具往返", () => {
  test("LLM 先 outline+getItems+peekPage+whyFlagged 再裁决，观察结果含目标块", async () => {
    const { ref, nextId } = assignIds(goldenInput());
    const seen: string[] = [];
    const llm = scripted([
      () => reply([call("outline", {})]),
      (messages) => {
        seen.push(String((messages.at(-1) as { content: string }).content));
        return reply([call("getItems", { id: "it_0002", before: 1, after: 1 })]);
      },
      (messages) => {
        seen.push(String((messages.at(-1) as { content: string }).content));
        return reply([call("peekPage", { id: "it_0002" })]);
      },
      (messages) => {
        seen.push(String((messages.at(-1) as { content: string }).content));
        return reply([call("whyFlagged", { id: "it_0002" })]);
      },
      (messages) => {
        seen.push(String((messages.at(-1) as { content: string }).content));
        return reply([call("demote", { id: "it_0002" })]);
      },
      // 后续疑点全 dismiss
      (messages) => {
        const m = /当前疑点：\[\w+\] item (it_\d+)/.exec(String((messages[1] as { content: string }).content));
        return reply([call("dismiss", { id: m![1], reason: "测试收尾" })]);
      },
    ]);

    // 脚本按调用次序回放，必须严格串行
    const r = await runLoop(ref, nextId, { chatFn: llm.fn, concurrency: 1, log: () => {} });
    expect(seen[0]).toContain("it_0002"); // outline 含伪标题（它有 text_level）
    expect(seen[1]).toContain(">>>"); // getItems 高亮目标块
    expect(seen[2]).toContain("── 第 0 页 ──"); // peekPage 分页展示
    expect(seen[3]).toContain("pseudo_heading"); // whyFlagged 给证据
    expect(r.opCounts.demote).toBe(1);
    expect(r.items.find((x) => x.id === "it_0002")!.item.text_level).toBeUndefined();
  });
});

describe("防震荡（§10）", () => {
  test("merge 产物立刻被 split 拒绝，LLM 收到拒绝后 dismiss", async () => {
    const { ref, nextId } = assignIds(goldenInput());
    const rejections: string[] = [];
    // 策略：cross_page_break 疑点 → merge（产新块 it_0008）；
    // 其它疑点一律先试 split it_0008（应被防震荡拒）、收到拒绝再 dismiss。
    const chatFn = async (messages: Message[]): Promise<ChatResult> => {
      const content = String((messages[1] as { content: string }).content);
      const kind = /当前疑点：\[(\w+)\]/.exec(content)![1];
      const id = /item (it_\d+)/.exec(content)![1]!;
      if (kind === "cross_page_break") {
        const idB = /后块=(it_\d+)/.exec(content)![1];
        return reply([call("merge", { idA: id, idB })]);
      }
      const last = messages.at(-1)!;
      if (last.role === "tool" && String((last as { content: string }).content).includes("防震荡")) {
        rejections.push("rejected");
        return reply([call("dismiss", { id, reason: "被防震荡拦截" })]);
      }
      return reply([call("split", { id: "it_0008", offset: 10 })]);
    };

    // split it_0008 依赖 merge 先落地，必须严格串行
    const r = await runLoop(ref, nextId, { chatFn, concurrency: 1, log: () => {} });
    expect(r.opCounts.merge).toBe(1);
    expect(rejections.length).toBeGreaterThan(0); // 防震荡确实拦了
    expect(r.opCounts.split).toBeUndefined();
  });
});

describe("轮数与坏 JSON", () => {
  test("单疑点轮数耗尽 → 强制搁置（计入 dismissed），循环仍收敛", async () => {
    const { ref, nextId } = assignIds(goldenInput());
    // 永远只观察、从不裁决
    const llm = scripted([() => reply([call("getItems", { id: "it_0002" })])]);
    const r = await runLoop(ref, nextId, { chatFn: llm.fn, maxRoundsPerSuspect: 3, log: () => {} });
    expect(r.dismissed).toBe(4); // 4 个 hasOp 疑点全部轮数耗尽被搁置
    expect(Object.keys(r.opCounts)).toHaveLength(0);
  });

  test("arguments 坏 JSON 经 safe-json-repair 仍可解析（尾逗号/缺引号场景）", async () => {
    const { ref, nextId } = assignIds(goldenInput());
    const llm = scripted([
      (messages) => {
        const content = String((messages[1] as { content: string }).content);
        const kind = /当前疑点：\[(\w+)\]/.exec(content)![1];
        const id = /item (it_\d+)/.exec(content)![1]!;
        if (kind === "pseudo_heading") {
          return reply([call("demote", `{"id": "${id}",}`)]); // 尾逗号坏 JSON
        }
        return reply([call("dismiss", { id, reason: "skip" })]);
      },
    ]);
    const r = await runLoop(ref, nextId, { chatFn: llm.fn, log: () => {} });
    expect(r.opCounts.demote).toBe(1);
  });
});
