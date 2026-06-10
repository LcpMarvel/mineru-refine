// 内部稳定 ID（SPEC §4a）：入口分配，op 按规则产新/继承，出口剥除。
// 绝不用 array index 跨 op 寻址。

import type { MineruItem, RefItem } from "./types.ts";

export type IdGen = () => string;

export function createIdGen(prefix = "it"): IdGen {
  let n = 0;
  return () => `${prefix}_${String(++n).padStart(4, "0")}`;
}

/** 入口：深拷贝输入并为每个 item 分配稳定 ID。返回的 nextId 供 merge/split 产新 ID。 */
export function assignIds(items: MineruItem[]): { ref: RefItem[]; nextId: IdGen } {
  const nextId = createIdGen();
  const ref = items.map((item) => ({ id: nextId(), item: structuredClone(item) }));
  return { ref, nextId };
}

/** 出口：剥除内部 ID，返回纯 MinerU schema（§2 透明性）。 */
export function stripIds(ref: RefItem[]): MineruItem[] {
  return ref.map((r) => structuredClone(r.item));
}

export function indexOfId(items: RefItem[], id: string): number {
  return items.findIndex((r) => r.id === id);
}

/** 找不到即抛——上游传了过期/错误 ID 必须立刻暴露，不静默吞。 */
export function mustIndexOfId(items: RefItem[], id: string): number {
  const i = indexOfId(items, id);
  if (i < 0) throw new Error(`未知 item ID: ${id}（可能已被 merge/drop，或从未存在）`);
  return i;
}
