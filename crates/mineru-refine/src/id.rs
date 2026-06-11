// 内部稳定 ID：入口分配，op 按规则产新/继承，出口剥除。
// 绝不用 array index 跨 op 寻址。

use crate::types::{MineruItem, RefItem};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// 线程安全的递增 ID 生成器（并行裁决的多个对话共用一个）。
#[derive(Clone)]
pub struct IdGen {
    prefix: &'static str,
    counter: Arc<AtomicU64>,
}

impl Default for IdGen {
    fn default() -> Self {
        Self::new("it")
    }
}

impl IdGen {
    pub fn new(prefix: &'static str) -> Self {
        Self {
            prefix,
            counter: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn next(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("{}_{:04}", self.prefix, n)
    }
}

/// 入口：深拷贝输入并为每个 item 分配稳定 ID。返回的 IdGen 供 merge/split 产新 ID。
pub fn assign_ids(items: &[MineruItem]) -> (Vec<RefItem>, IdGen) {
    let id_gen = IdGen::default();
    let ref_items = items
        .iter()
        .map(|item| RefItem {
            id: id_gen.next(),
            item: item.clone(),
        })
        .collect();
    (ref_items, id_gen)
}

/// 出口：剥除内部 ID，返回纯 MinerU schema（schema 透明性）。
pub fn strip_ids(ref_items: &[RefItem]) -> Vec<MineruItem> {
    ref_items.iter().map(|r| r.item.clone()).collect()
}

pub fn index_of_id(items: &[RefItem], id: &str) -> Option<usize> {
    items.iter().position(|r| r.id == id)
}

/// 找不到即报错——上游传了过期/错误 ID 必须立刻暴露，不静默吞。
pub fn must_index_of_id(items: &[RefItem], id: &str) -> Result<usize, String> {
    index_of_id(items, id)
        .ok_or_else(|| format!("未知 item ID: {id}（可能已被 merge/drop，或从未存在）"))
}
