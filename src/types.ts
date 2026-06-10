// 数据模型。MineruItem 镜像 MinerU content_list item 真实字段，
// 未知字段原样保留（schema 透明性）。

export type MineruItem = {
  type: "text" | "header" | "table" | "list" | "page_number" | "image" | (string & {});
  text?: string;
  text_level?: number; // 仅 text 且为标题时存在
  table_body?: string; // 仅 table，HTML
  table_caption?: string[]; // 仅 table
  list_items?: string[]; // 仅 list
  img_path?: string; // 仅 image
  page_idx?: number; // 0-based
  bbox?: number[]; // [x0, y0, x1, y1]
  [k: string]: unknown; // MinerU 其它字段原样透传
};

// 内部表示：item + 稳定 ID。ID 出口前剥除，绝不进输出 schema。
export type RefItem = { id: string; item: MineruItem };

/** MinerU 已正确分类的"页面家具"：不是 quirk、不进 worklist；跨页连续性判断/merge 时可跳过。 */
export const PAGE_FURNITURE_TYPES: ReadonlySet<string> = new Set(["page_number", "header", "footer"]);

// ── 探测器疑点 ──
export type SuspectKind =
  // 可处理（有对应 op）
  | "pseudo_heading"
  | "cross_page_break"
  | "giant_block"
  | "page_artifact"
  | "residual_markup"
  | "split_table" // 跨页两个有体表格 → mergeTable / dismiss
  | "split_list" // 跨页两个列表 → mergeList / dismiss
  | "empty_table" // 零内容空壳表（MinerU 自行跨页合并后留下的占位）→ drop
  // 只标记、无 op（标记后不做处理）
  | "caption_issue";

export type WorkItem = {
  kind: SuspectKind;
  itemId: string;
  evidence: string;
  hasOp: boolean;
};

// ── op 调用。参数一律稳定 ID，不用 index ──
export type StripPattern = "md_link" | "latex_dollar" | "latex_block" | "latex_command" | "escaped_dollar" | "html_tag";

export type OpCall =
  | { op: "merge"; idA: string; idB: string }
  | { op: "split"; id: string; offset: number }
  | { op: "demote"; id: string }
  | { op: "promote"; id: string; level: number }
  | { op: "reorder"; idsInOrder: string[] }
  | { op: "drop"; id: string }
  | { op: "strip"; id: string; pattern: StripPattern }
  | { op: "mergeTable"; idA: string; idB: string }
  | { op: "mergeList"; idA: string; idB: string; joinSeam?: boolean };

export type OpName = OpCall["op"];

// ── provenance（纯削减模式不加字，恒为空，结构保留备用）──
export type ProvenanceEntry = {
  itemId: string;
  field: "text" | "table_caption" | "list_items";
  charStart: number;
  charEnd: number;
  origin: "agent";
  op: string;
  confidence: number;
  note?: string;
};

export type RemovedSpan = { itemId: string; text: string; reason: string };

// ── 报告 ──
export type RefineReport = {
  iterations: number;
  opCounts: Record<string, number>;
  dismissed: number;
  removedSpans: RemovedSpan[];
  violations: number; // 保真闸回滚次数
  tokenUsage: { prompt: number; completion: number };
  failOpen: boolean;
};

export type RefineResult = {
  items: MineruItem[];
  provenance: ProvenanceEntry[];
  report: RefineReport;
};
