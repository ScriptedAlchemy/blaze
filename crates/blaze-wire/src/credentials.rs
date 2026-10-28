//! The credential vectors of section 8 that are not a type of the schema: the `Blaze-Agent`
//! signature over a request, the `Blaze-Capability` header, the `ops` bits and `JobContent`'s
//! bytes.
//!
//! The golden file `tests/golden/credentials.txt` holds them, written by
//! `cargo run -p blaze-wire --example write_golden` and checked by
//! `tests/credentials.rs`. The key is `sha256("blaze-wire/golden")`, so the file carries no
//! secret and every run makes the same signature.

use crate::calls::SignedCapability;
use crate::shapes::*;
use crate::{AgentRequest, Op, encode, sign};
use ed25519_dalek::SigningKey;

/// The key every credential vector is signed with: `sha256("blaze-wire/golden")`, so the file
/// carries no secret and every run makes the same signature.
pub fn key() -> SigningKey {
    use sha2::Digest as _;
    let seed: [u8; 32] = sha2::Sha256::digest(b"blaze-wire/golden").into();
    SigningKey::from_bytes(&seed)
}

/// The `Blaze-Agent` request the vector signs.
pub fn agent_request() -> AgentRequest<'static> {
    AgentRequest {
        agent: "worker-1",
        date_ms: 1_767_225_600_000,
        method: "POST",
        path: "/v1/workers/next",
        body: b"\xa1\x00\x00",
    }
}

/// The capability the header vector carries: a trusted builder's, on `Main`, for one job.
pub fn capability() -> Capability {
    Capability {
        principal: "builder:mac-studio".into(),
        role: 2,
        namespace: Namespace::Main,
        source: SourceRef::Git(GitRef { repo_id: 1261602522, commit: [7u8; 20], tree: [8u8; 20] }),
        execution: Some(ExecutionId(4_294_967_296)),
        attempt: Some(Attempt(1)),
        ops: (Op::Lookup as u32)
            | (Op::Fetch as u32)
            | (Op::Publish as u32)
            | (Op::Events as u32)
            | (Op::Roots as u32),
        not_after_ms: 1_767_229_200_000,
        epoch: 9,
    }
}

/// The `JobContent` vector: one build spec's digest on `Main`.
pub fn job_content() -> JobContentInput {
    JobContentInput { spec: [0x42; 32], namespace: Namespace::Main }
}

/// The text of the credentials' golden file.
pub fn golden_text() -> String {
    let key = key();
    let request = agent_request();
    let capability = capability();
    let signed = SignedCapability { signature: sign(&key, &capability), capability };
    let content = job_content();

    let mut out = String::new();
    out.push_str("# Section 8's credentials and the two byte shapes a client reads by hand.\n");
    out.push_str("# Written by `cargo run -p blaze-wire --example write_golden`; the key is\n");
    out.push_str("# sha256(\"blaze-wire/golden\"), so the file carries no secret.\n\n");

    out.push_str("[agent]\n");
    out.push_str("agent: ");
    out.push_str(request.agent);
    out.push('\n');
    out.push_str("date_ms: ");
    out.push_str(&request.date_ms.to_string());
    out.push('\n');
    out.push_str("method: ");
    out.push_str(request.method);
    out.push('\n');
    out.push_str("path: ");
    out.push_str(request.path);
    out.push('\n');
    out.push_str("body: ");
    out.push_str(&crate::json::hex(request.body));
    out.push('\n');
    out.push_str("signing_input: ");
    out.push_str(&crate::json::hex(&request.signing_input()));
    out.push('\n');
    out.push_str("signature: ");
    out.push_str(&request.sign(&key));
    out.push('\n');
    out.push_str("key: ");
    out.push_str(&crate::json::hex(&key.verifying_key().to_bytes()));
    out.push_str("\n\n");

    out.push_str("[capability]\n");
    out.push_str("cbor: ");
    out.push_str(&crate::json::hex(&encode(&signed)));
    out.push('\n');
    out.push_str("header: ");
    out.push_str(&crate::capability_header(&signed));
    out.push('\n');
    out.push_str("ops: ");
    out.push_str(&signed.capability.ops.to_string());
    out.push('\n');
    out.push_str("ops_names: ");
    out.push_str(&ops_names(signed.capability.ops));
    out.push('\n');
    out.push_str("capability_digest: ");
    out.push_str(&crate::json::hex(&crate::digest_of(&signed.capability)));
    out.push_str("\n\n");

    out.push_str("[ops]\n");
    for op in Op::ALL {
        out.push_str(&format!("{}: {}\n", op_name(op), op as u32));
    }
    out.push_str("all: ");
    out.push_str(&Op::ALL.iter().map(|op| *op as u32).fold(0u32, |a, b| a | b).to_string());
    out.push_str("\n\n");

    out.push_str("[job_content]\n");
    out.push_str("cbor: ");
    out.push_str(&crate::json::hex(&encode(&content)));
    out.push('\n');
    out.push_str("digest: ");
    out.push_str(&crate::json::hex(&JobContent::of(&content).0));
    out.push('\n');
    out
}

/// The names of the `ops` bits set in `ops`, in bit order.
pub fn ops_names(ops: u32) -> String {
    Op::ALL.iter().filter(|op| op.in_bits(ops)).map(|op| op_name(*op)).collect::<Vec<_>>().join(",")
}

fn op_name(op: Op) -> &'static str {
    match op {
        Op::Lookup => "lookup",
        Op::Fetch => "fetch",
        Op::Publish => "publish",
        Op::Events => "events",
        Op::Cancel => "cancel",
        Op::Roots => "roots",
        Op::DeleteNamespace => "delete-a-namespace",
        Op::Work => "work",
    }
}
