//! CIDR 集合匹配（对应上游 IPAddress 库的 contains 语义的最简实现）。
//! MVP 使用排序 Vec + 线性匹配；封禁表/忽略列表规模（数千条目）下性能足够，
//! 后续可替换为 radix trie（见重写方案 M2）。
use ipnet::IpNet;
use std::net::IpAddr;

#[derive(Clone, Debug, Default)]
pub struct CidrSet {
    nets: Vec<IpNet>,
}

/// 解析用户输入：支持 "1.2.3.4"、"1.2.3.0/24"、"::1"、"2000::/3"
pub fn parse_cidr(s: &str) -> Option<IpNet> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.contains('/') {
        s.parse::<IpNet>().ok()
    } else {
        s.parse::<IpAddr>().ok().map(crate::model::host_net)
    }
}

impl CidrSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_strs<'a, I: IntoIterator<Item = &'a str>>(items: I) -> (Self, Vec<String>) {
        let mut nets = Vec::new();
        let mut errors = Vec::new();
        for item in items {
            match parse_cidr(item) {
                Some(net) => nets.push(net),
                None => errors.push(item.to_string()),
            }
        }
        (Self { nets }, errors)
    }

    pub fn push(&mut self, net: IpNet) {
        self.nets.push(net);
    }

    pub fn is_empty(&self) -> bool {
        self.nets.is_empty()
    }

    pub fn len(&self) -> usize {
        self.nets.len()
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        self.nets.iter().any(|n| n.contains(&ip))
    }

    /// 返回命中的第一个网络（用于生成规则文案，对应上游遍历 ips 找 ra）
    pub fn find(&self, ip: IpAddr) -> Option<IpNet> {
        self.nets.iter().find(|n| n.contains(&ip)).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = &IpNet> {
        self.nets.iter()
    }
}
