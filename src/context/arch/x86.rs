use core::{mem, sync::atomic::AtomicBool};

use alloc::sync::Arc;

use crate::{
    gdt::{pcr, GDT_USER_FS, GDT_USER_GS},
    paging::{RmmA, RmmArch, TableKind},
    percpu::PercpuBlock,
    syscall::FloatRegisters,
};

use core::mem::offset_of;
use spin::Once;

/// This must be used by the kernel to ensure that context switches are done atomically
/// Compare and exchange this to true when beginning a context switch on any CPU
/// The `Context::switch_to` function will set it back to false, allowing other CPU's to switch
/// This must be done, as no locks can be held on the stack during switch
pub static CONTEXT_SWITCH_LOCK: AtomicBool = AtomicBool::new(false);

const ST_RESERVED: u128 = 0xFFFF_FFFF_FFFF_0000_0000_0000_0000_0000;

pub const KFX_ALIGN: usize = 16;
pub const KSTACK_SIZE: usize = 65536;
pub const KSTACK_ALIGN: usize = 16;

#[derive(Clone, Debug)]
#[repr(C)]
pub struct Context {
    /// EFLAGS register
    eflags: usize,
    /// EBX register
    ebx: usize,
    /// EDI register
    edi: usize,
    /// ESI register
    esi: usize,
    /// Base pointer
    ebp: usize,
    /// Stack pointer
    pub(crate) esp: usize,
    /// FSBASE.
    ///
    /// NOTE: Same fsgsbase behavior as with gsbase.
    pub(crate) fsbase: usize,
    /// GSBASE.
    ///
    /// NOTE: Without fsgsbase, this register will strictly be equal to the register value when
    /// running. With fsgsbase, this is neither saved nor restored upon every syscall (there is no
    /// need to!), and thus it must be re-read from the register before copying this struct.
    pub(crate) gsbase: usize,
    userspace_io_allowed: bool,
}

impl Context {
    pub fn new() -> Context {
        Context {
            eflags: 0,
            ebx: 0,
            edi: 0,
            esi: 0,
            ebp: 0,
            esp: 0,
            fsbase: 0,
            gsbase: 0,
            userspace_io_allowed: false,
        }
    }

    pub fn set_stack(&mut self, address: usize) {
        self.esp = address;
    }
}
impl super::Context {
    pub fn get_fx_regs(&self) -> FloatRegisters {
        let mut regs = unsafe { self.kfx.as_ptr().cast::<FloatRegisters>().read() };
        regs._reserved = 0;
        let mut new_st = regs.st_space;
        for st in &mut new_st {
            // Only allow access to the 80 lowest bits
            *st &= !ST_RESERVED;
        }
        regs.st_space = new_st;
        regs
    }

    pub fn set_fx_regs(&mut self, mut new: FloatRegisters) {
        {
            let old = unsafe { &*(self.kfx.as_ptr().cast::<FloatRegisters>()) };
            new._reserved = old._reserved;
            let old_st = new.st_space;
            let mut new_st = new.st_space;
            for (new_st, old_st) in new_st.iter_mut().zip(&old_st) {
                *new_st &= !ST_RESERVED;
                *new_st |= old_st & ST_RESERVED;
            }
            new.st_space = new_st;

            // Make sure we don't use `old` from now on
        }

        unsafe {
            self.kfx.as_mut_ptr().cast::<FloatRegisters>().write(new);
        }
    }
    pub fn set_userspace_io_allowed(&mut self, allowed: bool) {
        self.arch.userspace_io_allowed = allowed;

        if self.id == super::context_id() {
            unsafe {
                crate::gdt::set_userspace_io_allowed(allowed);
            }
        }
    }
}

pub static EMPTY_CR3: Once<rmm::PhysicalAddress> = Once::new();

// SAFETY: EMPTY_CR3 must be initialized.
pub unsafe fn empty_cr3() -> rmm::PhysicalAddress {
    debug_assert!(EMPTY_CR3.poll().is_some());
    *EMPTY_CR3.get_unchecked()
}

/// Switch to the next context by restoring its stack and registers
pub unsafe fn switch_to(prev: &mut super::Context, next: &mut super::Context) {
    if let Some(ref stack) = next.kstack {
        crate::gdt::set_tss_stack(stack.as_ptr() as usize + stack.len());
    }
    crate::gdt::set_userspace_io_allowed(next.arch.userspace_io_allowed);

    core::arch::asm!("
        fxsave [{prev_fx}]
        fxrstor [{next_fx}]
        ", prev_fx = in(reg) prev.kfx.as_mut_ptr(),
        next_fx = in(reg) next.kfx.as_ptr(),
    );

    {
        let gdt = &mut (&mut *pcr()).gdt;

        prev.arch.fsbase = gdt[GDT_USER_FS].offset() as usize;
        gdt[GDT_USER_FS].set_offset(next.arch.fsbase as u32);
        prev.arch.gsbase = gdt[GDT_USER_GS].offset() as usize;
        gdt[GDT_USER_GS].set_offset(next.arch.gsbase as u32);
    }
    PercpuBlock::current().new_addrsp_tmp.set(next.addr_space.clone());

    core::arch::asm!(
        "call {inner}",
        inner = sym switch_to_inner,
        in("ecx") &mut prev.arch,
        in("edx") &mut next.arch,
    );
}

// Check disassembly!
#[naked]
unsafe extern "cdecl" fn switch_to_inner() {
    use Context as Cx;

    core::arch::asm!(
        // As a quick reminder for those who are unfamiliar with the System V ABI (extern "C"):
        //
        // - the current parameters are passed in the registers `edi`, `esi`,
        // - we can modify scratch registers, e.g. rax
        // - we cannot change callee-preserved registers arbitrarily, e.g. ebx, which is why we
        //   store them here in the first place.
        concat!("
        // ecx is prev, edx is next

        // Save old registers, and load new ones
        mov [ecx + {off_ebx}], ebx
        mov ebx, [edx + {off_ebx}]

        mov [ecx + {off_edi}], edi
        mov edi, [edx + {off_edi}]

        mov [ecx + {off_esi}], esi
        mov esi, [edx + {off_esi}]

        mov [ecx + {off_ebp}], ebp
        mov ebp, [edx + {off_ebp}]

        mov [ecx + {off_esp}], esp
        mov esp, [edx + {off_esp}]

        // push EFLAGS (can only be modified via stack)
        pushfd
        // pop EFLAGS into `self.eflags`
        pop DWORD PTR [ecx + {off_eflags}]

        // push `next.eflags`
        push DWORD PTR [edx + {off_eflags}]
        // pop into EFLAGS
        popfd

        // When we return, we cannot even guarantee that the return address on the stack, points to
        // the calling function, `context::switch`. Thus, we have to execute this Rust hook by
        // ourselves, which will unlock the contexts before the later switch.

        // Note that switch_finish_hook will be responsible for executing `ret`.
        jmp {switch_hook}

        "),

        off_eflags = const(offset_of!(Cx, eflags)),

        off_ebx = const(offset_of!(Cx, ebx)),
        off_edi = const(offset_of!(Cx, edi)),
        off_esi = const(offset_of!(Cx, esi)),
        off_ebp = const(offset_of!(Cx, ebp)),
        off_esp = const(offset_of!(Cx, esp)),

        switch_hook = sym crate::context::switch_finish_hook,
        options(noreturn),
    );
}
