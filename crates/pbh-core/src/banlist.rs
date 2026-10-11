//! 内存封禁表（从 engine 下沉至 core，供 AutoRangeBan 等模块只读访问）。
//! 键为主机前缀块（/32、/128），对应上游 BanList 的 DualIPv4v6AssociativeTries +
//! toPrefixBlock 行为。MVP 为 HashMap 实现；CIDR 范围封禁条目（AutoRangeBan 产生的
//! 更大前缀）通过 contains_covering 做包含判断。
use crate::model::BanMetadata;
use ipnet::IpNet;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use crate::model::Millis;
use parking_lot::RwLock;

pub type BanListHandle = Arc<RwLock<BanList>>;

#[derive(Default)]
pub struct BanList {
    map: HashMap<IpNet, BanMetadata>,
}

impl BanList {
    pub fn new() -> Self {
        Self::default()
    }

    /// 精确主机键查询
    pub fn contains(&self, net: &IpNet) -> bool {
        self.map.contains_key(net)
    }

    /// 上游 banList.contains(peerAddress)：主机被任一封禁条目覆盖（含 CIDR 条目）
    pub fn contains_addr(&self, ip: IpAddr) -> bool {
        let host = crate::model::host_net(ip);
        if self.map.contains_key(&host) {
            return true;
        }
        self.map.keys().any(|n| n.prefix_len() < host.prefix_len() && n.contains(&ip))
    }

    pub fn insert(&mut self, net: IpNet, meta: BanMetadata) -> Option<BanMetadata> {
        self.map.insert(net, meta)
    }

    pub fn remove(&mut self, net: &IpNet) -> Option<BanMetadata> {
        self.map.remove(net)
    }

    /// 移除被给定网络包含的全部条目（对应上游 removeElementsContainedBy）
    pub fn remove_contained(&mut self, net: &IpNet) -> Vec<BanMetadata> {
        let keys: Vec<IpNet> = self
            .map
            .keys()
            .filter(|k| net.contains(&k.addr()) && k.prefix_len() >= net.prefix_len())
            .copied()
            .collect();
        let mut out = Vec::new();
        for k in keys {
            if let Some(m) = self.map.remove(&k) {
                out.push(m);
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn keys(&self) -> Vec<IpNet> {
        self.map.keys().copied().collect()
    }

    pub fn snapshot(&self) -> Vec<(IpNet, BanMetadata)> {
        self.map.iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&IpNet, &BanMetadata)> {
        self.map.iter()
    }

    /// 移除已到解封时间的条目（对应 removeExpiredBans）
    pub fn remove_expired(&mut self, now: Millis) -> Vec<(IpNet, BanMetadata)> {
        let expired: Vec<IpNet> = self
            .map
            .iter()
            .filter(|(_, m)| m.unban_at <= now)
            .map(|(k, _)| *k)
            .collect();
        let mut out = Vec::with_capacity(expired.len());
        for net in expired {
            if let Some(m) = self.map.remove(&net) {
                out.push((net, m));
            }
        }
        out
    }
}
