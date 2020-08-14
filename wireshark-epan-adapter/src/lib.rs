#![allow(non_shorthand_field_patterns)]

#[rustfmt::skip]
#[allow(warnings)]
// #[allow(non_upper_case_globals, non_camel_case_types, non_snake_case)]
mod sys;

mod plugin;
pub use self::plugin::{
    Plugin, NameDescriptor, FieldDescriptor, PrefFilenameDescriptor, DissectorDescriptor, Dissector,
};
use self::plugin::Contexts;

pub mod dissector;
