//! Serde's wire shapes without a procedural-macro dependency in the server bootstrap.

use serde::de::{self, DeserializeSeed, Visitor};
use std::fmt;

pub(crate) struct Identifier {
    pub names: &'static [&'static str],
    pub aliases: fn(&str) -> Option<usize>,
    pub variant: bool,
}

impl<'de> DeserializeSeed<'de> for Identifier {
    type Value = usize;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<usize, D::Error> {
        deserializer.deserialize_identifier(self)
    }
}

impl<'de> Visitor<'de> for Identifier {
    type Value = usize;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(if self.variant { "variant identifier" } else { "field identifier" })
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<usize, E> {
        match usize::try_from(value) {
            Ok(index) if index < self.names.len() => Ok(index),
            _ if !self.variant => Ok(usize::MAX),
            _ => Err(E::invalid_value(de::Unexpected::Unsigned(value), &self)),
        }
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<usize, E> {
        if let Some(index) =
            self.names.iter().position(|&name| name == value).or_else(|| (self.aliases)(value))
        {
            Ok(index)
        } else if self.variant {
            Err(E::unknown_variant(value, self.names))
        } else {
            Ok(usize::MAX)
        }
    }

    fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<usize, E> {
        match std::str::from_utf8(value) {
            Ok(value) => self.visit_str(value),
            Err(_) if !self.variant => Ok(usize::MAX),
            Err(_) => Err(E::invalid_value(de::Unexpected::Bytes(value), &self)),
        }
    }
}

pub(crate) fn missing<'de, T: serde::Deserialize<'de>, E: de::Error>(
    name: &'static str,
) -> Result<T, E> {
    struct Missing<E>(&'static str, std::marker::PhantomData<E>);
    impl<'de, E: de::Error> serde::Deserializer<'de> for Missing<E> {
        type Error = E;
        fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, E> {
            Err(E::missing_field(self.0))
        }
        fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, E> {
            visitor.visit_none()
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes byte_buf
            i128 u128 unit unit_struct newtype_struct seq tuple tuple_struct map struct enum identifier ignored_any
        }
    }
    T::deserialize(Missing(name, std::marker::PhantomData))
}

macro_rules! wire_missing {
    ($name:expr) => {
        crate::wire::missing($name)?
    };
    ($name:expr, $default:expr) => {
        $default
    };
}

macro_rules! wire_seq_missing {
    ($index:expr, $visitor:expr) => {
        return Err(serde::de::Error::invalid_length($index, $visitor))
    };
    ($index:expr, $visitor:expr, $default:expr) => {
        $default
    };
}

macro_rules! wire_fields {
    ($visitor:ident, $value:ty, $construct:expr; $($field:ident : $ty:ty $(= $default:expr)?),* $(,)?) => {
        struct $visitor;
        impl<'de> serde::de::Visitor<'de> for $visitor {
            type Value = $value;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("protocol fields")
            }
            #[allow(unused_mut)]
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut _seq: A) -> Result<Self::Value, A::Error> {
                #[allow(non_camel_case_types, dead_code)]
                enum Field { $($field),* }
                $(let $field: $ty = match _seq.next_element()? {
                    Some(value) => value,
                    None => wire_seq_missing!(Field::$field as usize, &self $(, $default)?),
                };)*
                Ok($construct)
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                #[allow(non_camel_case_types, dead_code)]
                enum Field { $($field),* }
                $(let mut $field: Option<$ty> = None;)*
                while let Some(index) = map.next_key_seed(crate::wire::Identifier {
                    names: &[$(stringify!($field)),*], aliases: |_| None, variant: false,
                })? {
                    match index {
                        $(index if index == Field::$field as usize => {
                            if $field.is_some() { return Err(serde::de::Error::duplicate_field(stringify!($field))); }
                            $field = Some(map.next_value()?);
                        })*
                        _ => { let _ = map.next_value::<serde::de::IgnoredAny>()?; }
                    }
                }
                $(let $field = match $field { Some(value) => value, None => wire_missing!(stringify!($field) $(, $default)?) };)*
                Ok($construct)
            }
        }
    };
}

macro_rules! wire_struct {
    ($(#[$meta:meta])* $vis:vis struct $name:ident {
        $($(#[$field_meta:meta])* $field_vis:vis $field:ident : $ty:ty $(= $default:expr)?),* $(,)?
    }) => {
        $(#[$meta])* $vis struct $name { $($(#[$field_meta])* $field_vis $field: $ty),* }
        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeStruct as _;
                let fields: &[&str] = &[$(stringify!($field)),*];
                #[allow(unused_mut)]
                let mut state = serializer.serialize_struct(stringify!($name), fields.len())?;
                $(state.serialize_field(stringify!($field), &self.$field)?;)*
                state.end()
            }
        }
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                wire_fields!(FieldsVisitor, $name, $name { $($field),* }; $($field: $ty $(= $default)?),*);
                deserializer.deserialize_struct(stringify!($name), &[$(stringify!($field)),*], FieldsVisitor)
            }
        }
    };
}

macro_rules! wire_name {
    ($variant:ident) => {
        stringify!($variant)
    };
    ($variant:ident [$name:literal $(, $alias:literal)*]) => {
        $name
    };
}

macro_rules! wire_alias {
    ($value:ident; $variant:ident) => { false };
    ($value:ident; $variant:ident [$name:literal $(, $alias:literal)*]) => { false $(|| $value == $alias)* };
}

macro_rules! wire_variant_serialize {
    ($serializer:ident, $index:expr, $enum:expr, $variant:expr;) => {
        $serializer.serialize_unit_variant($enum, $index, $variant)
    };
    ($serializer:ident, $index:expr, $enum:expr, $variant:expr; ($value:ident : $ty:ty)) => {
        $serializer.serialize_newtype_variant($enum, $index, $variant, $value)
    };
    ($serializer:ident, $index:expr, $enum:expr, $variant:expr; {$($field:ident : $ty:ty),*}) => {{
        use serde::ser::SerializeStructVariant as _;
        let fields: &[&str] = &[$(stringify!($field)),*];
        #[allow(unused_mut)]
        let mut state = $serializer.serialize_struct_variant($enum, $index, $variant, fields.len())?;
        $(state.serialize_field(stringify!($field), $field)?;)*
        state.end()
    }};
}

macro_rules! wire_variant_deserialize {
    ($access:ident, $enum:ident, $variant:ident;) => {{
        serde::de::VariantAccess::unit_variant($access)?;
        Ok($enum::$variant)
    }};
    ($access:ident, $enum:ident, $variant:ident; ($ty:ty)) => {
        serde::de::VariantAccess::newtype_variant::<$ty>($access).map($enum::$variant)
    };
    ($access:ident, $enum:ident, $variant:ident; {$($field:ident : $ty:ty),*}) => {{
        wire_fields!(FieldsVisitor, $enum, $enum::$variant { $($field),* }; $($field: $ty),*);
        serde::de::VariantAccess::struct_variant($access, &[$(stringify!($field)),*], FieldsVisitor)
    }};
}

macro_rules! wire_binding {
    ($ty:ty; $value:ident) => {
        $value
    };
}

macro_rules! wire_enum {
    ($(#[$meta:meta])* $vis:vis enum $name:ident {
        $($(#[$variant_meta:meta])* $variant:ident $([$wire:literal $(, $alias:literal)*])? $(($ty:ty))? $({$($field:ident : $field_ty:ty),* $(,)?})?),* $(,)?
    }) => {
        $(#[$meta])* $vis enum $name {
            $($(#[$variant_meta])* $variant $(($ty))? $({$($field: $field_ty),*})?),*
        }
        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                enum Index { $($variant),* }
                match self {
                    $(Self::$variant $((wire_binding!($ty; value)))? $({$($field),*})? => {
                        wire_variant_serialize!(serializer, Index::$variant as u32, stringify!($name), wire_name!($variant $([$wire $(, $alias)*])?); $((value: $ty))? $({$($field: $field_ty),*})?)
                    }),*
                }
            }
        }
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                enum Index { $($variant),* }
                const NAMES: &[&str] = &[$(wire_name!($variant $([$wire $(, $alias)*])?)),*];
                struct EnumVisitor;
                impl<'de> serde::de::Visitor<'de> for EnumVisitor {
                    type Value = $name;
                    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(stringify!($name)) }
                    fn visit_enum<A: serde::de::EnumAccess<'de>>(self, access: A) -> Result<$name, A::Error> {
                        let (index, variant) = access.variant_seed(crate::wire::Identifier {
                            names: NAMES, variant: true,
                            aliases: |_value| {
                                $(if wire_alias!(_value; $variant $([$wire $(, $alias)*])?) { return Some(Index::$variant as usize); })*
                                None
                            },
                        })?;
                        match index {
                            $(index if index == Index::$variant as usize => wire_variant_deserialize!(variant, $name, $variant; $(($ty))? $({$($field: $field_ty),*})?),)*
                            _ => unreachable!(),
                        }
                    }
                }
                deserializer.deserialize_enum(stringify!($name), NAMES, EnumVisitor)
            }
        }
    };
}

macro_rules! wire_newtype {
    ($(#[$meta:meta])* $vis:vis struct $name:ident($field_vis:vis $ty:ty);) => {
        $(#[$meta])* $vis struct $name($field_vis $ty);
        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_newtype_struct(stringify!($name), &self.0)
            }
        }
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct NewtypeVisitor;
                impl<'de> serde::de::Visitor<'de> for NewtypeVisitor {
                    type Value = $name;
                    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(stringify!($name)) }
                    fn visit_newtype_struct<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<$name, D::Error> {
                        serde::Deserialize::deserialize(deserializer).map($name)
                    }
                }
                deserializer.deserialize_newtype_struct(stringify!($name), NewtypeVisitor)
            }
        }
    };
}
