//! pbh-core: 领域模型与核心算法。
//!
//! 本 crate 是 PeerBanHelper 行为兼容的 Rust 重实现的核心层，包含：
//! - 领域模型（Peer / Torrent / PeerAddress / BanMetadata 等），JSON 形状与
//!   上游 Java 版（Gson 序列化，null 字段省略，时间为 epoch 毫秒）保持一致；
//! - TranslationComponent 与多语言存储（LangStore），键名与上游 messages.yml 完全一致；
//! - 规则引擎（RuleParser 语义 1:1 移植，含上游 "Hit-" 前缀等历史行为）；
//! - CheckResult / PeerAction 及其合并语义（SKIP > BAN > BAN_FOR_DISCONNECT > NO_ACTION，
//!   同级取更长封禁时长）；
//! - CIDR 集合匹配。
pub mod banlist;
pub mod check;
pub mod ipset;
pub mod lang;
pub mod model;
pub mod rule;
pub mod translate;

pub use check::{CheckResult, PeerAction};
pub use model::{
    BanMetadata, DownloaderBasicInfo, Millis, Peer, PeerAddress, PeerWrapper, Torrent,
    TorrentWrapper,
};
pub use translate::{TcParam, TranslationComponent};

/// 与上游 Java 类名保持一致的模块上下文标识（FQCN）。
/// BanMetadata.context 与 history.module_name 使用这些值，保证进程外数据一致。
pub mod context {
    pub const ABSTRACT_RULE: &str = "com.ghostchu.peerbanhelper.module.AbstractRuleFeatureModule";
    pub const RUN_CHECK_ORGAN: &str =
        "com.ghostchu.peerbanhelper.banpipeline.organ.RunCheckModuleOrgan";
    pub const PEER_ID_BLACKLIST: &str =
        "com.ghostchu.peerbanhelper.module.impl.rule.PeerIdBlacklist";
    pub const CLIENT_NAME_BLACKLIST: &str =
        "com.ghostchu.peerbanhelper.module.impl.rule.ClientNameBlacklist";
    pub const IP_BLACKLIST: &str = "com.ghostchu.peerbanhelper.module.impl.rule.IPBlackList";
}
