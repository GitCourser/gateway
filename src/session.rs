//! 会话粘连存储。
//!
//! 设计要点：
//! - 纯内存、无 TTL、单进程部署；绑定关系在进程重启后丢失，会话将重新选号。
//! - 容量上限由 `SESSION_MAX` 环境变量控制（默认 10），超出后按 FIFO 逐出最早插入的条目。
//! - 并发安全：`RwLock` 保护的 `VecDeque`（插入顺序）+ `HashMap`（快速查找）。
//!   已存在的会话命中时不改变其位置（纯 FIFO，不是 LRU）。
//!
//! 「锁内选号」约定：为保证同一会话的所有并发请求路由到同一上游，
//! 调用方在未命中时应持有写锁完成选号并写入（见 `SessionStore::entry_or_insert_with`），
//! 使同会话请求串行化，而不同会话之间互不阻塞。

use std::collections::{HashMap, VecDeque};
use tokio::sync::RwLock;

/// 一个会话绑定到的上游路由目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBinding {
    /// 上游提供商 ID（对应配置文件 `providers` 字典的键）。
    pub provider: String,
    /// 选号得到的账号 ID。
    pub account_id: i64,
}

#[derive(Debug, Default)]
struct Inner {
    order: VecDeque<String>,
    bindings: HashMap<String, SessionBinding>,
}

/// FIFO 容量受控的会话粘连存储。
pub struct SessionStore {
    max: usize,
    inner: RwLock<Inner>,
}

impl SessionStore {
    pub fn new(max: usize) -> Self {
        Self {
            max: max.max(1),
            inner: RwLock::new(Inner::default()),
        }
    }

    #[allow(dead_code)]
    pub fn max_capacity(&self) -> usize {
        self.max
    }

    #[allow(dead_code)]
    pub async fn len(&self) -> usize {
        self.inner.read().await.bindings.len()
    }

    #[allow(dead_code)]
    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.bindings.is_empty()
    }

    /// 查找会话绑定；命中返回 `Some`，不改变其在 FIFO 中的位置。
    pub async fn get(&self, session_id: &str) -> Option<SessionBinding> {
        self.inner.read().await.bindings.get(session_id).cloned()
    }

    /// 插入或更新绑定；容量已满时逐出最早插入且当前不在插入/更新中的条目。
    ///
    /// 注意：更新已存在会话的绑定时，其 FIFO 位置保持不变。
    #[allow(dead_code)]
    pub async fn insert(&self, session_id: String, binding: SessionBinding) {
        let mut inner = self.inner.write().await;
        insert_locked(&mut inner, self.max, session_id, binding);
    }

    /// 未命中时在写锁内执行选号并写入；命中则直接返回已有绑定。
    ///
    /// 同一会话的并发请求在此串行化：第一个请求完成选号写入后，
    /// 后续请求通过二次检查直接复用该绑定，保证「同会话同上游」。
    pub async fn entry_or_insert_with<F, Fut, E>(
        &self,
        session_id: String,
        select: F,
    ) -> Result<(SessionBinding, bool), E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<SessionBinding, E>>,
    {
        let mut inner = self.inner.write().await;
        if let Some(existing) = inner.bindings.get(&session_id) {
            return Ok((existing.clone(), false));
        }
        let binding = select().await?;
        insert_locked(&mut inner, self.max, session_id, binding.clone());
        Ok((binding, true))
    }
}

/// 在已持有写锁的前提下执行插入与 FIFO 逐出（供锁定入口与测试复用）。
fn insert_locked(inner: &mut Inner, max: usize, session_id: String, binding: SessionBinding) {
    if !inner.bindings.contains_key(&session_id) && inner.bindings.len() >= max.max(1) {
        // 逐出队头（最早插入）条目，跳过恰好与本次插入同键的极端情况。
        while let Some(oldest) = inner.order.pop_front() {
            if inner.bindings.remove(&oldest).is_some() {
                break;
            }
        }
    }
    if inner.bindings.insert(session_id.clone(), binding).is_none() {
        inner.order.push_back(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(provider: &str, account: i64) -> SessionBinding {
        SessionBinding {
            provider: provider.to_string(),
            account_id: account,
        }
    }

    #[tokio::test]
    async fn fifo_evicts_oldest() {
        let store = SessionStore::new(3);
        for i in 0..3 {
            store.insert(format!("s{}", i), binding("p", i)).await;
        }
        assert_eq!(store.len().await, 3);
        store.insert("s3".to_string(), binding("p", 3)).await;

        // 最早的 s0 被逐出，其余保留。
        assert_eq!(store.len().await, 3);
        assert_eq!(store.get("s0").await, None);
        assert_eq!(store.get("s1").await, Some(binding("p", 1)));
        assert_eq!(store.get("s3").await, Some(binding("p", 3)));
    }

    #[tokio::test]
    async fn updating_existing_key_keeps_fifo_order() {
        let store = SessionStore::new(2);
        store.insert("a".to_string(), binding("p1", 1)).await;
        store.insert("b".to_string(), binding("p2", 2)).await;
        // 更新 a 的绑定不应改变其插入顺序，也不应触发逐出。
        store.insert("a".to_string(), binding("p1", 99)).await;
        assert_eq!(store.get("a").await, Some(binding("p1", 99)));
        store.insert("c".to_string(), binding("p3", 3)).await;
        // 队头是 a（仍最早插入），被逐出。
        assert_eq!(store.get("a").await, None);
        assert_eq!(store.get("b").await, Some(binding("p2", 2)));
        assert_eq!(store.get("c").await, Some(binding("p3", 3)));
    }

    #[tokio::test]
    async fn entry_or_insert_with_dedupes_concurrent_selects() {
        let store = std::sync::Arc::new(SessionStore::new(10));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let st = store.clone();
            handles.push(tokio::spawn(async move {
                st.entry_or_insert_with("shared".to_string(), || async {
                    Ok::<SessionBinding, std::convert::Infallible>(binding("provider_x", 7))
                })
                .await
            }));
        }
        let mut inserted_count = 0;
        for h in handles {
            let (_, inserted) = h.await.unwrap().unwrap();
            if inserted {
                inserted_count += 1;
            }
        }
        // 8 个并发请求中只有 1 个真正执行了选号写入。
        assert_eq!(inserted_count, 1);
        assert_eq!(store.get("shared").await, Some(binding("provider_x", 7)));
    }

    #[tokio::test]
    async fn entry_or_insert_with_does_not_overwrite_existing() {
        let store = SessionStore::new(10);
        store.insert("s".to_string(), binding("p1", 1)).await;
        let (bound, inserted) = store
            .entry_or_insert_with("s".to_string(), || async {
                Ok::<SessionBinding, std::convert::Infallible>(binding("p2", 2))
            })
            .await
            .unwrap();
        assert!(!inserted);
        assert_eq!(bound, binding("p1", 1));
    }

    #[tokio::test]
    async fn capacity_floor_is_one() {
        let store = SessionStore::new(0);
        store.insert("a".to_string(), binding("p", 1)).await;
        store.insert("b".to_string(), binding("p", 2)).await;
        assert_eq!(store.len().await, 1);
        assert_eq!(store.get("b").await, Some(binding("p", 2)));
    }
}
