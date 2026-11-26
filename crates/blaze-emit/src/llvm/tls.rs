use super::*;
use blaze_code::tls::{DESC_ROOT, ROOT_OWN, TABLE_LEN, TABLE_SLOTS};

impl Lower<'_, '_> {
    /// The import word for a Blaze TLS symbol is a table index, including zero.
    /// macOS uses libpthread's specific-data area (TPIDRRO_EL0 with low bits
    /// cleared), Linux the native thread pointer. DESC_ROOT identifies the layout
    /// chosen by the launcher and the getter initializes the table on a miss.
    pub(super) fn tls(&mut self, i: usize, id: RefId) -> Result<(), LowerError> {
        if self.view.head.ptr_bits != 64 {
            return Err(self.error("indexed TLS needs the native AArch64 descriptor"));
        }
        let (desc_target, desc) = self.link(Outside::TlsDesc, Space::Static)?;
        let (getter_target, getter) = self.link(Outside::TlsGetter, Space::Func)?;
        self.implicit(desc_target);
        self.implicit(getter_target);
        self.declaration(
            AString::from_str_in(&desc, self.alloc),
            format_in!(self.alloc, "{desc} = external global i8"),
        )?;
        self.declaration(
            AString::from_str_in(&getter, self.alloc),
            format_in!(self.alloc, "declare ptr {getter}(ptr, i64, ptr)"),
        )?;
        let index = AString::from_str_in(self.ref_name(id)?, self.alloc);
        self.declaration(
            AString::from_str_in(&index, self.alloc),
            format_in!(self.alloc, "{index} = extern_weak global i8"),
        )?;
        self.declaration(
            AString::from_str_in("@llvm.thread.pointer", self.alloc),
            AString::from_str_in("declare ptr @llvm.thread.pointer()", self.alloc),
        )?;
        let mask = !ROOT_OWN;
        format_into!(
            &mut self.ir,
            "  %tlsindex{i} = ptrtoint ptr {index} to i64\n\
             %tlsrootptr{i} = getelementptr i8, ptr {desc}, i32 {DESC_ROOT}\n\
             %tlsroot{i} = load i64, ptr %tlsrootptr{i}, align 8\n\
             %tlsown{i} = icmp slt i64 %tlsroot{i}, 0\n\
             %tlsoff{i} = and i64 %tlsroot{i}, {mask}\n\
             br i1 %tlsown{i}, label %tlslinux{i}, label %tlsdarwin{i}\n\
             tlslinux{i}:\n\
             %tlstp{i} = call ptr @llvm.thread.pointer()\n\
             br label %tlslookup{i}\n\
             tlsdarwin{i}:\n\
             %tsdreg{i} = call i64 asm sideeffect \"mrs $0, tpidrro_el0\", \"=r\"()\n\
             %tsdalign{i} = and i64 %tsdreg{i}, -8\n\
             %tsd{i} = inttoptr i64 %tsdalign{i} to ptr\n\
             br label %tlslookup{i}\n\
             tlslookup{i}:\n\
             %tlsbase{i} = phi ptr [ %tlstp{i}, %tlslinux{i} ], [ %tsd{i}, %tlsdarwin{i} ]\n\
             %tlsat{i} = getelementptr i8, ptr %tlsbase{i}, i64 %tlsoff{i}\n\
             %tlstable{i} = load ptr, ptr %tlsat{i}, align 8\n\
             %tlsnull{i} = icmp eq ptr %tlstable{i}, null\n\
             br i1 %tlsnull{i}, label %tlsslow{i}, label %tlslen{i}\n\
             tlslen{i}:\n\
             %tlslenptr{i} = getelementptr i8, ptr %tlstable{i}, i32 {TABLE_LEN}\n\
             %tlsn{i} = load i64, ptr %tlslenptr{i}, align 8\n\
             %tlsfits{i} = icmp ult i64 %tlsindex{i}, %tlsn{i}\n\
             br i1 %tlsfits{i}, label %tlsread{i}, label %tlsslow{i}\n\
             tlsread{i}:\n\
             %tlsslots{i} = getelementptr i8, ptr %tlstable{i}, i32 {TABLE_SLOTS}\n\
             %tlsslot{i} = getelementptr ptr, ptr %tlsslots{i}, i64 %tlsindex{i}\n\
             %tlscopy{i} = load ptr, ptr %tlsslot{i}, align 8\n\
             %tlsmissing{i} = icmp eq ptr %tlscopy{i}, null\n\
             br i1 %tlsmissing{i}, label %tlsslow{i}, label %tlsdone{i}\n\
             tlsslow{i}:\n\
             %tlsmade{i} = call ptr {getter}(ptr {desc}, i64 %tlsindex{i}, ptr %tlsat{i})\n\
             br label %tlsdone{i}\n\
             tlsdone{i}:\n\
             %v{i} = phi ptr [ %tlscopy{i}, %tlsread{i} ], [ %tlsmade{i}, %tlsslow{i} ]\n"
        );
        Ok(())
    }
}
