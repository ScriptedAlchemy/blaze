//! The types of docs/distributed.md's "Data shapes", and the inputs its digest newtypes hash.
//!
//! Each struct is a CBOR map of the field numbers the doc's comments give, every key present, in
//! ascending order; each enum is the two-element array `[discriminant, payload]`. A newtype over a
//! digest is its inner value: a byte string of 32 bytes. A `Vec` marked **set** in the doc is a
//! [`Set`], sorted by the bytes of each item's encoding and without duplicates.

use crate::cbor::Wire;
use crate::{Digest, Named, Set};
use crate::{wire_enum, wire_struct};

/// Message-bounded free-form input text, unlike ordinary 4 KiB identifiers.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InputText(pub String);
impl From<String> for InputText {
    fn from(value: String) -> Self {
        Self(value)
    }
}
impl PartialEq<str> for InputText {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}
impl std::ops::Deref for InputText {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}
impl Wire for InputText {
    fn encode(&self, e: &mut crate::Encoder) {
        self.0.encode(e);
    }
    fn decode(r: &mut crate::Reader<'_>) -> Result<Self, crate::Error> {
        Ok(Self(r.path_text()?.to_owned()))
    }
}
impl Named for InputText {
    const NAME: &'static str = "InputText";
}
impl crate::vectors::Sample for InputText {
    fn sample(g: &mut crate::Gen) -> Self {
        Self(String::sample(g))
    }
}

/// A lossless logical path: `/` separates byte components and `.` denotes the root.
/// Component bytes outside ASCII letters, digits, `-`, `_`, `.` use uppercase `%HH`.
/// Escapes of safe bytes, empty/dot/parent components, decoded slash and NUL are invalid.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourcePath(String);

impl SourcePath {
    /// Validates a portable source-relative path without canonicalizing malformed input.
    pub fn new(path: String) -> Result<Self, crate::Error> {
        if path != "." {
            if path.is_empty() {
                return Err(path_error());
            }
            for part in path.split('/') {
                let bytes = decode_component(part)?;
                if encode_component(&bytes) != part {
                    return Err(path_error());
                }
            }
        }
        Ok(Self(path))
    }

    /// Encode a normalized native relative path without Unicode replacement or path guessing.
    pub fn from_native(path: &std::path::Path) -> Result<Self, crate::Error> {
        let mut parts = Vec::new();
        for part in path.components() {
            match part {
                std::path::Component::Normal(name) => {
                    #[cfg(unix)]
                    let bytes = {
                        use std::os::unix::ffi::OsStrExt;
                        name.as_bytes()
                    };
                    #[cfg(not(unix))]
                    let bytes = name.to_str().ok_or_else(path_error)?.as_bytes();
                    parts.push(encode_component(bytes));
                }
                std::path::Component::CurDir => {}
                _ => return Err(path_error()),
            }
        }
        Self::new(if parts.is_empty() { ".".to_owned() } else { parts.join("/") })
    }

    /// Decode logical components to native ones. Unix preserves every valid filename byte.
    /// Other hosts report an explicit conversion error for byte names they cannot represent.
    pub fn to_native(&self) -> Result<std::path::PathBuf, crate::Error> {
        if self.0 == "." {
            return Ok(std::path::PathBuf::from("."));
        }
        let mut path = std::path::PathBuf::new();
        for component in self.0.split('/') {
            let bytes = decode_component(component)?;
            #[cfg(unix)]
            let name = {
                use std::os::unix::ffi::OsStringExt;
                std::ffi::OsString::from_vec(bytes)
            };
            #[cfg(not(unix))]
            let name =
                std::ffi::OsString::from(String::from_utf8(bytes).map_err(|_| path_error())?);
            // Native conversion must not reinterpret a component as an absolute/traversal path.
            if std::path::Path::new(&name).components().count() != 1
                || !matches!(
                    std::path::Path::new(&name).components().next(),
                    Some(std::path::Component::Normal(_))
                )
            {
                return Err(path_error());
            }
            path.push(name);
        }
        Ok(path)
    }

    /// Whether the final decoded component names Cargo's required root manifest.
    pub fn is_cargo_manifest(&self) -> bool {
        self.0.rsplit('/').next() == Some("Cargo.toml")
    }

    /// The canonical escaped logical path, never an absolute checkout path.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn path_error() -> crate::Error {
    crate::Error { kind: crate::ErrorKind::InvalidPath, at: 0 }
}
fn safe_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}
fn encode_component(bytes: &[u8]) -> String {
    let mut text = String::new();
    for &byte in bytes {
        if safe_byte(byte) {
            text.push(byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(text, "%{byte:02X}");
        }
    }
    text
}
fn decode_component(text: &str) -> Result<Vec<u8>, crate::Error> {
    let mut bytes = Vec::new();
    let mut at = 0;
    while at < text.len() {
        let byte = text.as_bytes()[at];
        if byte == b'%' {
            let hex = text.get(at + 1..at + 3).ok_or_else(path_error)?;
            let byte = u8::from_str_radix(hex, 16).map_err(|_| path_error())?;
            if safe_byte(byte) || matches!(byte, 0 | b'/') {
                return Err(path_error());
            }
            bytes.push(byte);
            at += 3;
        } else {
            if !safe_byte(byte) {
                return Err(path_error());
            }
            bytes.push(byte);
            at += 1;
        }
    }
    if bytes.is_empty() || bytes == b"." || bytes == b".." {
        return Err(path_error());
    }
    Ok(bytes)
}

impl Wire for SourcePath {
    fn encode(&self, e: &mut crate::Encoder) {
        self.0.encode(e);
    }
    fn decode(r: &mut crate::Reader<'_>) -> Result<Self, crate::Error> {
        let at = r.position();
        Self::new(r.path_text()?.to_owned()).map_err(|mut error| {
            error.at = at;
            error
        })
    }
}

impl Named for SourcePath {
    const NAME: &'static str = "SourcePath";
}
impl crate::vectors::Sample for SourcePath {
    fn sample(g: &mut crate::vectors::Gen) -> Self {
        Self(format!("workspace-{}/Cargo.toml", g.below(100)))
    }
}

wire_struct! {
    /// What the native side of a build is: everything outside blaze that changes what is linked or
    /// how a program runs. An executor serves a job only on an equal platform.
    ExecPlatform {
        0 => triple: String,
        1 => os: String,
        2 => sdk: Option<(String, String, String)>,
        3 => libc: Option<(String, String)>,
        4 => linker: (String, String, Digest),
        5 => cpu: String,
        6 => cc: Option<(String, String, Digest)>,
        7 => cxx: Option<(String, String, Digest)>,
        8 => ar: Option<(String, String, Digest)>,
        9 => native_root: Option<Digest>,
    }
}

wire_enum! {
    /// Which tree a job builds: a commit the server verified, or #271's root.
    SourceRef {
        Git => 0 (GitRef),
        Root => 1 (RootRef),
    }
}

wire_struct! {
    /// The fields of `SourceRef::Git`.
    GitRef {
        0 => repo_id: u64,
        1 => commit: [u8; 20],
        2 => tree: [u8; 20],
    }
}

wire_struct! {
    /// The fields of `SourceRef::Root`.
    RootRef {
        0 => root: Digest,
    }
}

wire_struct! {
    /// A build after cargo's configuration, workspace, feature and profile resolution: what
    /// `cargo blaze <command>` would do, with nothing left to the worker's environment.
    BuildSpec {
        validate validate_build_paths,
        0 => source: SourceRef,
        1 => lock: Digest,
        2 => units: Vec<UnitSpec>,
        3 => asked: Set<u32>,
        4 => profile: ProfileSpec,
        5 => platform: Digest,
        6 => rustflags: Vec<InputText>,
        7 => config_env: Set<(String, ConfigEnvValue)>,
        8 => script_env: Set<(String, ConfigEnvValue)>,
        9 => cwd: SourcePath,
        10 => command: Command,
        11 => toolchain: StoreIdentity,
        12 => workspace_manifest: SourcePath,
    }
}

fn validate_build_paths(spec: &BuildSpec) -> Result<(), crate::Error> {
    if !spec.workspace_manifest.is_cargo_manifest() {
        return Err(crate::Error { kind: crate::ErrorKind::InvalidPath, at: 0 });
    }
    Ok(())
}

wire_enum! {
    /// Schema 2 preserves whether configuration text is literal or a workspace-relative path.
    ConfigEnvValue {
        Literal => 0 (InputText),
        WorkspaceRelative => 1 (SourcePath),
        Native => 2 (NativeValue),
    }
}

impl ConfigEnvValue {
    /// Render the value for a worker at the selected workspace, without guessing from its text.
    pub fn materialize(
        &self,
        workspace: &std::path::Path,
    ) -> Result<std::ffi::OsString, crate::Error> {
        match self {
            Self::Literal(value) => Ok(value.0.clone().into()),
            Self::WorkspaceRelative(path) => Ok(workspace.join(path.to_native()?).into_os_string()),
            Self::Native(bytes) => {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStringExt;
                    Ok(std::ffi::OsString::from_vec(bytes.0.clone()))
                }
                #[cfg(not(unix))]
                {
                    Ok(std::ffi::OsString::from(
                        String::from_utf8(bytes.0.clone()).map_err(|_| path_error())?,
                    ))
                }
            }
        }
    }

    /// Its logical text, for diagnostics; this alone does not preserve its semantics.
    pub fn text(&self) -> &str {
        match self {
            Self::Literal(value) => &value.0,
            Self::WorkspaceRelative(path) => path.as_str(),
            Self::Native(_) => "<native bytes>",
        }
    }

    /// Keep UTF-8's existing text encoding, and encode other native Unix values as bytes.
    pub fn from_native(value: &std::ffi::OsStr) -> Self {
        if let Some(text) = value.to_str() {
            return Self::Literal(InputText(text.to_owned()));
        }
        Self::Native(NativeValue(value.as_encoded_bytes().to_vec()))
    }
}

/// Message-bounded, NUL-free native Unix environment bytes. Not an arbitrary blob.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativeValue(pub Vec<u8>);
impl Wire for NativeValue {
    fn encode(&self, e: &mut crate::Encoder) {
        e.bytes(&self.0);
    }
    fn decode(r: &mut crate::Reader<'_>) -> Result<Self, crate::Error> {
        let at = r.position();
        let bytes = r.input_bytes()?;
        if bytes.contains(&0) {
            return Err(crate::Error { kind: crate::ErrorKind::InvalidPath, at });
        }
        Ok(Self(bytes.to_vec()))
    }
}
impl Named for NativeValue {
    const NAME: &'static str = "NativeValue";
}
impl crate::vectors::Sample for NativeValue {
    fn sample(g: &mut crate::Gen) -> Self {
        Self(vec![b'n', 0x80 + (g.below(128) as u8)])
    }
}

wire_struct! {
    /// One unit of the resolved graph.
    UnitSpec {
        0 => package: (String, String, Digest),
        1 => target: (u8, String),
        2 => features: Set<String>,
        3 => cfg: Set<InputText>,
        4 => edition: u16,
        5 => deps: Set<(u32, String)>,
        6 => test: bool,
        7 => host: bool,
        8 => profile: ProfileSpec,
    }
}

wire_struct! {
    /// A unit's profile after `[profile.*.package.*]` and build-override.
    ProfileSpec {
        0 => name: String,
        1 => opt_level: u8,
        2 => debug: u8,
        3 => debug_assertions: bool,
        4 => overflow_checks: bool,
        5 => panic: u8,
        6 => strip: u8,
        7 => split_debuginfo: u8,
        8 => backend: u8,
        9 => mir_opt: u8,
        10 => backend_opt: u8,
        11 => backend_emit: u8,
    }
}

wire_enum! {
    /// Where a local build looks before it compiles.
    LocalPolicy {
        Never => 0,
        Fallback => 1,
        Race => 2,
    }
}

wire_enum! {
    /// The command a build spec is for.
    Command {
        Check => 0,
        Build => 1,
        Test => 2 (RunSpec),
        Run => 3 (RunSpec),
    }
}

wire_struct! {
    /// What a test run is asked to do. A timeout schedules, and is in the job request, not here.
    RunSpec {
        0 => filters: Vec<String>,
        1 => exact: bool,
        2 => skip: Vec<String>,
        3 => ignored: u8,
        4 => no_run: bool,
        5 => no_fail_fast: bool,
        6 => test_threads: Option<u32>,
        7 => harness_args: Vec<String>,
        8 => env: Set<(String, String)>,
        9 => doc: u8,
    }
}

wire_enum! {
    /// The namespaces of the cache, and what may write each.
    Namespace {
        Main => 0,
        Pr => 1 (PrNamespace),
        Std => 2,
    }
}

wire_struct! {
    /// The fields of `Namespace::Pr`: its pull request's number.
    PrNamespace {
        0 => n: u64,
    }
}

wire_struct! {
    /// Which packs hold a crate's entries in a namespace, as one immutable generation.
    CrateRecord {
        0 => namespace: Namespace,
        1 => epoch: u64,
        2 => crate_key: CrateKey,
        3 => generation: u64,
        4 => packs: Set<PackRef>,
        5 => source: SourceRef,
        6 => signature: [u8; 64],
    }
}

impl CrateRecord {
    /// The six-field map the server signs: fields 0 to 5, without the signature field.
    ///
    /// Section 2 signs the record without the field the signature goes in, so the signed bytes
    /// are `{0: namespace, 1: epoch, 2: crate_key, 3: generation, 4: packs, 5: source}`, a map of
    /// six, and the namespace is in them, so a `Pr` record is not a `Main` record. The whole
    /// record, signature field 6 included, is still what [`digest_of`](crate::digest_of) hashes.
    pub fn signed_bytes(&self) -> Vec<u8> {
        let mut e = crate::Encoder::new();
        e.map(6);
        e.uint(0);
        self.namespace.encode(&mut e);
        e.uint(1);
        self.epoch.encode(&mut e);
        e.uint(2);
        self.crate_key.encode(&mut e);
        e.uint(3);
        self.generation.encode(&mut e);
        e.uint(4);
        self.packs.encode(&mut e);
        e.uint(5);
        self.source.encode(&mut e);
        e.into_bytes()
    }

    /// The signature of this record under `key`: Ed25519 over
    /// `"blaze/sig/CrateRecord/v4" ‖ 0x00 ‖ `[`signed_bytes`](CrateRecord::signed_bytes), the
    /// domain separation every other signature of the schema uses.
    pub fn sign(&self, key: &ed25519_dalek::SigningKey) -> [u8; 64] {
        use ed25519_dalek::Signer as _;
        key.sign(&crate::signature_input_encoded::<CrateRecord>(&self.signed_bytes())).to_bytes()
    }

    /// This record with field 6 set to [`sign`](CrateRecord::sign)'s signature.
    pub fn signed(&self, key: &ed25519_dalek::SigningKey) -> CrateRecord {
        CrateRecord { signature: self.sign(key), ..self.clone() }
    }

    /// Whether `signature` is this record's under `key`, over the six fields the server signs,
    /// under the `CrateRecord` signature domain.
    pub fn signature_is(&self, key: &ed25519_dalek::VerifyingKey, signature: &[u8; 64]) -> bool {
        crate::verify_encoded_name("CrateRecord", &self.signed_bytes(), key, signature)
    }
}

impl crate::vectors::SelfSigned for CrateRecord {
    fn signed_bytes(&self) -> Vec<u8> {
        CrateRecord::signed_bytes(self)
    }

    fn with_signature(self, key: &ed25519_dalek::SigningKey) -> CrateRecord {
        self.signed(key)
    }

    fn own_signature(&self) -> Option<[u8; 64]> {
        Some(self.signature)
    }
}

wire_struct! {
    /// One pack of a crate's record.
    PackRef {
        0 => pack: Digest,
        1 => size: u64,
        2 => keys: Digest,
        3 => packed: Option<(Digest, u64)>,
        4 => format: u32,
    }
}

wire_struct! {
    /// What one build used: the GC's live roots.
    RootManifest {
        0 => namespace: Namespace,
        1 => source: SourceRef,
        2 => config: Digest,
        3 => spec: Digest,
        4 => used: Set<(CrateKey, u64, Digest)>,
        5 => tests: Set<TestKey>,
    }
}

wire_struct! {
    /// What the server grants a principal for one job. Signed by the server.
    Capability {
        0 => principal: String,
        1 => role: u8,
        2 => namespace: Namespace,
        3 => source: SourceRef,
        4 => execution: Option<ExecutionId>,
        5 => attempt: Option<Attempt>,
        6 => ops: u32,
        7 => not_after_ms: u64,
        8 => epoch: u64,
    }
}

impl Capability {
    /// Whether `signature` is this capability's under `key`: Ed25519 over the capability's own
    /// domain-separated bytes, which the server makes and a client checks.
    pub fn signature_is(&self, key: &ed25519_dalek::VerifyingKey, signature: &[u8; 64]) -> bool {
        crate::verify(key, self, signature)
    }
}

wire_enum! {
    /// A function of distributed codegen, as every executor of an execution names it: the input of
    /// its `WorkName`.
    WorkKey {
        Item => 0 (ItemKey),
        Edge => 1 (EdgeKey),
    }
}

wire_struct! {
    /// The fields of `WorkKey::Item`: a function with a stable name.
    ItemKey {
        0 => unit: Digest,
        1 => stable: Digest,
        2 => env: Digest,
    }
}

wire_struct! {
    /// The fields of `WorkKey::Edge`: a request with no stable name.
    EdgeKey {
        0 => parent: WorkName,
        1 => ordinal: u32,
    }
}

wire_struct! {
    /// A name an executor found, and the digest of the complete local request it holds for it.
    Found {
        0 => name: WorkName,
        1 => tuple: Digest,
    }
}

wire_struct! {
    /// What an executor computed from its front end before it may take work.
    RootSet {
        0 => spec: Digest,
        1 => roots: Digest,
        2 => count: u64,
        3 => index: Digest,
        4 => inputs: Digest,
    }
}

wire_struct! {
    /// An executor's tenure over the work it holds in one execution.
    Lease {
        0 => execution: ExecutionId,
        1 => attempt: Attempt,
        2 => holder: String,
        3 => generation: u64,
    }
}

wire_struct! {
    /// Work given to a holder: a range of the sorted roots, and names granted to it.
    Grant {
        0 => lease: Lease,
        1 => roots: Set<(u64, u64)>,
        2 => names: Set<WorkName>,
    }
}

wire_enum! {
    /// What an executor's batch did with one name.
    ItemOutcome {
        Exported => 0 (EntryRef),
        FromStore => 1 (EntryRef),
        Failed => 2 (Why),
        Local => 3 (Why),
        Lost => 4 (Why),
    }
}

wire_struct! {
    /// The fields of `ItemOutcome::Exported` and `ItemOutcome::FromStore`.
    EntryRef {
        0 => entry: EntryKey,
    }
}

wire_struct! {
    /// The fields of `ItemOutcome::Failed`, `Local` and `Lost`.
    Why {
        0 => why: String,
    }
}

wire_struct! {
    /// One name's result in a batch.
    ItemResult {
        0 => name: WorkName,
        1 => outcome: ItemOutcome,
        2 => found: Set<Found>,
        3 => unnamed: Set<(u32, String)>,
    }
}

wire_struct! {
    /// One batch's results, committed at once.
    BatchResult {
        0 => lease: Lease,
        1 => seq: u64,
        2 => items: Vec<ItemResult>,
        3 => packs: Vec<PackRef>,
        4 => returned_roots: Set<(u64, u64)>,
        5 => returned_names: Set<WorkName>,
    }
}

wire_struct! {
    /// What a run of a test observed.
    InputManifest {
        0 => reads: Set<Observation>,
        1 => env_read: Set<String>,
        2 => spawned: Set<Digest>,
        3 => policy: u32,
    }
}

wire_enum! {
    /// One thing a test run observed.
    Observation {
        File => 0 (FileObservation),
        Dir => 1 (DirObservation),
        Absent => 2 (AbsentObservation),
        Link => 3 (LinkObservation),
    }
}

wire_struct! {
    /// The fields of `Observation::File`.
    FileObservation {
        0 => path: String,
        1 => digest: Digest,
        2 => mode: u32,
        3 => len: u64,
    }
}

wire_struct! {
    /// The fields of `Observation::Dir`.
    DirObservation {
        0 => path: String,
        1 => names: Digest,
        2 => mode: u32,
    }
}

wire_struct! {
    /// The fields of `Observation::Absent`.
    AbsentObservation {
        0 => path: String,
    }
}

wire_struct! {
    /// The fields of `Observation::Link`.
    LinkObservation {
        0 => path: String,
        1 => target: String,
    }
}

wire_struct! {
    /// What a run of a test observed, published with its verdict.
    TestVerdict {
        0 => key: TestKey,
        1 => base: TestBaseKey,
        2 => manifest: Digest,
        3 => name: String,
        4 => passed: bool,
        5 => duration_ns: u64,
        6 => output: Option<Vec<u8>>,
    }
}

wire_struct! {
    /// What a test program needs to run on a machine that did not build it, by digest.
    RuntimeBundle {
        0 => program: Digest,
        1 => programs: Set<(String, Digest)>,
        2 => libraries: Set<(String, Digest)>,
        3 => files: Digest,
        4 => cwd: String,
        5 => env: Set<(String, String)>,
    }
}

wire_struct! {
    /// What admission holds for an executor, a job or a test shard.
    Reservation {
        0 => memory: u64,
        1 => cpus: u32,
        2 => disk: u64,
    }
}

wire_enum! {
    /// Stage timestamps, research 7.12.
    Stage {
        Queued => 0,
        Admitted => 1,
        Assigned => 2,
        Fetched => 3,
        Loaded => 4,
        Compiled => 5,
        Published => 6,
        Linked => 7,
        Tested => 8,
        Reported => 9,
        Done => 10,
    }
}

wire_enum! {
    /// One event of an execution's stream.
    JobEvent {
        Stage => 0 (StageEvent),
        Assigned => 1 (AssignedEvent),
        Published => 2 (PublishedEvent),
        Program => 3 (ProgramEvent),
        Verdict => 4 (TestVerdict),
        Refused => 5 (RefusedEvent),
        Output => 6 (OutputEvent),
        Done => 7 (DoneEvent),
    }
}

wire_struct! {
    /// The fields of `JobEvent::Stage`.
    StageEvent {
        0 => stage: Stage,
        1 => at_ms: u64,
    }
}

wire_struct! {
    /// The fields of `JobEvent::Assigned`.
    AssignedEvent {
        0 => executor: String,
        1 => attempt: Attempt,
    }
}

wire_struct! {
    /// The fields of `JobEvent::Published`.
    PublishedEvent {
        0 => records: Vec<(CrateKey, u64)>,
    }
}

wire_struct! {
    /// The fields of `JobEvent::Program`.
    ProgramEvent {
        0 => name: String,
        1 => blob: Digest,
    }
}

wire_struct! {
    /// The fields of `JobEvent::Refused`.
    RefusedEvent {
        0 => function: String,
        1 => why: String,
    }
}

wire_struct! {
    /// The fields of `JobEvent::Output`.
    OutputEvent {
        0 => stream: u8,
        1 => bytes: Vec<u8>,
    }
}

wire_struct! {
    /// The fields of `JobEvent::Done`.
    DoneEvent {
        0 => outcome: Outcome,
        1 => counters: Vec<(String, u64)>,
    }
}

wire_enum! {
    /// How an execution ended.
    Outcome {
        Passed => 0,
        Failed => 1,
        Refused => 2,
        Lost => 3,
        Cancelled => 4,
        TimedOut => 5,
    }
}

wire_struct! {
    /// What admission holds for an executor.
    ExecutorState {
        0 => id: String,
        1 => trust: u8,
        2 => namespace: Namespace,
        3 => front_end: Option<FrontEndKey>,
        4 => at: Option<SourceRef>,
        5 => reserved: Reservation,
        6 => busy: Option<(ExecutionId, Attempt)>,
    }
}

wire_struct! {
    /// The input of `CrateKey`'s digest.
    CrateKeyInput {
        0 => identity: StoreIdentity,
        1 => triple: String,
        2 => settings: Digest,
        3 => stable: Digest,
    }
}

wire_struct! {
    /// A `UnitSpec` without its source id, for `FrontEndKey`.
    FrontEndUnit {
        0 => name: String,
        1 => version: String,
        2 => target: (u8, String),
        3 => features: Set<String>,
        4 => cfg: Set<InputText>,
        5 => edition: u16,
        6 => deps: Set<(u32, String)>,
        7 => test: bool,
        8 => host: bool,
        9 => profile: ProfileSpec,
    }
}

wire_struct! {
    /// The input of `FrontEndKey`'s digest.
    FrontEndKeyInput {
        0 => units: Vec<FrontEndUnit>,
        1 => lock: Digest,
        2 => cfg_rustflags: Set<InputText>,
        3 => toolchain: StoreIdentity,
        4 => triple: String,
        5 => workspace_manifest: SourcePath,
    }
}

wire_struct! {
    /// The input of `ConfigKey`'s digest.
    ConfigKeyInput {
        0 => kind: u8,
        1 => asked: Set<(String, String)>,
        2 => profiles: Vec<ProfileSpec>,
        3 => platform: Digest,
        4 => rustflags: Set<InputText>,
        5 => toolchain: StoreIdentity,
        6 => workspace_manifest: SourcePath,
    }
}

wire_struct! {
    /// The input of `JobContent`'s digest.
    JobContentInput {
        0 => spec: Digest,
        1 => namespace: Namespace,
    }
}

wire_struct! {
    /// The input of `TestBaseKey`'s digest.
    TestBaseKeyInput {
        0 => binary: Digest,
        1 => test: String,
        2 => run: RunSpec,
        3 => platform: Digest,
        4 => sandbox: u32,
        5 => bundle: Digest,
    }
}

// The digest newtypes. Each is its inner value, a 32-byte string, and each names the type its
// digest is taken of, so the domain separation covers the name as well as the bytes.

macro_rules! digest_newtype {
    ($(#[$meta:meta])* $name:ident => $input:ty) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub Digest);

        impl Wire for $name {
            fn encode(&self, e: &mut crate::Encoder) {
                e.bytes(&self.0);
            }

            fn decode(r: &mut crate::Reader<'_>) -> Result<Self, crate::Error> {
                let mut out = [0u8; 32];
                out.copy_from_slice(r.bytes_of(32)?);
                Ok($name(out))
            }
        }

        impl Named for $name {
            const NAME: &'static str = stringify!($name);
        }

        impl $name {
            /// The digest of one input value, under this type's domain separation.
            pub fn of(value: &$input) -> $name {
                $name(crate::digest_of_encoded::<Self>(&crate::encode(value)))
            }
        }

        impl crate::vectors::Sample for $name {
            fn sample(g: &mut crate::vectors::Gen) -> Self {
                $name(<[u8; 32]>::sample(g))
            }
        }
    };
}

/// `Identity` in ra-jit-store, unchanged: `sha256` of the FORMAT, the build stamp and the
/// toolchain, each behind a little-endian `u64` of its length (`lib.rs:173-181`). It is not a
/// digest of this crate's domain separation, because it must be the store's own key, so a
/// `StoreIdentity` on the wire means the same store a local build reads; the digest below is only
/// what a golden vector and a blob that names the value use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StoreIdentity(pub Digest);

impl Wire for StoreIdentity {
    fn encode(&self, e: &mut crate::Encoder) {
        e.bytes(&self.0);
    }

    fn decode(r: &mut crate::Reader<'_>) -> Result<Self, crate::Error> {
        let mut out = [0u8; 32];
        out.copy_from_slice(r.bytes_of(32)?);
        Ok(StoreIdentity(out))
    }
}

impl Named for StoreIdentity {
    const NAME: &'static str = "StoreIdentity";
}

impl crate::vectors::Sample for StoreIdentity {
    fn sample(g: &mut crate::vectors::Gen) -> Self {
        StoreIdentity(<[u8; 32]>::sample(g))
    }
}

/// The caller's key of one compiled function, unchanged (cache.md): the store's
/// `sha256(identity ‖ key)`, not a digest this crate computes. [`digest_of`](crate::digest_of)
/// of one is its wire digest, for golden vectors and for a blob that names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryKey(pub Digest);

impl Wire for EntryKey {
    fn encode(&self, e: &mut crate::Encoder) {
        e.bytes(&self.0);
    }

    fn decode(r: &mut crate::Reader<'_>) -> Result<Self, crate::Error> {
        let mut out = [0u8; 32];
        out.copy_from_slice(r.bytes_of(32)?);
        Ok(EntryKey(out))
    }
}

impl Named for EntryKey {
    const NAME: &'static str = "EntryKey";
}

impl crate::vectors::Sample for EntryKey {
    fn sample(g: &mut crate::vectors::Gen) -> Self {
        EntryKey(<[u8; 32]>::sample(g))
    }
}

digest_newtype! {
    /// The home crate of a set of entries, under one profile.
    CrateKey => CrateKeyInput
}

digest_newtype! {
    /// The part of a `BuildSpec` a warm front end depends on.
    FrontEndKey => FrontEndKeyInput
}

digest_newtype! {
    /// The `BuildSpec` without what changes from commit to commit.
    ConfigKey => ConfigKeyInput
}

digest_newtype! {
    /// A job's content: what merges duplicates.
    JobContent => JobContentInput
}

digest_newtype! {
    /// A test before it runs.
    TestBaseKey => TestBaseKeyInput
}

digest_newtype! {
    /// What a pass is kept under: `[TestBaseKey, InputManifest's digest]`.
    TestKey => (TestBaseKey, Digest)
}

digest_newtype! {
    /// A function of distributed codegen, as every executor of an execution names it: the digest of
    /// its `WorkKey`.
    WorkName => WorkKey
}

impl StoreIdentity {
    /// The store's own identity, as `ra-jit-store`'s `Identity::new` makes it (`lib.rs:173-181`):
    /// `sha256` of each of the FORMAT, the build stamp and the toolchain behind a little-endian
    /// `u64` of its length.
    pub fn from_store(format: u32, built_from: &[u8], toolchain: &str) -> StoreIdentity {
        use sha2::{Digest as _, Sha256};
        let mut hasher = Sha256::new();
        for field in [&format.to_le_bytes()[..], built_from, toolchain.as_bytes()] {
            hasher.update((field.len() as u64).to_le_bytes());
            hasher.update(field);
        }
        StoreIdentity(hasher.finalize().into())
    }
}

/// The server's, one per execution of a `JobContent`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExecutionId(pub u64);

impl Wire for ExecutionId {
    fn encode(&self, e: &mut crate::Encoder) {
        e.uint(self.0);
    }

    fn decode(r: &mut crate::Reader<'_>) -> Result<Self, crate::Error> {
        Ok(ExecutionId(r.uint()?))
    }
}

impl Named for ExecutionId {
    const NAME: &'static str = "ExecutionId";
}

impl crate::vectors::Sample for ExecutionId {
    fn sample(g: &mut crate::vectors::Gen) -> Self {
        ExecutionId(u64::sample(g))
    }
}

/// Goes up when an execution is reassigned; fences events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Attempt(pub u32);

impl Wire for Attempt {
    fn encode(&self, e: &mut crate::Encoder) {
        e.uint(u64::from(self.0));
    }

    fn decode(r: &mut crate::Reader<'_>) -> Result<Self, crate::Error> {
        let at = r.position();
        let v = r.uint()?;
        u32::try_from(v).map(Attempt).map_err(|_| crate::Error {
            kind: crate::ErrorKind::UnexpectedType { expected: "an attempt in range" },
            at,
        })
    }
}

impl Named for Attempt {
    const NAME: &'static str = "Attempt";
}

impl crate::vectors::Sample for Attempt {
    fn sample(g: &mut crate::vectors::Gen) -> Self {
        Attempt(u32::sample(g))
    }
}
