//! the field names serde will read for a struct, asked of serde
//!
//! shared by the two editor suites, because both ask the same question of the
//! same struct: `editors/vscode/` declares a json schema and
//! `editors/intellij/` writes a kotlin map, and each has to name exactly what
//! [`bpd_dap::Configuration`] reads

use std::collections::BTreeSet;

/// the field names serde will read for a struct, asked of serde
///
/// a derived `Deserialize` hands its field list — already renamed, so already
/// camel case — to the deserializer it is given. capturing it there is the
/// difference between a test that checks two lists agree and a test that
/// re-states one of them
pub(crate) fn fields_of<'de, T: serde::Deserialize<'de> + std::fmt::Debug>() -> BTreeSet<String> {
    let mut found = Vec::new();
    let error = T::deserialize(Fields { found: &mut found })
        .expect_err("this deserializer answers a struct with an error, always");
    assert_eq!(
        error.to_string(),
        CAPTURED,
        "the field list was never asked for, so `{}` is not a struct serde reads by name",
        std::any::type_name::<T>()
    );
    assert!(
        !found.is_empty(),
        "a struct with no fields tells this test nothing"
    );
    found.into_iter().collect()
}

/// what the capturing deserializer says once it has the field list
const CAPTURED: &str = "the field list is the whole of what this wanted";

/// a deserializer that answers nothing and records the field list it is offered
struct Fields<'a> {
    found: &'a mut Vec<String>,
}

impl<'de> serde::Deserializer<'de> for Fields<'_> {
    type Error = serde::de::value::Error;

    fn deserialize_struct<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.found
            .extend(fields.iter().map(|field| (*field).to_owned()));
        Err(serde::de::Error::custom(CAPTURED))
    }

    fn deserialize_any<V: serde::de::Visitor<'de>>(
        self,
        _visitor: V,
    ) -> Result<V::Value, Self::Error> {
        Err(serde::de::Error::custom(
            "this deserializer answers a struct with its field list and nothing else",
        ))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map enum identifier ignored_any
    }
}
