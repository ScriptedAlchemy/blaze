//! Independent derive-based oracles for the wire contract at
//! cf529a630a67ddbe4f3f50e281bc485c08643134.

use std::fmt::Debug;

use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{ProcMacroKind, bidirectional_protocol::msg as bidi, legacy_protocol::msg as legacy};

mod reference {
    use serde_derive::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize)]
    pub(crate) enum ProcMacroKind {
        CustomDerive,
        Attr,
        #[serde(alias = "Bang", rename = "FuncLike")]
        Bang,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub(crate) struct FlatTree {
        subtree: Vec<u32>,
        literal: Vec<u32>,
        punct: Vec<u32>,
        ident: Vec<u32>,
        token_tree: Vec<u32>,
        text: Vec<String>,
    }

    #[derive(Debug, Default, Serialize, Deserialize)]
    pub(crate) enum SpanMode {
        #[default]
        Id,
        RustAnalyzer,
    }

    #[derive(Debug, Default, Serialize, Deserialize)]
    #[serde(default)]
    pub(crate) struct ServerConfig {
        span_mode: SpanMode,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub(crate) struct PanicMessage(String);

    pub(crate) mod bidirectional {
        #![expect(clippy::disallowed_types)]

        use std::{
            collections::{HashMap, HashSet},
            ops::Range,
        };

        use serde_derive::{Deserialize, Serialize};

        use super::{FlatTree, PanicMessage, ProcMacroKind, ServerConfig};

        #[derive(Debug, Serialize, Deserialize)]
        pub(crate) enum SubRequest {
            FilePath {
                file_id: u32,
            },
            SourceText {
                file_id: u32,
                ast_id: u32,
                start: u32,
                end: u32,
            },
            LocalFilePath {
                file_id: u32,
            },
            LineColumn {
                file_id: u32,
                ast_id: u32,
                offset: u32,
            },
            ByteRange {
                file_id: u32,
                ast_id: u32,
                start: u32,
                end: u32,
            },
            SpanSource {
                file_id: u32,
                ast_id: u32,
                start: u32,
                end: u32,
                ctx: u32,
            },
            SpanParent {
                file_id: u32,
                ast_id: u32,
                start: u32,
                end: u32,
                ctx: u32,
            },
            SpanJoin {
                file_id: u32,
                ast_id_first: u32,
                start_first: u32,
                end_first: u32,
                ctx_first: u32,
                ast_id_second: u32,
                start_second: u32,
                end_second: u32,
                ctx_second: u32,
            },
        }

        #[derive(Debug, Serialize, Deserialize)]
        pub(crate) enum SubResponse {
            FilePathResult { name: String },
            SourceTextResult { text: Option<String> },
            LocalFilePathResult { name: Option<String> },
            LineColumnResult { line: u32, column: u32 },
            ByteRangeResult { range: Range<usize> },
            SpanSourceResult { file_id: u32, ast_id: u32, start: u32, end: u32, ctx: u32 },
            SpanParentResult { parent_span: Option<ParentSpan> },
            SpanJoinResult { span: Option<SpanJoin> },
            Cancel { reason: String },
        }

        #[derive(Debug, Serialize, Deserialize)]
        pub(crate) struct ParentSpan {
            file_id: u32,
            ast_id: u32,
            start: u32,
            end: u32,
            ctx: u32,
        }

        #[derive(Debug, Serialize, Deserialize)]
        pub(crate) struct SpanJoin {
            ast_id: u32,
            start: u32,
            end: u32,
            ctx: u32,
        }

        #[derive(Debug, Serialize, Deserialize)]
        pub(crate) enum BidirectionalMessage {
            Request(Request),
            Response(Response),
            SubRequest(SubRequest),
            SubResponse(SubResponse),
        }

        #[derive(Debug, Serialize, Deserialize)]
        pub(crate) enum Request {
            ListMacros(ListMacros),
            ExpandMacro(Box<ExpandMacro>),
            ApiVersionCheck(ApiVersionCheck),
            SetConfig(ServerConfig),
        }

        #[derive(Debug, Serialize, Deserialize)]
        pub(crate) enum Response {
            ListMacros(Result<Vec<(String, ProcMacroKind)>, String>),
            ApiVersionCheck(u32),
            SetConfig(ServerConfig),
            ExpandMacro(Result<ExpandMacroResponse, PanicMessage>),
        }

        #[derive(Debug, Serialize, Deserialize)]
        pub(crate) struct ListMacros {
            dylib_path: String,
        }

        #[derive(Debug, Serialize, Deserialize)]
        pub(crate) struct ApiVersionCheck {}

        #[derive(Debug, Serialize, Deserialize)]
        pub(crate) struct ExpandMacro {
            lib: String,
            env: Vec<(String, String)>,
            current_dir: Option<String>,
            data: ExpandMacroData,
        }

        #[derive(Debug, Serialize, Deserialize)]
        pub(crate) struct ExpandMacroResponse {
            tree: FlatTree,
            span_data_table: Vec<u32>,
            tracked_env_vars: HashMap<Box<str>, Option<Box<str>>>,
            tracked_paths: HashSet<Box<str>>,
        }

        #[derive(Debug, Serialize, Deserialize)]
        pub(crate) struct ExpandMacroData {
            macro_body: FlatTree,
            macro_name: String,
            attributes: Option<FlatTree>,
            #[serde(default)]
            has_global_spans: ExpnGlobals,
            #[serde(default)]
            span_data_table: Vec<u32>,
        }

        #[derive(Debug, Default, Serialize, Deserialize)]
        pub(crate) struct ExpnGlobals {
            def_site: usize,
            call_site: usize,
            mixed_site: usize,
        }
    }

    pub(crate) mod legacy {
        use serde_derive::{Deserialize, Serialize};

        use super::{FlatTree, PanicMessage, ProcMacroKind, ServerConfig};

        #[derive(Debug, Serialize, Deserialize)]
        pub(crate) enum Request {
            ApiVersionCheck {},
            ListMacros { dylib_path: String },
            ExpandMacro(Box<ExpandMacro>),
            SetConfig(ServerConfig),
        }

        #[derive(Debug, Serialize, Deserialize)]
        pub(crate) enum Response {
            ApiVersionCheck(u32),
            ListMacros(Result<Vec<(String, ProcMacroKind)>, String>),
            ExpandMacro(Result<FlatTree, PanicMessage>),
            SetConfig(ServerConfig),
            ExpandMacroExtended(Result<ExpandMacroExtended, PanicMessage>),
        }

        #[derive(Debug, Serialize, Deserialize)]
        pub(crate) struct ExpandMacroExtended {
            tree: FlatTree,
            span_data_table: Vec<u32>,
        }

        #[derive(Debug, Serialize, Deserialize)]
        pub(crate) struct ExpandMacro {
            lib: String,
            env: Vec<(String, String)>,
            current_dir: Option<String>,
            #[serde(flatten)]
            data: ExpandMacroData,
        }

        #[derive(Debug, Serialize, Deserialize)]
        pub(crate) struct ExpandMacroData {
            macro_body: FlatTree,
            macro_name: String,
            attributes: Option<FlatTree>,
            #[serde(default, skip_serializing_if = "ExpnGlobals::skip_serializing_if")]
            has_global_spans: ExpnGlobals,
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            span_data_table: Vec<u32>,
        }

        #[derive(Debug, Default, Serialize, Deserialize)]
        pub(crate) struct ExpnGlobals {
            #[serde(default, skip_serializing)]
            serialize: bool,
            def_site: usize,
            call_site: usize,
            mixed_site: usize,
        }

        impl ExpnGlobals {
            fn skip_serializing_if(&self) -> bool {
                !self.serialize
            }
        }
    }
}

use reference::{bidirectional as ref_bidi, legacy as ref_legacy};

fn json_contract<T, R>(input: &str) -> (T, R)
where
    T: Serialize + DeserializeOwned + Debug,
    R: Serialize + DeserializeOwned + Debug,
{
    let actual: T = serde_json::from_str(input).unwrap();
    let reference: R = serde_json::from_str(input).unwrap();
    let actual_json = serde_json::to_vec(&actual).unwrap();
    let reference_json = serde_json::to_vec(&reference).unwrap();
    assert_eq!(actual_json, reference_json, "JSON bytes for {input}");

    let actual_cross: T = serde_json::from_slice(&reference_json).unwrap();
    let reference_cross: R = serde_json::from_slice(&actual_json).unwrap();
    // Legacy `serialize` is accepted on input but omitted on output. Compare
    // both decoders after the same hop rather than assuming idempotence.
    assert_eq!(
        serde_json::to_vec(&actual_cross).unwrap(),
        serde_json::to_vec(&reference_cross).unwrap(),
        "JSON cross-decode for {input}",
    );
    (actual, reference)
}

fn wire_contract<T, R>(fixture: &Value)
where
    T: Serialize + DeserializeOwned + Debug,
    R: Serialize + DeserializeOwned + Debug,
{
    let (actual, reference) = json_contract::<T, R>(&fixture.to_string());
    let actual_bytes = postcard::to_allocvec(&actual).unwrap();
    let reference_bytes = postcard::to_allocvec(&reference).unwrap();
    assert_eq!(actual_bytes, reference_bytes, "postcard bytes for {fixture}");
    let actual_cross: T = postcard::from_bytes(&reference_bytes).unwrap();
    let reference_cross: R = postcard::from_bytes(&actual_bytes).unwrap();
    assert_eq!(postcard::to_allocvec(&actual_cross).unwrap(), reference_bytes);
    assert_eq!(postcard::to_allocvec(&reference_cross).unwrap(), actual_bytes);

    let actual_frame = crate::transport::postcard::encode(&actual).unwrap();
    let reference_frame = postcard::to_allocvec_cobs(&reference).unwrap();
    assert_eq!(actual_frame, reference_frame, "COBS frame for {fixture}");
    let actual_cross: T = crate::transport::postcard::decode(&mut reference_frame.clone()).unwrap();
    let reference_cross: R = postcard::from_bytes_cobs(&mut actual_frame.clone()).unwrap();
    assert_eq!(postcard::to_allocvec(&actual_cross).unwrap(), reference_bytes);
    assert_eq!(postcard::to_allocvec(&reference_cross).unwrap(), actual_bytes);

    for end in 0..reference_bytes.len() {
        assert!(
            postcard::from_bytes::<R>(&reference_bytes[..end]).is_err(),
            "reference accepted truncated postcard at {end} for {fixture}",
        );
        assert!(
            postcard::from_bytes::<T>(&reference_bytes[..end]).is_err(),
            "production accepted truncated postcard at {end} for {fixture}",
        );
    }
}

fn json_acceptance<T, R>(input: &str, accepted: bool)
where
    T: Serialize + DeserializeOwned + Debug,
    R: Serialize + DeserializeOwned + Debug,
{
    let actual = serde_json::from_str::<T>(input);
    let reference = serde_json::from_str::<R>(input);
    assert_eq!(reference.is_ok(), accepted, "reference: {input}: {reference:?}");
    assert_eq!(actual.is_ok(), accepted, "production: {input}: {actual:?}");
    if let (Ok(actual), Ok(reference)) = (actual, reference) {
        assert_eq!(serde_json::to_vec(&actual).unwrap(), serde_json::to_vec(&reference).unwrap());
    }
}

fn struct_json_contract<T, R>(fixture: Value, required: &[&str])
where
    T: Serialize + DeserializeOwned + Debug,
    R: Serialize + DeserializeOwned + Debug,
{
    json_contract::<T, R>(&fixture.to_string());
    let object = fixture.as_object().unwrap();
    let reversed = object
        .iter()
        .rev()
        .map(|(key, value)| format!("{}:{value}", serde_json::to_string(key).unwrap()))
        .collect::<Vec<_>>()
        .join(",");
    json_contract::<T, R>(&format!("{{{reversed}}}"));

    let mut unknown = fixture.clone();
    unknown["future_field"] = json!({"nested": [null, {"ignored": [1, 2, 3]}]});
    json_acceptance::<T, R>(&unknown.to_string(), true);
    for &field in required {
        let mut missing = fixture.clone();
        missing.as_object_mut().unwrap().remove(field).unwrap();
        json_acceptance::<T, R>(&missing.to_string(), false);
    }
    for (field, value) in object {
        let duplicate = format!(
            "{{{}:{value},{}",
            serde_json::to_string(field).unwrap(),
            &fixture.to_string()[1..],
        );
        json_acceptance::<T, R>(&duplicate, false);
    }
}

fn tree() -> Value {
    json!({
        "subtree": [4294967295u32, 0, 0, 4],
        "literal": [128, 1],
        "punct": [127, 64, 1],
        "ident": [0, 0, 1, 1],
        "token_tree": [3, 7, 1, 4],
        "text": ["struct", "r#例\"\n"],
    })
}

fn globals() -> Value {
    json!({"def_site": 127, "call_site": 128, "mixed_site": 16384})
}

fn bidi_data() -> Value {
    json!({
        "macro_body": tree(), "macro_name": "derive_例", "attributes": tree(),
        "has_global_spans": globals(), "span_data_table": [0, 127, 128, 16384, 4294967295u32],
    })
}

fn bidi_expand() -> Value {
    json!({
        "lib": "/tmp/例/lib macro.so", "env": [["KEY", "quoted\"\n"], ["KEY", ""]],
        "current_dir": "/tmp/例", "data": bidi_data(),
    })
}

fn bidi_expanded() -> Value {
    // Separate randomized HashMaps/Sets have stable byte order only at <= 1 entry.
    json!({
        "tree": tree(), "span_data_table": [0, 128, 4294967295u32],
        "tracked_env_vars": {"KEY": "value\n例"}, "tracked_paths": ["/tmp/例"],
    })
}

fn legacy_data(serialize: bool) -> Value {
    let mut global = globals();
    global["serialize"] = json!(serialize);
    json!({
        "macro_body": tree(), "macro_name": "expand_例", "attributes": tree(),
        "has_global_spans": global, "span_data_table": [0, 128, 4294967295u32],
    })
}

fn legacy_expand(serialize: bool) -> Value {
    let mut fixture = legacy_data(serialize);
    let object = fixture.as_object_mut().unwrap();
    object.insert("lib".into(), json!("/tmp/例/lib macro.so"));
    object.insert("env".into(), json!([["KEY", "value\n"], ["KEY", ""]]));
    object.insert("current_dir".into(), json!("/tmp/例"));
    fixture
}

fn subrequests() -> Vec<Value> {
    vec![
        json!({"FilePath": {"file_id": 4294967295u32}}),
        json!({"SourceText": {"file_id": 1, "ast_id": 127, "start": 128, "end": 16384}}),
        json!({"LocalFilePath": {"file_id": 0}}),
        json!({"LineColumn": {"file_id": 1, "ast_id": 2, "offset": 4294967295u32}}),
        json!({"ByteRange": {"file_id": 3, "ast_id": 4, "start": 5, "end": 6}}),
        json!({"SpanSource": {"file_id": 7, "ast_id": 8, "start": 9, "end": 10, "ctx": 11}}),
        json!({"SpanParent": {"file_id": 12, "ast_id": 13, "start": 14, "end": 15, "ctx": 16}}),
        json!({"SpanJoin": {
            "file_id": 17, "ast_id_first": 18, "start_first": 19, "end_first": 20,
            "ctx_first": 21, "ast_id_second": 127, "start_second": 128,
            "end_second": 16384, "ctx_second": 4294967295u32,
        }}),
    ]
}

fn subresponses() -> Vec<Value> {
    vec![
        json!({"FilePathResult": {"name": "/tmp/例.rs"}}),
        json!({"SourceTextResult": {"text": "fn 例() {\n\"quoted\"\n}"}}),
        json!({"SourceTextResult": {"text": null}}),
        json!({"SourceTextResult": {"text": ""}}),
        json!({"LocalFilePathResult": {"name": "/tmp/例.rs"}}),
        json!({"LocalFilePathResult": {"name": null}}),
        json!({"LineColumnResult": {"line": 128, "column": 16384}}),
        json!({"ByteRangeResult": {"range": {"start": 127, "end": usize::MAX}}}),
        json!({"SpanSourceResult": {"file_id": 1, "ast_id": 2, "start": 3, "end": 4, "ctx": 5}}),
        json!({"SpanParentResult": {"parent_span": {"file_id": 6, "ast_id": 7, "start": 8, "end": 9, "ctx": 10}}}),
        json!({"SpanParentResult": {"parent_span": null}}),
        json!({"SpanJoinResult": {"span": {"ast_id": 11, "start": 12, "end": 13, "ctx": 14}}}),
        json!({"SpanJoinResult": {"span": null}}),
        json!({"Cancel": {"reason": "cancelled\n例\""}}),
    ]
}

fn bidi_requests() -> Vec<Value> {
    vec![
        json!({"ListMacros": {"dylib_path": "/tmp/例/lib.so"}}),
        json!({"ExpandMacro": bidi_expand()}),
        json!({"ApiVersionCheck": {}}),
        json!({"SetConfig": {"span_mode": "Id"}}),
        json!({"SetConfig": {"span_mode": "RustAnalyzer"}}),
    ]
}

fn bidi_responses() -> Vec<Value> {
    let mut absent_env = bidi_expanded();
    absent_env["tracked_env_vars"] = json!({"UNSET": null});
    vec![
        json!({"ListMacros": {"Ok": [["derive", "CustomDerive"], ["attr", "Attr"], ["bang", "Bang"]]}}),
        json!({"ListMacros": {"Ok": []}}),
        json!({"ListMacros": {"Err": "load failed\n例"}}),
        json!({"ApiVersionCheck": 4294967295u32}),
        json!({"SetConfig": {"span_mode": "RustAnalyzer"}}),
        json!({"ExpandMacro": {"Ok": bidi_expanded()}}),
        json!({"ExpandMacro": {"Ok": absent_env}}),
        json!({"ExpandMacro": {"Err": "panic\n例\""}}),
    ]
}

#[test]
fn bidirectional_all_variants_match_derive_json_and_postcard() {
    for fixture in subrequests() {
        wire_contract::<bidi::SubRequest, ref_bidi::SubRequest>(&fixture);
        wire_contract::<bidi::BidirectionalMessage, ref_bidi::BidirectionalMessage>(
            &json!({"SubRequest": fixture}),
        );
    }
    for fixture in subresponses() {
        wire_contract::<bidi::SubResponse, ref_bidi::SubResponse>(&fixture);
        wire_contract::<bidi::BidirectionalMessage, ref_bidi::BidirectionalMessage>(
            &json!({"SubResponse": fixture}),
        );
    }
    for fixture in bidi_requests() {
        wire_contract::<bidi::Request, ref_bidi::Request>(&fixture);
        wire_contract::<bidi::BidirectionalMessage, ref_bidi::BidirectionalMessage>(
            &json!({"Request": fixture}),
        );
    }
    for fixture in bidi_responses() {
        wire_contract::<bidi::Response, ref_bidi::Response>(&fixture);
        wire_contract::<bidi::BidirectionalMessage, ref_bidi::BidirectionalMessage>(
            &json!({"Response": fixture}),
        );
    }
}

#[test]
fn bidirectional_structs_independently_match_derive() {
    let parent =
        json!({"file_id": 1, "ast_id": 127, "start": 128, "end": 16384, "ctx": 4294967295u32});
    wire_contract::<bidi::ParentSpan, ref_bidi::ParentSpan>(&parent);
    struct_json_contract::<bidi::ParentSpan, ref_bidi::ParentSpan>(
        parent,
        &["file_id", "ast_id", "start", "end", "ctx"],
    );
    let join = json!({"ast_id": 1, "start": 127, "end": 128, "ctx": 16384});
    wire_contract::<bidi::SpanJoin, ref_bidi::SpanJoin>(&join);
    struct_json_contract::<bidi::SpanJoin, ref_bidi::SpanJoin>(
        join,
        &["ast_id", "start", "end", "ctx"],
    );
    let list = json!({"dylib_path": "/tmp/例/lib.so"});
    wire_contract::<bidi::ListMacros, ref_bidi::ListMacros>(&list);
    struct_json_contract::<bidi::ListMacros, ref_bidi::ListMacros>(list, &["dylib_path"]);
    wire_contract::<bidi::ApiVersionCheck, ref_bidi::ApiVersionCheck>(&json!({}));
    struct_json_contract::<bidi::ApiVersionCheck, ref_bidi::ApiVersionCheck>(json!({}), &[]);
    wire_contract::<bidi::ExpandMacro, ref_bidi::ExpandMacro>(&bidi_expand());
    struct_json_contract::<bidi::ExpandMacro, ref_bidi::ExpandMacro>(
        bidi_expand(),
        &["lib", "env", "data"],
    );
    wire_contract::<bidi::ExpandMacroData, ref_bidi::ExpandMacroData>(&bidi_data());
    struct_json_contract::<bidi::ExpandMacroData, ref_bidi::ExpandMacroData>(
        bidi_data(),
        &["macro_body", "macro_name"],
    );
    wire_contract::<bidi::ExpandMacroResponse, ref_bidi::ExpandMacroResponse>(&bidi_expanded());
    struct_json_contract::<bidi::ExpandMacroResponse, ref_bidi::ExpandMacroResponse>(
        bidi_expanded(),
        &["tree", "span_data_table", "tracked_env_vars", "tracked_paths"],
    );
    wire_contract::<bidi::ExpnGlobals, ref_bidi::ExpnGlobals>(&globals());
    struct_json_contract::<bidi::ExpnGlobals, ref_bidi::ExpnGlobals>(
        globals(),
        &["def_site", "call_site", "mixed_site"],
    );
    wire_contract::<bidi::ExpnGlobals, ref_bidi::ExpnGlobals>(
        &json!({"def_site": usize::MAX, "call_site": 0, "mixed_site": 1}),
    );

    let mut minimal_data = bidi_data();
    for field in ["attributes", "has_global_spans", "span_data_table"] {
        minimal_data.as_object_mut().unwrap().remove(field);
    }
    wire_contract::<bidi::ExpandMacroData, ref_bidi::ExpandMacroData>(&minimal_data);
    let mut minimal_expand = bidi_expand();
    minimal_expand.as_object_mut().unwrap().remove("current_dir");
    minimal_expand["data"] = minimal_data;
    wire_contract::<bidi::ExpandMacro, ref_bidi::ExpandMacro>(&minimal_expand);
    let empty_response =
        json!({"tree": tree(), "span_data_table": [], "tracked_env_vars": {}, "tracked_paths": []});
    wire_contract::<bidi::ExpandMacroResponse, ref_bidi::ExpandMacroResponse>(&empty_response);
}

#[test]
fn shared_types_and_native_json_shapes_match_derive() {
    wire_contract::<legacy::FlatTree, reference::FlatTree>(&tree());
    struct_json_contract::<legacy::FlatTree, reference::FlatTree>(
        tree(),
        &["subtree", "literal", "punct", "ident", "token_tree", "text"],
    );
    wire_contract::<legacy::FlatTree, reference::FlatTree>(
        &json!({"subtree": [], "literal": [], "punct": [], "ident": [], "token_tree": [], "text": []}),
    );
    for fixture in [json!({}), json!({"span_mode": "Id"}), json!({"span_mode": "RustAnalyzer"})] {
        wire_contract::<legacy::ServerConfig, reference::ServerConfig>(&fixture);
        struct_json_contract::<legacy::ServerConfig, reference::ServerConfig>(fixture, &[]);
    }
    for fixture in [json!("Id"), json!("RustAnalyzer")] {
        wire_contract::<legacy::SpanMode, reference::SpanMode>(&fixture);
    }
    for fixture in [json!(""), json!("panic\n例\"")] {
        wire_contract::<legacy::PanicMessage, reference::PanicMessage>(&fixture);
    }
    for fixture in [json!("CustomDerive"), json!("Attr"), json!("FuncLike"), json!("Bang")] {
        wire_contract::<ProcMacroKind, reference::ProcMacroKind>(&fixture);
    }
    assert_eq!(serde_json::to_string(&ProcMacroKind::Bang).unwrap(), r#""FuncLike""#);
    assert_eq!(serde_json::from_str::<ProcMacroKind>(r#""Bang""#).unwrap(), ProcMacroKind::Bang);
    let (config, _) = json_contract::<legacy::ServerConfig, reference::ServerConfig>("{}");
    assert_eq!(serde_json::to_string(&config).unwrap(), r#"{"span_mode":"Id"}"#);
    let (panic, _) = json_contract::<legacy::PanicMessage, reference::PanicMessage>(r#""panic""#);
    assert_eq!(serde_json::to_string(&panic).unwrap(), r#""panic""#);
    let (flat, _) = json_contract::<legacy::FlatTree, reference::FlatTree>(
        r#"{"subtree":[],"literal":[],"punct":[],"ident":[],"token_tree":[],"text":[]}"#,
    );
    assert_eq!(
        serde_json::to_string(&flat).unwrap(),
        r#"{"subtree":[],"literal":[],"punct":[],"ident":[],"token_tree":[],"text":[]}"#
    );
}

#[test]
fn legacy_all_variants_match_derive_json() {
    for fixture in [
        json!({"ApiVersionCheck": {}}),
        json!({"ListMacros": {"dylib_path": "/tmp/例/lib.so"}}),
        json!({"ExpandMacro": legacy_expand(true)}),
        json!({"ExpandMacro": legacy_expand(false)}),
        json!({"SetConfig": {}}),
        json!({"SetConfig": {"span_mode": "RustAnalyzer"}}),
    ] {
        json_contract::<legacy::Request, ref_legacy::Request>(&fixture.to_string());
    }
    for fixture in [
        json!({"ApiVersionCheck": 4294967295u32}),
        json!({"ListMacros": {"Ok": [["d", "CustomDerive"], ["a", "Attr"], ["f", "FuncLike"], ["b", "Bang"]]}}),
        json!({"ListMacros": {"Ok": []}}),
        json!({"ListMacros": {"Err": "load failed\n例"}}),
        json!({"ExpandMacro": {"Ok": tree()}}),
        json!({"ExpandMacro": {"Err": "panic\"\n例"}}),
        json!({"SetConfig": {"span_mode": "Id"}}),
        json!({"SetConfig": {"span_mode": "RustAnalyzer"}}),
        json!({"ExpandMacroExtended": {"Ok": {"tree": tree(), "span_data_table": [0, 127, 128, 4294967295u32]}}}),
        json!({"ExpandMacroExtended": {"Err": "panic"}}),
    ] {
        json_contract::<legacy::Response, ref_legacy::Response>(&fixture.to_string());
    }
}

#[test]
fn legacy_flatten_skip_and_defaults_match_derive() {
    for serialize in [false, true] {
        let data = legacy_data(serialize);
        json_contract::<legacy::ExpandMacroData, ref_legacy::ExpandMacroData>(&data.to_string());
        struct_json_contract::<legacy::ExpandMacroData, ref_legacy::ExpandMacroData>(
            data,
            &["macro_body", "macro_name"],
        );
        struct_json_contract::<legacy::ExpandMacro, ref_legacy::ExpandMacro>(
            legacy_expand(serialize),
            &["lib", "env", "macro_body", "macro_name"],
        );
        let mut global = globals();
        global["serialize"] = json!(serialize);
        struct_json_contract::<legacy::ExpnGlobals, ref_legacy::ExpnGlobals>(
            global.clone(),
            &["def_site", "call_site", "mixed_site"],
        );
        let (actual, _) =
            json_contract::<legacy::ExpnGlobals, ref_legacy::ExpnGlobals>(&global.to_string());
        assert_eq!(actual.serialize, serialize);
        assert_eq!(
            serde_json::to_string(&actual).unwrap(),
            r#"{"def_site":127,"call_site":128,"mixed_site":16384}"#
        );

        let (actual, _) = json_contract::<legacy::ExpandMacro, ref_legacy::ExpandMacro>(
            &legacy_expand(serialize).to_string(),
        );
        let json = serde_json::to_string(&actual).unwrap();
        let shape: Value = serde_json::from_str(&json).unwrap();
        assert!(shape.get("data").is_none());
        assert!(shape.get("macro_body").is_some());
        assert_eq!(shape.get("has_global_spans").is_some(), serialize);
        assert!(shape.get("span_data_table").is_some());
        let decoded: legacy::ExpandMacro = serde_json::from_str(&json).unwrap();
        assert!(!decoded.data.has_global_spans.serialize);
        assert!(serde_json::to_value(&decoded).unwrap().get("has_global_spans").is_none());
    }
    struct_json_contract::<legacy::ExpnGlobals, ref_legacy::ExpnGlobals>(
        globals(),
        &["def_site", "call_site", "mixed_site"],
    );
    struct_json_contract::<legacy::ExpandMacroExtended, ref_legacy::ExpandMacroExtended>(
        json!({"tree": tree(), "span_data_table": [0, 128]}),
        &["tree", "span_data_table"],
    );
    let mut minimal = legacy_expand(true);
    for field in ["attributes", "current_dir", "has_global_spans", "span_data_table"] {
        minimal.as_object_mut().unwrap().remove(field);
    }
    let (actual, _) =
        json_contract::<legacy::ExpandMacro, ref_legacy::ExpandMacro>(&minimal.to_string());
    assert!(actual.current_dir.is_none());
    assert!(actual.data.attributes.is_none());
    assert!(actual.data.span_data_table.is_empty());
    assert!(!actual.data.has_global_spans.serialize);
    assert_eq!(actual.data.has_global_spans.def_site, 0);
    assert_eq!(actual.data.has_global_spans.call_site, 0);
    assert_eq!(actual.data.has_global_spans.mixed_site, 0);
    let shape = serde_json::to_value(&actual).unwrap();
    assert_eq!(shape["current_dir"], Value::Null);
    assert_eq!(shape["attributes"], Value::Null);
    assert!(shape.get("has_global_spans").is_none());
    assert!(shape.get("span_data_table").is_none());

    let mut empty = legacy_data(true);
    empty["attributes"] = Value::Null;
    empty["span_data_table"] = json!([]);
    let (actual, _) =
        json_contract::<legacy::ExpandMacroData, ref_legacy::ExpandMacroData>(&empty.to_string());
    let shape = serde_json::to_value(actual).unwrap();
    assert!(shape.get("has_global_spans").is_some());
    assert!(shape.get("span_data_table").is_none());

    // The flattened derive cannot serialize a map of unknown length to postcard.
    let (actual, reference) =
        json_contract::<legacy::ExpandMacro, ref_legacy::ExpandMacro>(&minimal.to_string());
    let actual_error = postcard::to_allocvec(&actual).unwrap_err();
    let reference_error = postcard::to_allocvec(&reference).unwrap_err();
    assert_eq!(actual_error, reference_error);
}

fn struct_variant_json_contract<T, R>(fixture: &Value, optional: &[&str])
where
    T: Serialize + DeserializeOwned + Debug,
    R: Serialize + DeserializeOwned + Debug,
{
    let (variant, payload) = fixture.as_object().unwrap().iter().next().unwrap();
    let fields = payload.as_object().unwrap();
    let mut unknown = fixture.clone();
    unknown[variant]["future_field"] = json!([{"nested": [null, true]}]);
    json_acceptance::<T, R>(&unknown.to_string(), true);
    for (field, value) in fields {
        let mut missing = fixture.clone();
        missing[variant].as_object_mut().unwrap().remove(field);
        json_acceptance::<T, R>(&missing.to_string(), optional.contains(&field.as_str()));
        let duplicate_payload = format!(
            "{{{}:{value},{}",
            serde_json::to_string(field).unwrap(),
            &payload.to_string()[1..],
        );
        let duplicate =
            format!("{{{}:{duplicate_payload}}}", serde_json::to_string(variant).unwrap());
        json_acceptance::<T, R>(&duplicate, false);
    }
    let multiple_variants = format!(
        "{{{}:{payload},{}:{payload}}}",
        serde_json::to_string(variant).unwrap(),
        serde_json::to_string(variant).unwrap(),
    );
    json_acceptance::<T, R>(&multiple_variants, false);
}

#[test]
fn enum_struct_fields_preserve_unknown_duplicate_and_missing_behavior() {
    for fixture in subrequests() {
        struct_variant_json_contract::<bidi::SubRequest, ref_bidi::SubRequest>(&fixture, &[]);
    }
    for fixture in subresponses() {
        let variant = fixture.as_object().unwrap().keys().next().unwrap().as_str();
        let optional = match variant {
            "SourceTextResult" => &["text"][..],
            "LocalFilePathResult" => &["name"][..],
            "SpanParentResult" => &["parent_span"][..],
            "SpanJoinResult" => &["span"][..],
            _ => &[],
        };
        struct_variant_json_contract::<bidi::SubResponse, ref_bidi::SubResponse>(
            &fixture, optional,
        );
    }
    struct_variant_json_contract::<legacy::Request, ref_legacy::Request>(
        &json!({"ListMacros": {"dylib_path": "/tmp/lib.so"}}),
        &[],
    );
    struct_variant_json_contract::<legacy::Request, ref_legacy::Request>(
        &json!({"ApiVersionCheck": {}}),
        &[],
    );
    for input in [
        r#"{"SourceTextResult":{"text":null,"text":"second"}}"#,
        r#"{"LocalFilePathResult":{"name":null,"name":null}}"#,
        r#"{"SpanParentResult":{"parent_span":null,"parent_span":null}}"#,
        r#"{"SpanJoinResult":{"span":null,"span":null}}"#,
        r#"{"FilePathResult":{"name":null}}"#,
        r#"{"LineColumnResult":{"line":-1,"column":0}}"#,
        r#"{"LineColumnResult":{"line":4294967296,"column":0}}"#,
        r#"{"ByteRangeResult":{"range":{"start":0}}}"#,
        r#"{"ByteRangeResult":{"range":{"start":0,"start":1,"end":2}}}"#,
    ] {
        json_acceptance::<bidi::SubResponse, ref_bidi::SubResponse>(input, false);
    }
    for input in [
        r#"{"FilePath":{"file_id":-1}}"#,
        r#"{"FilePath":{"file_id":4294967296}}"#,
        r#"{"FilePath":{"file_id":1.5}}"#,
        r#"{"FilePath":{"file_id":"1"}}"#,
        r#"{"FilePath":{"file_id":null}}"#,
    ] {
        json_acceptance::<bidi::SubRequest, ref_bidi::SubRequest>(input, false);
    }
}

fn sequence_json_contract<T, R>(fields: Vec<Value>, minimum: usize)
where
    T: Serialize + DeserializeOwned + Debug,
    R: Serialize + DeserializeOwned + Debug,
{
    for end in 0..=fields.len() {
        json_acceptance::<T, R>(&serde_json::to_string(&fields[..end]).unwrap(), end >= minimum);
    }
    let mut extra = fields;
    extra.push(json!("extra"));
    json_acceptance::<T, R>(&serde_json::to_string(&extra).unwrap(), false);
}

#[test]
fn sequence_structs_preserve_required_options_and_trailing_defaults() {
    sequence_json_contract::<bidi::ParentSpan, ref_bidi::ParentSpan>(
        vec![json!(1), json!(2), json!(3), json!(4), json!(5)],
        5,
    );
    sequence_json_contract::<bidi::SpanJoin, ref_bidi::SpanJoin>(
        vec![json!(1), json!(2), json!(3), json!(4)],
        4,
    );
    sequence_json_contract::<bidi::ListMacros, ref_bidi::ListMacros>(vec![json!("/tmp/lib.so")], 1);
    sequence_json_contract::<bidi::ApiVersionCheck, ref_bidi::ApiVersionCheck>(vec![], 0);
    sequence_json_contract::<bidi::ExpandMacro, ref_bidi::ExpandMacro>(
        vec![json!("/tmp/lib.so"), json!([]), Value::Null, bidi_data()],
        4,
    );
    sequence_json_contract::<bidi::ExpandMacroData, ref_bidi::ExpandMacroData>(
        vec![tree(), json!("macro"), Value::Null, globals(), json!([128])],
        3,
    );
    sequence_json_contract::<bidi::ExpandMacroResponse, ref_bidi::ExpandMacroResponse>(
        vec![tree(), json!([128]), json!({"KEY": null}), json!(["/tmp/path"])],
        4,
    );
    sequence_json_contract::<bidi::ExpnGlobals, ref_bidi::ExpnGlobals>(
        vec![json!(1), json!(2), json!(3)],
        3,
    );
    sequence_json_contract::<legacy::FlatTree, reference::FlatTree>(
        vec![json!([1]), json!([2]), json!([3]), json!([4]), json!([5]), json!(["例"])],
        6,
    );
    sequence_json_contract::<legacy::ServerConfig, reference::ServerConfig>(
        vec![json!("RustAnalyzer")],
        0,
    );
    sequence_json_contract::<legacy::ExpandMacroData, ref_legacy::ExpandMacroData>(
        vec![
            tree(),
            json!("macro"),
            Value::Null,
            json!({"serialize": true, "def_site": 1, "call_site": 2, "mixed_site": 3}),
            json!([128]),
        ],
        3,
    );
    sequence_json_contract::<legacy::ExpandMacroExtended, ref_legacy::ExpandMacroExtended>(
        vec![tree(), json!([128])],
        2,
    );
    sequence_json_contract::<legacy::ExpnGlobals, ref_legacy::ExpnGlobals>(
        vec![json!(true), json!(1), json!(2), json!(3)],
        4,
    );
    json_acceptance::<legacy::ExpandMacro, ref_legacy::ExpandMacro>(
        &json!(["/tmp/lib.so", [], null, tree(), "macro", null]).to_string(),
        false,
    );
    json_acceptance::<legacy::PanicMessage, reference::PanicMessage>(r#"["panic"]"#, false);

    for fixture in [
        json!({"FilePath": [4294967295u32]}),
        json!({"SourceText": [1, 127, 128, 16384]}),
        json!({"LocalFilePath": [0]}),
        json!({"LineColumn": [1, 2, 4294967295u32]}),
        json!({"ByteRange": [3, 4, 5, 6]}),
        json!({"SpanSource": [7, 8, 9, 10, 11]}),
        json!({"SpanParent": [12, 13, 14, 15, 16]}),
        json!({"SpanJoin": [17, 18, 19, 20, 21, 127, 128, 16384, 4294967295u32]}),
    ] {
        let variant = fixture.as_object().unwrap().keys().next().unwrap();
        wire_contract::<bidi::SubRequest, ref_bidi::SubRequest>(&fixture);
        let mut short = fixture.clone();
        short[variant].as_array_mut().unwrap().pop();
        json_acceptance::<bidi::SubRequest, ref_bidi::SubRequest>(&short.to_string(), false);
        let mut long = fixture.clone();
        long[variant].as_array_mut().unwrap().push(json!(0));
        json_acceptance::<bidi::SubRequest, ref_bidi::SubRequest>(&long.to_string(), false);
    }
    for fixture in [
        json!({"SourceTextResult": []}),
        json!({"LocalFilePathResult": []}),
        json!({"SpanParentResult": []}),
        json!({"SpanJoinResult": []}),
    ] {
        json_acceptance::<bidi::SubResponse, ref_bidi::SubResponse>(&fixture.to_string(), false);
    }
    for fixture in [
        json!({"SourceTextResult": [null]}),
        json!({"LocalFilePathResult": [null]}),
        json!({"SpanParentResult": [null]}),
        json!({"SpanJoinResult": [null]}),
    ] {
        json_acceptance::<bidi::SubResponse, ref_bidi::SubResponse>(&fixture.to_string(), true);
    }
}

fn unknown_enum_contract<T, R>()
where
    T: Serialize + DeserializeOwned + Debug,
    R: Serialize + DeserializeOwned + Debug,
{
    for input in [r#""FutureVariant""#, r#"{"FutureVariant":{}}"#, "{}", "[]", "0", "null"] {
        json_acceptance::<T, R>(input, false);
    }
    for discriminant in [127u32, 128, u32::MAX] {
        let bytes = postcard::to_allocvec(&discriminant).unwrap();
        assert!(postcard::from_bytes::<R>(&bytes).is_err());
        assert!(postcard::from_bytes::<T>(&bytes).is_err());
    }
}

#[test]
fn unknown_variants_and_invalid_binary_options_are_rejected() {
    unknown_enum_contract::<ProcMacroKind, reference::ProcMacroKind>();
    unknown_enum_contract::<legacy::SpanMode, reference::SpanMode>();
    unknown_enum_contract::<legacy::Request, ref_legacy::Request>();
    unknown_enum_contract::<legacy::Response, ref_legacy::Response>();
    unknown_enum_contract::<bidi::Request, ref_bidi::Request>();
    unknown_enum_contract::<bidi::Response, ref_bidi::Response>();
    unknown_enum_contract::<bidi::SubRequest, ref_bidi::SubRequest>();
    unknown_enum_contract::<bidi::SubResponse, ref_bidi::SubResponse>();
    unknown_enum_contract::<bidi::BidirectionalMessage, ref_bidi::BidirectionalMessage>();
    for bytes in [[1, 2], [2, 2], [6, 2], [7, 2]] {
        assert!(postcard::from_bytes::<ref_bidi::SubResponse>(&bytes).is_err());
        assert!(postcard::from_bytes::<bidi::SubResponse>(&bytes).is_err());
    }
    for fixture in [json!({"Id": null}), json!({"RustAnalyzer": null})] {
        wire_contract::<legacy::SpanMode, reference::SpanMode>(&fixture);
    }
    for fixture in [
        json!({"CustomDerive": null}),
        json!({"Attr": null}),
        json!({"Bang": null}),
        json!({"FuncLike": null}),
    ] {
        wire_contract::<ProcMacroKind, reference::ProcMacroKind>(&fixture);
    }
}

fn normalized_collections<T: Serialize>(value: &T) -> Value {
    let mut value = serde_json::to_value(value).unwrap();
    value["tracked_paths"].as_array_mut().unwrap().sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    value
}

#[test]
fn unordered_collections_cross_decode_without_assuming_hash_iteration_order() {
    let fixture = json!({
        "tree": tree(), "span_data_table": [0, 128],
        "tracked_env_vars": {"EMPTY": "", "UNSET": null, "例": "quoted\"\n", "PATH": "/tmp/path"},
        "tracked_paths": ["/tmp/a", "/tmp/例", "", "/tmp/a"],
    });
    let actual: bidi::ExpandMacroResponse = serde_json::from_value(fixture.clone()).unwrap();
    let reference: ref_bidi::ExpandMacroResponse = serde_json::from_value(fixture).unwrap();
    let expected = normalized_collections(&reference);
    assert_eq!(normalized_collections(&actual), expected);
    let actual_json = serde_json::to_vec(&actual).unwrap();
    let reference_json = serde_json::to_vec(&reference).unwrap();
    assert_eq!(
        normalized_collections(
            &serde_json::from_slice::<bidi::ExpandMacroResponse>(&reference_json).unwrap()
        ),
        expected
    );
    assert_eq!(
        normalized_collections(
            &serde_json::from_slice::<ref_bidi::ExpandMacroResponse>(&actual_json).unwrap()
        ),
        expected
    );
    let actual_bytes = postcard::to_allocvec(&actual).unwrap();
    let reference_bytes = postcard::to_allocvec(&reference).unwrap();
    assert_eq!(
        normalized_collections(
            &postcard::from_bytes::<bidi::ExpandMacroResponse>(&reference_bytes).unwrap()
        ),
        expected
    );
    assert_eq!(
        normalized_collections(
            &postcard::from_bytes::<ref_bidi::ExpandMacroResponse>(&actual_bytes).unwrap()
        ),
        expected
    );
    assert_eq!(actual.tracked_env_vars.len(), 4);
    assert_eq!(actual.tracked_paths.len(), 3);
}

#[test]
fn explicit_null_is_not_a_struct_or_collection_default() {
    for field in ["has_global_spans", "span_data_table"] {
        let mut fixture = bidi_data();
        fixture[field] = Value::Null;
        json_acceptance::<bidi::ExpandMacroData, ref_bidi::ExpandMacroData>(
            &fixture.to_string(),
            false,
        );
        let mut fixture = legacy_data(true);
        fixture[field] = Value::Null;
        json_acceptance::<legacy::ExpandMacroData, ref_legacy::ExpandMacroData>(
            &fixture.to_string(),
            false,
        );
        let mut fixture = legacy_expand(true);
        fixture[field] = Value::Null;
        json_acceptance::<legacy::ExpandMacro, ref_legacy::ExpandMacro>(
            &fixture.to_string(),
            false,
        );
    }
    json_acceptance::<legacy::ServerConfig, reference::ServerConfig>(
        r#"{"span_mode":null}"#,
        false,
    );
    let mut global = globals();
    global["serialize"] = Value::Null;
    json_acceptance::<legacy::ExpnGlobals, ref_legacy::ExpnGlobals>(&global.to_string(), false);
    for field in ["tree", "span_data_table", "tracked_env_vars", "tracked_paths"] {
        let mut fixture = bidi_expanded();
        fixture[field] = Value::Null;
        json_acceptance::<bidi::ExpandMacroResponse, ref_bidi::ExpandMacroResponse>(
            &fixture.to_string(),
            false,
        );
    }
    for field in ["subtree", "literal", "punct", "ident", "token_tree", "text"] {
        let mut fixture = tree();
        fixture[field] = Value::Null;
        json_acceptance::<legacy::FlatTree, reference::FlatTree>(&fixture.to_string(), false);
    }
}

#[test]
fn legacy_flatten_distinguishes_nested_data_and_ignores_unknown_fields() {
    let nested = json!({
        "lib": "/tmp/lib.so", "env": [], "current_dir": null, "data": legacy_data(true),
    });
    json_acceptance::<legacy::ExpandMacro, ref_legacy::ExpandMacro>(&nested.to_string(), false);
    let flattened = legacy_expand(true);
    json_acceptance::<bidi::ExpandMacro, ref_bidi::ExpandMacro>(&flattened.to_string(), false);
    let mut both = flattened.clone();
    both["data"] = json!({"macro_body": null, "macro_name": "ignored"});
    json_acceptance::<legacy::ExpandMacro, ref_legacy::ExpandMacro>(&both.to_string(), true);
    let unknown_duplicates = format!(
        "{{\"future\":null,\"future\":{{\"nested\":[1,2,3]}},{}",
        &flattened.to_string()[1..],
    );
    json_acceptance::<legacy::ExpandMacro, ref_legacy::ExpandMacro>(&unknown_duplicates, true);
    for field in ["attributes", "current_dir"] {
        let mut fixture = legacy_expand(true);
        fixture[field] = Value::Null;
        let duplicates = format!(
            "{{{}:null,{}",
            serde_json::to_string(field).unwrap(),
            &fixture.to_string()[1..],
        );
        json_acceptance::<legacy::ExpandMacro, ref_legacy::ExpandMacro>(&duplicates, false);
    }
    let mut required_missing = legacy_expand(false);
    required_missing["has_global_spans"] = json!({"serialize": false});
    json_acceptance::<legacy::ExpandMacro, ref_legacy::ExpandMacro>(
        &required_missing.to_string(),
        false,
    );
}

#[test]
fn version_check_and_macro_kind_discriminants_stay_pinned() {
    assert_eq!(postcard::to_allocvec(&legacy::Request::ApiVersionCheck {}).unwrap(), [0]);
    assert_eq!(
        postcard::to_allocvec(&legacy::Response::ApiVersionCheck(128)).unwrap(),
        [0, 128, 1]
    );
    assert_eq!(
        postcard::to_allocvec(&bidi::Request::ApiVersionCheck(bidi::ApiVersionCheck {})).unwrap(),
        [2],
    );
    assert_eq!(postcard::to_allocvec(&bidi::Response::ApiVersionCheck(128)).unwrap(), [1, 128, 1]);
    assert_eq!(
        postcard::to_allocvec(&bidi::BidirectionalMessage::Request(
            bidi::Request::ApiVersionCheck(bidi::ApiVersionCheck {}),
        ))
        .unwrap(),
        [0, 2],
    );
    for (kind, byte, name) in [
        (ProcMacroKind::CustomDerive, 0, "CustomDerive"),
        (ProcMacroKind::Attr, 1, "Attr"),
        (ProcMacroKind::Bang, 2, "FuncLike"),
    ] {
        assert_eq!(postcard::to_allocvec(&kind).unwrap(), [byte]);
        assert_eq!(postcard::from_bytes::<ProcMacroKind>(&[byte]).unwrap(), kind);
        assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{name}\""));
    }
}
