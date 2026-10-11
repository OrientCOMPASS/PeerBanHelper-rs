//! CheckResult / PeerAction：与上游 module.CheckResult、module.PeerAction 语义一致。
use crate::context;
use crate::translate::TranslationComponent;
use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerAction {
    NoAction,
    BanForDisconnect,
    Ban,
    Skip,
}

impl PeerAction {
    /// Java 枚举 ordinal（NO_ACTION=0, BAN_FOR_DISCONNECT=1, BAN=2, SKIP=3）
    pub fn rank(self) -> u8 {
        match self {
            PeerAction::NoAction => 0,
            PeerAction::BanForDisconnect => 1,
            PeerAction::Ban => 2,
            PeerAction::Skip => 3,
        }
    }

    pub fn exclude_from_report(self) -> bool {
        matches!(self, PeerAction::BanForDisconnect)
    }

    pub fn exclude_from_display(self) -> bool {
        matches!(self, PeerAction::BanForDisconnect)
    }
}

#[derive(Clone, Debug)]
pub struct CheckResult {
    /// Java 类 FQCN（见 pbh_core::context）
    pub module_context: String,
    pub action: PeerAction,
    /// 封禁时长（毫秒），0 表示使用全局时长
    pub duration: i64,
    pub rule: TranslationComponent,
    pub reason: TranslationComponent,
    pub structured_data: Map<String, Value>,
}

impl CheckResult {
    pub fn new(
        module_context: impl Into<String>,
        action: PeerAction,
        duration: i64,
        rule: TranslationComponent,
        reason: TranslationComponent,
        structured_data: Map<String, Value>,
    ) -> Self {
        Self {
            module_context: module_context.into(),
            action,
            duration,
            rule,
            reason,
            structured_data,
        }
    }

    fn na() -> TranslationComponent {
        TranslationComponent::new("N/A")
    }

    /// AbstractRuleFeatureModule.OK_CHECK_RESULT
    pub fn pass() -> Self {
        let mut sd = Map::new();
        sd.insert("status".into(), Value::String("pass".into()));
        Self::new(
            context::ABSTRACT_RULE,
            PeerAction::NoAction,
            0,
            Self::na(),
            TranslationComponent::new("Check passed"),
            sd,
        )
    }

    /// AbstractRuleFeatureModule.HANDSHAKING_CHECK_RESULT
    pub fn handshaking() -> Self {
        let mut sd = Map::new();
        sd.insert("status".into(), Value::String("handshaking".into()));
        Self::new(
            context::ABSTRACT_RULE,
            PeerAction::NoAction,
            0,
            Self::na(),
            TranslationComponent::new("Peer handshaking"),
            sd,
        )
    }

    /// AbstractRuleFeatureModule.TEAPOT_CHECK_RESULT
    #[allow(dead_code)]
    pub fn teapot() -> Self {
        let mut sd = Map::new();
        sd.insert("status".into(), Value::String("teapot".into()));
        Self::new(
            context::ABSTRACT_RULE,
            PeerAction::NoAction,
            0,
            Self::na(),
            TranslationComponent::new("I'm a teapot"),
            sd,
        )
    }

    /// RunCheckModuleOrgan.checkIfPossibleBadConfig 命中忽略地址时的 SKIP 结果
    pub fn ignored_address() -> Self {
        let mut sd = Map::new();
        sd.insert("type".into(), Value::String("ignoredAddresses".into()));
        Self::new(
            context::RUN_CHECK_ORGAN,
            PeerAction::Skip,
            0,
            TranslationComponent::new("general-rule-ignored-address"),
            TranslationComponent::new("general-reason-skip-ignored-peers"),
            sd,
        )
    }

    /// 上游 DigestionSession 合并语义：等级更高者胜（SKIP > BAN > BAN_FOR_DISCONNECT > NO_ACTION）；
    /// 等级相同时保留更长的封禁时长；其余情况后来者覆盖。
    pub fn merge(existing: CheckResult, incoming: CheckResult) -> CheckResult {
        if existing.action.rank() > incoming.action.rank() {
            return existing;
        }
        if existing.action.rank() == incoming.action.rank()
            && existing.duration > incoming.duration
        {
            return existing;
        }
        incoming
    }
}
