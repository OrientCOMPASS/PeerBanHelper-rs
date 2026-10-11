//! 内存日志环形缓冲 + 广播（对应上游 JListAppender.ringDeque + NewLogEntryCreatedEvent）。
use parking_lot::RwLock;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;

/// 对应上游 WebUILogEntryDTO(time, thread, level, content, seq)
#[derive(Clone, Debug, Serialize)]
pub struct LogEntryDto {
    pub time: i64,
    pub thread: String,
    pub level: String,
    pub content: String,
    pub seq: i64,
}

pub struct LogHub {
    ring: RwLock<VecDeque<LogEntryDto>>,
    tx: broadcast::Sender<LogEntryDto>,
    seq: AtomicI64,
    capacity: usize,
}

impl LogHub {
    pub fn new(capacity: usize) -> Arc<Self> {
        let (tx, _) = broadcast::channel(capacity.max(64) * 2);
        Arc::new(Self {
            ring: RwLock::new(VecDeque::with_capacity(capacity)),
            tx,
            seq: AtomicI64::new(0),
            capacity,
        })
    }

    pub fn push(&self, level: &str, thread: &str, content: String) {
        let entry = LogEntryDto {
            time: pbh_core::model::Millis::now().millis(),
            thread: thread.to_string(),
            level: level.to_string(),
            content,
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
        };
        {
            let mut ring = self.ring.write();
            if ring.len() >= self.capacity {
                ring.pop_front();
            }
            ring.push_back(entry.clone());
        }
        let _ = self.tx.send(entry);
    }

    pub fn history(&self) -> Vec<LogEntryDto> {
        self.ring.read().iter().cloned().collect()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LogEntryDto> {
        self.tx.subscribe()
    }
}
