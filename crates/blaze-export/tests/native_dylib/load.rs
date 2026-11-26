//! Execute the exported C ABI in a fresh process under the host loader's deadline.
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

const CHILD_IMAGE: &str = "BLAZE_TEST_CDYLIB_IMAGE";
const VERIFIED: i32 = 42;

pub(super) fn run(bytes: &[u8], case: &str, test: &str) {
    if let Some(path) = std::env::var_os(CHILD_IMAGE) {
        let path = Path::new(&path);
        assert!(std::fs::read(path).unwrap() == bytes, "{} differs from the test's export", path.display());
        verify(path, case);
        // libtest also exits successfully when an exact filter matches zero tests.
        std::process::exit(VERIFIED);
    }
    let extension = std::env::consts::DLL_EXTENSION;
    let status = crate::host::run(&format!("{case}.{extension}"), bytes, |path| {
        let mut child = Command::new(std::env::current_exe().unwrap());
        child.args(["--exact", test, "--nocapture"]).env(CHILD_IMAGE, path);
        child
    });
    assert_eq!(status.code(), Some(VERIFIED), "loading {case}: {status}");
}

// pthread_key_t is unsigned long on Darwin and unsigned int on glibc.
#[cfg(target_os = "macos")]
type ThreadKey = std::ffi::c_ulong;
#[cfg(target_os = "linux")]
type ThreadKey = std::ffi::c_uint;

#[cfg_attr(target_os = "linux", link(name = "dl"))]
unsafe extern "C" {
    fn dlopen(path: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
    fn dlclose(handle: *mut c_void) -> c_int;
    fn pthread_key_create(key: *mut ThreadKey, destructor: Option<unsafe extern "C" fn(*mut c_void)>) -> c_int;
    fn pthread_key_delete(key: ThreadKey) -> c_int;
    fn free(ptr: *mut c_void);
}

fn loader_error() -> String {
    // SAFETY: dlerror returns null or a thread-local C string valid until the next loader call.
    let why = unsafe { dlerror() };
    assert!(!why.is_null(), "the dynamic loader returned no diagnostic");
    unsafe { CStr::from_ptr(why) }.to_string_lossy().into_owned()
}

fn verify(path: &Path, case: &str) {
    let filename = CString::new(path.as_os_str().as_bytes()).unwrap();
    let mut key: ThreadKey = 0;
    // SAFETY: key is writable and no destructor is installed. Reserving it before dlopen
    // distinguishes our key from the one the DSO initializer must acquire for itself.
    assert_eq!(unsafe { pthread_key_create(&raw mut key, None) }, 0);
    const RTLD_NOW: c_int = 2;
    // SAFETY: filename is NUL-terminated and names the image written by the parent test.
    let library = unsafe { dlopen(filename.as_ptr(), RTLD_NOW) };
    assert!(!library.is_null(), "dlopen {}: {}", path.display(), loader_error());
    let symbol = |name: &CStr| {
        // SAFETY: the library remains open and name is a C string for the duration of lookup.
        let address = unsafe { dlsym(library, name.as_ptr()) };
        assert!(!address.is_null(), "dlsym {name:?}: {}", loader_error());
        address
    };
    // SAFETY: these fixture exports take no arguments. answer returns a 32-bit integer and
    // each other named function returns a pointer. POSIX permits calling the dlsym address
    // with the symbol's exact function ABI while the image remains loaded.
    unsafe {
        if case != "empty_library" {
            let answer: unsafe extern "C" fn() -> u32 = std::mem::transmute(symbol(c"answer"));
            assert_eq!(answer(), 42);
        }
        let pointer = |name: &CStr| {
            let get: unsafe extern "C" fn() -> *mut c_void = std::mem::transmute(symbol(name));
            let value = get();
            assert!(!value.is_null(), "{name:?} returned null");
            value
        };
        match case {
            "rust_record" => {
                let identity = pointer(c"identity");
                // Darwin's __dso_handle names its Mach header. GNU's handle points to itself.
                #[cfg(target_os = "macos")]
                assert_eq!(identity.cast::<u32>().read(), 0xfeed_facf);
                #[cfg(target_os = "linux")]
                assert_eq!(identity.cast::<*mut c_void>().read(), identity);
            }
            "rust_data_tls" => {
                // descriptor() returns the initialized, two-word, aligned TLS descriptor.
                let [owned_key, offset] = pointer(c"descriptor").cast::<[u64; 2]>().read();
                assert_ne!(owned_key, u64::from(key));
                #[cfg(target_os = "macos")]
                {
                    assert_eq!(offset, owned_key.checked_mul(8).unwrap());
                    assert!(offset > 0);
                }
                #[cfg(target_os = "linux")]
                check_tls_root(library, pointer(c"thread_pointer").addr(), offset);

                // allocate() calls the host malloc(32), so the entire block is writable and
                // the matching host free may release it exactly once.
                let block = pointer(c"allocate");
                block.cast::<u8>().write_bytes(0x5a, 32);
                free(block);
            }
            "empty_library" => {}
            _ => panic!("unknown cdylib fixture {case}"),
        }
        // The pointers returned by this image are no longer used after dlclose.
        assert_eq!(dlclose(library), 0, "dlclose {}: {}", path.display(), loader_error());
        assert_eq!(pthread_key_delete(key), 0);
    }
}

#[cfg(target_os = "linux")]
fn check_tls_root(library: *mut c_void, thread_pointer: usize, offset: u64) {
    #[repr(C)]
    struct TlsIndex {
        module: usize,
        offset: usize,
    }
    unsafe extern "C" {
        fn dlinfo(handle: *mut c_void, request: c_int, info: *mut c_void) -> c_int;
        fn __tls_get_addr(index: *const TlsIndex) -> *mut c_void;
    }
    const RTLD_DI_TLS_MODID: c_int = 9;
    const OWNED: u64 = 1 << 63;
    assert_ne!(offset & OWNED, 0);
    let mut module = 0usize;
    // SAFETY: RTLD_DI_TLS_MODID writes one size_t for the open handle. This fixture has
    // no native TLS, so its Rust root starts at offset zero in that module's TLS block.
    assert_eq!(unsafe { dlinfo(library, RTLD_DI_TLS_MODID, (&raw mut module).cast()) }, 0);
    assert_ne!(module, 0);
    let index = TlsIndex { module, offset: 0 };
    let root = unsafe { __tls_get_addr(&raw const index) };
    assert!(!root.is_null());
    assert_eq!(thread_pointer.checked_add(usize::try_from(offset & !OWNED).unwrap()), Some(root.addr()));
}
