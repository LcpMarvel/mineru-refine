// 测试共享件：golden fixture 构造 + 脚本化 mock LLM（不打真 API）。

import type { ChatResult, Message, Tool } from "../src/deepseek.ts";
import type { ChatFn } from "../src/loop.ts";
import type { MineruItem, SuspectKind } from "../src/types.ts";

export function bbox(y0: number): number[] {
  return [50, y0, 550, y0 + 20];
}

/**
 * golden fixture：一份带 5 类可处理 quirk 的"文档"。
 * it_0001 真标题 / it_0002 伪标题 / it_0003+it_0004 跨页断句 /
 * it_0005 页码混入 / it_0006 markdown 链接残留 / it_0007 干净表格。
 */
export function goldenInput(): MineruItem[] {
  return [
    { type: "text", text: "第一章 总则", text_level: 1, page_idx: 0, bbox: bbox(40) },
    {
      type: "text",
      text: "公司应当建立健全战略管理体系，确保战略目标的实现。",
      text_level: 1, // 伪标题：含逗号 + 句末标点
      page_idx: 0,
      bbox: bbox(80),
    },
    { type: "text", text: "战略管理是指公司为实现长期发展目标而进行的", page_idx: 0, bbox: bbox(120) },
    { type: "text", text: "一系列计划、执行与评估活动。", page_idx: 1, bbox: bbox(40) },
    { type: "text", text: "- 3 -", page_idx: 1, bbox: bbox(780) },
    { type: "text", text: "详见[公司官网](http://example.com)发布的文件。", page_idx: 1, bbox: bbox(120) },
    {
      type: "table",
      table_body: "<table><tr><td>指标</td><td>目标值</td></tr></table>",
      table_caption: ["表1 绩效指标"],
      page_idx: 1,
      bbox: bbox(200),
    },
  ];
}

/** goldenInput 经正确清洗后的期望输出（golden fixture 断言目标）。 */
export function goldenExpected(): MineruItem[] {
  const input = goldenInput();
  return [
    input[0]!,
    { ...input[1]!, text_level: undefined } as MineruItem, // demote（实际是删字段，断言时单独处理）
    {
      ...input[2]!,
      text: "战略管理是指公司为实现长期发展目标而进行的一系列计划、执行与评估活动。",
      bbox: [50, 40, 550, 140], // union(it3.bbox, it4.bbox)
      page_idx: 0, // 取首块
    },
    // it_0005 页码被 drop
    { ...input[5]!, text: "详见公司官网发布的文件。" }, // strip md_link
    input[6]!,
  ].map((it) => {
    const clone = structuredClone(it) as MineruItem;
    if (clone.text_level === undefined) delete clone.text_level;
    return clone;
  });
}

type MockDecision = { name: string; args: Record<string, unknown> };
type KindHandler = (suspectId: string, evidence: string, messages: Message[]) => MockDecision;

/** 脚本化"假 LLM"：从 user 消息解析疑点 kind/id，按 kind 直接回对应 op 的 tool_call。 */
export function makeMockChat(overrides: Partial<Record<SuspectKind, KindHandler>> = {}): ChatFn & { calls: number } {
  let callId = 0;

  const defaults: Partial<Record<SuspectKind, KindHandler>> = {
    pseudo_heading: (id) => ({ name: "demote", args: { id } }),
    cross_page_break: (id, evidence) => {
      const idB = /后块=(it_\d+)/.exec(evidence)?.[1];
      if (!idB) throw new Error(`mock 无法从证据解析后块 ID: ${evidence}`);
      return { name: "merge", args: { idA: id, idB } };
    },
    page_artifact: (id) => ({ name: "drop", args: { id } }),
    residual_markup: (id) => ({ name: "strip", args: { id, pattern: "md_link" } }),
    giant_block: (id) => ({ name: "dismiss", args: { id, reason: "mock 默认不拆" } }),
    empty_table: (id) => ({ name: "drop", args: { id } }),
    split_table: (id, evidence) => {
      const idB = /后块=(it_\d+)/.exec(evidence)?.[1];
      if (!idB) throw new Error(`mock 无法从证据解析后块 ID: ${evidence}`);
      return { name: "mergeTable", args: { idA: id, idB } };
    },
    split_list: (id, evidence) => {
      const idB = /后块=(it_\d+)/.exec(evidence)?.[1];
      if (!idB) throw new Error(`mock 无法从证据解析后块 ID: ${evidence}`);
      return { name: "mergeList", args: { idA: id, idB } };
    },
  };

  const fn = async (messages: Message[], _tools: Tool[]): Promise<ChatResult> => {
    fn.calls++;
    const user = messages.find((m) => m.role === "user");
    const content = user && "content" in user ? String(user.content) : "";
    const m = /当前疑点：\[(\w+)\] item (it_\d+)/.exec(content);
    if (!m) throw new Error(`mock 无法解析疑点描述: ${content.slice(0, 120)}`);
    const kind = m[1] as SuspectKind;
    const suspectId = m[2]!;
    const evidence = /证据：(.+)/.exec(content)?.[1] ?? "";

    const handler = overrides[kind] ?? defaults[kind];
    if (!handler) throw new Error(`mock 未定义 ${kind} 的处理`);
    const { name, args } = handler(suspectId, evidence, messages);

    return {
      message: {
        role: "assistant",
        content: null,
        tool_calls: [
          { id: `call_${++callId}`, type: "function", function: { name, arguments: JSON.stringify(args) } },
        ],
      },
      finish_reason: "tool_calls",
      usage: { prompt_tokens: 100, completion_tokens: 20, total_tokens: 120 },
    };
  };
  fn.calls = 0;
  return fn;
}

/** 一调用就炸的"假 LLM"：验证 fail-open 与"无疑点不打 LLM"。 */
export function explodingChat(): ChatFn & { calls: number } {
  const fn = async (): Promise<ChatResult> => {
    fn.calls++;
    throw new Error("LLM 不可用（测试注入）");
  };
  fn.calls = 0;
  return fn;
}
