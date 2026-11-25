import ctypes
import sys

libc = ctypes.CDLL(None)
key_type = ctypes.c_ulong if sys.platform == "darwin" else ctypes.c_uint
key = key_type()
assert libc.pthread_key_create(ctypes.byref(key), None) == 0
d = ctypes.CDLL(sys.argv[1])
case = sys.argv[2]
if case == "rust_record":
    assert d.answer() == 42
    d.identity.restype = ctypes.c_void_p
    identity = d.identity()
    assert identity
    if sys.platform == "darwin":
        assert ctypes.string_at(identity, 4) == b"\xcf\xfa\xed\xfe"
    else:
        assert ctypes.c_void_p.from_address(identity).value == identity
elif case == "rust_data_tls":
    assert d.answer() == 42
    d.descriptor.restype = ctypes.POINTER(ctypes.c_uint64)
    desc = d.descriptor()
    assert desc[0] != key.value
    if sys.platform == "darwin":
        assert desc[1] == desc[0] * 8 and desc[1] > 0
    else:
        # With no native TLS, this DSO's ROOT is byte zero of its own TLS block.
        # Ask glibc for that address independently of the descriptor's TPREL relocation.
        owned = 1 << 63
        assert desc[1] & owned
        modid = ctypes.c_size_t()
        rtld_di_tls_modid = 9
        libc.dlinfo.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_void_p]
        assert libc.dlinfo(d._handle, rtld_di_tls_modid, ctypes.byref(modid)) == 0
        assert modid.value
        tls_index = (ctypes.c_size_t * 2)(modid.value, 0)
        libc.__tls_get_addr.argtypes = [ctypes.POINTER(ctypes.c_size_t)]
        libc.__tls_get_addr.restype = ctypes.c_void_p
        d.thread_pointer.restype = ctypes.c_void_p
        assert d.thread_pointer() + (desc[1] & ~owned) == libc.__tls_get_addr(tls_index)
    d.allocate.restype = ctypes.c_void_p
    ptr = d.allocate()
    assert ptr
    ctypes.memset(ptr, 0x5A, 32)
    libc.free.argtypes = [ctypes.c_void_p]
    libc.free(ptr)
elif case == "empty_library":
    assert d is not None
else:
    raise ValueError(case)
assert libc.pthread_key_delete(key) == 0
print("independent Rust CodeRecord dlopen/dlsym passed")
