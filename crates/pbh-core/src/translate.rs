//! TranslationComponent：与上游 Gson TypeAdapter 输出一致的 `{key, params}` 结构。
//! params 元素要么是字符串（Java 侧 `String.valueOf(param)`），要么是嵌套的 TranslationComponent。
use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub enum TcParam {
    S(String),
    Tc(Box<TranslationComponent>),
}

impl TcParam {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            TcParam::S(s) => Some(s),
            TcParam::Tc(_) => None,
        }
    }
}

impl From<String> for TcParam {
    fn from(s: String) -> Self {
        TcParam::S(s)
    }
}

impl From<&str> for TcParam {
    fn from(s: &str) -> Self {
        TcParam::S(s.to_owned())
    }
}

impl From<TranslationComponent> for TcParam {
    fn from(tc: TranslationComponent) -> Self {
        TcParam::Tc(Box::new(tc))
    }
}

impl Serialize for TcParam {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            TcParam::S(s) => serializer.serialize_str(s),
            TcParam::Tc(tc) => tc.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for TcParam {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ParamVisitor;
        impl<'de> Visitor<'de> for ParamVisitor {
            type Value = TcParam;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("string or TranslationComponent object")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<TcParam, E> {
                Ok(TcParam::S(v.to_owned()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<TcParam, E> {
                Ok(TcParam::S(v))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<TcParam, A::Error> {
                let tc = TranslationComponent::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(TcParam::Tc(Box::new(tc)))
            }
        }
        deserializer.deserialize_any(ParamVisitor)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TranslationComponent {
    pub key: String,
    pub params: Vec<TcParam>,
}

impl TranslationComponent {
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            params: Vec::new(),
        }
    }

    pub fn with(key: impl Into<String>, params: Vec<TcParam>) -> Self {
        Self {
            key: key.into(),
            params,
        }
    }

    /// 便捷构造：全部参数为字符串
    pub fn strs(key: impl Into<String>, params: impl IntoIterator<Item = String>) -> Self {
        Self {
            key: key.into(),
            params: params.into_iter().map(TcParam::S).collect(),
        }
    }
}

impl Serialize for TranslationComponent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("TranslationComponent", 2)?;
        s.serialize_field("key", &self.key)?;
        s.serialize_field("params", &self.params)?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for TranslationComponent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            key: Option<String>,
            #[serde(default)]
            params: Vec<TcParam>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(TranslationComponent {
            key: raw.key.unwrap_or_default(),
            params: raw.params,
        })
    }
}

