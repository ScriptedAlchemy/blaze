use super::{ExpandMacro, ExpandMacroData, ExpnGlobals, FlatTree};
use paths::Utf8PathBuf;

impl serde::Serialize for ExpnGlobals {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;
        let mut state = serializer.serialize_struct("ExpnGlobals", 3)?;
        state.serialize_field("def_site", &self.def_site)?;
        state.serialize_field("call_site", &self.call_site)?;
        state.serialize_field("mixed_site", &self.mixed_site)?;
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for ExpnGlobals {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        wire_fields!(FieldsVisitor, ExpnGlobals, ExpnGlobals { serialize, def_site, call_site, mixed_site };
            serialize: bool = false, def_site: usize, call_site: usize, mixed_site: usize);
        deserializer.deserialize_struct(
            "ExpnGlobals",
            &["serialize", "def_site", "call_site", "mixed_site"],
            FieldsVisitor,
        )
    }
}

impl serde::Serialize for ExpandMacroData {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;
        let globals = !self.has_global_spans.skip_serializing_if();
        let spans = !self.span_data_table.is_empty();
        let mut state = serializer
            .serialize_struct("ExpandMacroData", 3 + usize::from(globals) + usize::from(spans))?;
        state.serialize_field("macro_body", &self.macro_body)?;
        state.serialize_field("macro_name", &self.macro_name)?;
        state.serialize_field("attributes", &self.attributes)?;
        if globals {
            state.serialize_field("has_global_spans", &self.has_global_spans)?;
        }
        if spans {
            state.serialize_field("span_data_table", &self.span_data_table)?;
        }
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for ExpandMacroData {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        wire_fields!(FieldsVisitor, ExpandMacroData, ExpandMacroData { macro_body, macro_name, attributes, has_global_spans, span_data_table };
            macro_body: FlatTree, macro_name: String, attributes: Option<FlatTree>,
            has_global_spans: ExpnGlobals = Default::default(), span_data_table: Vec<u32> = Vec::new());
        deserializer.deserialize_struct(
            "ExpandMacroData",
            &["macro_body", "macro_name", "attributes", "has_global_spans", "span_data_table"],
            FieldsVisitor,
        )
    }
}

impl serde::Serialize for ExpandMacro {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;
        let mut state = serializer.serialize_map(None)?;
        state.serialize_entry("lib", &self.lib)?;
        state.serialize_entry("env", &self.env)?;
        state.serialize_entry("current_dir", &self.current_dir)?;
        state.serialize_entry("macro_body", &self.data.macro_body)?;
        state.serialize_entry("macro_name", &self.data.macro_name)?;
        state.serialize_entry("attributes", &self.data.attributes)?;
        if !self.data.has_global_spans.skip_serializing_if() {
            state.serialize_entry("has_global_spans", &self.data.has_global_spans)?;
        }
        if !self.data.span_data_table.is_empty() {
            state.serialize_entry("span_data_table", &self.data.span_data_table)?;
        }
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for ExpandMacro {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        wire_fields!(FieldsVisitor, ExpandMacro, ExpandMacro { lib, env, current_dir,
            data: ExpandMacroData { macro_body, macro_name, attributes, has_global_spans, span_data_table } };
            lib: Utf8PathBuf, env: Vec<(String, String)>, current_dir: Option<String>,
            macro_body: FlatTree, macro_name: String, attributes: Option<FlatTree>,
            has_global_spans: ExpnGlobals = Default::default(), span_data_table: Vec<u32> = Vec::new());
        deserializer.deserialize_map(FieldsVisitor)
    }
}
