use crate::core::guest_regs_addr;
use crate::core::jit_asm_addr;
use crate::core::thread_regs;
use crate::core::emu::Emu;
use crate::core::hle::bios;
use crate::core::memory::regions;
use crate::jit::analyzer::asm_analyzer::AsmAnalyzer;
#[cfg(target_arch = "arm")]
use crate::jit::assembler::block_asm::BlockAsm;
use crate::jit::assembler::{GuestInstOffset, GUEST_REGS_LENGTH};
use crate::jit::disassembler::lookup_table::lookup_opcode;
use crate::jit::disassembler::thumb::lookup_table_thumb::lookup_thumb_opcode;
use crate::jit::inst_branch_handler::call_jit_fun;
use crate::jit::inst_info::InstInfo;
use crate::jit::jit_asm_common_funs::exit_guest_context;
use crate::jit::op::Op;
use crate::jit::reg::Reg;
use crate::jit::reg::{reg_reserve, RegReserve};
use crate::jit::Cond;
use crate::logging::{branch_println, debug_println};
use crate::mmap::PAGE_SHIFT;
use crate::{get_jit_asm_ptr, BRANCH_LOG, DEBUG_LOG, IS_DEBUG};
use bilge::prelude::*;
use static_assertions::const_assert_eq;
use std::arch::{asm, naked_asm};
use std::hint::assert_unchecked;
use std::intrinsics::unlikely;
use std::{mem, slice};
#[cfg(target_arch = "arm")]
use vixl::Label;
use xxhash_rust::xxh32::xxh32;

#[derive(Default)]
#[cfg(any(debug_assertions, target_os = "linux"))]
pub struct JitDebugInfo {
    pub inst_offsets: Vec<usize>,
    pub block_offsets: Vec<usize>,
    pub blocks: Vec<(u32, usize, usize)>,
}

#[cfg(any(debug_assertions, target_os = "linux"))]
impl JitDebugInfo {
    pub fn resize(&mut self, basic_blocks_size: usize, insts_size: usize) {
        self.inst_offsets.resize(insts_size + 1, 0);
        self.block_offsets.resize(basic_blocks_size, 0);
        self.blocks.clear();
    }

    pub fn record_basic_block_offset(&mut self, basic_block_index: usize, offset: usize) {
        self.block_offsets[basic_block_index] = offset;
    }

    pub fn record_basic_block(&mut self, basic_block_start_pc: u32, offset: usize, size: usize) {
        self.blocks.push((basic_block_start_pc, offset, size));
    }

    pub fn record_inst_offset(&mut self, inst_index: usize, offset: usize) {
        self.inst_offsets[inst_index] = offset;
    }

    fn print_info(&self, start_pc: u32, thumb: bool) {
        println!("basic block offsets:");
        for (i, offset) in self.block_offsets.iter().enumerate() {
            print!("({i}, 0x{offset:x}),");
        }
        println!();
        println!("insts offsets:");
        for (i, offset) in self.inst_offsets.iter().enumerate() {
            print!("(0x{:x}, 0x{offset:x}),", start_pc + (i << if thumb { 1 } else { 2 }) as u32);
        }
        println!();
    }
}

#[derive(Default)]
#[cfg(all(not(debug_assertions), not(target_os = "linux")))]
pub struct JitDebugInfo {}

#[cfg(all(not(debug_assertions), not(target_os = "linux")))]
impl JitDebugInfo {
    pub fn resize(&mut self, basic_blocks_size: usize, insts_size: usize) {}
    pub fn record_basic_block_offset(&mut self, basic_block_index: usize, offset: usize) {}
    pub fn record_basic_block(&mut self, basic_block_start_pc: u32, offset: usize, size: usize) {}
    pub fn record_inst_offset(&mut self, inst_index: usize, offset: usize) {}
    fn print_info(&self, start_pc: u32, thumb: bool) {}
}

#[cfg(target_arch = "arm")]
pub struct JitForwardBranch {
    pub inst_index: usize,
    pub target_pc: u32,
    pub dirty_guest_regs: RegReserve,
    pub guest_regs_mapping: [Reg; GUEST_REGS_LENGTH],
    pub bind_label: Label,
}

#[cfg(target_arch = "arm")]
impl JitForwardBranch {
    pub fn new(inst_index: usize, target_pc: u32, dirty_guest_regs: RegReserve, guest_regs_mapping: [Reg; GUEST_REGS_LENGTH], bind_label: Label) -> Self {
        JitForwardBranch {
            inst_index,
            target_pc,
            dirty_guest_regs,
            guest_regs_mapping,
            bind_label,
        }
    }
}

#[cfg(target_arch = "arm")]
pub struct JitRunSchedulerLabel {
    pub inst_index: usize,
    pub target_pc: u32,
    pub dirty_guest_regs: RegReserve,
    pub guest_regs_mapping: [Reg; GUEST_REGS_LENGTH],
    pub bind_label: Label,
    pub continue_label: Label,
    pub exit_label: Option<Label>,
}

#[cfg(target_arch = "arm")]
impl JitRunSchedulerLabel {
    pub fn new(
        inst_index: usize,
        target_pc: u32,
        dirty_guest_regs: RegReserve,
        guest_regs_mapping: [Reg; GUEST_REGS_LENGTH],
        bind_label: Label,
        continue_label: Label,
        exit_label: Option<Label>,
    ) -> Self {
        JitRunSchedulerLabel {
            inst_index,
            target_pc,
            dirty_guest_regs,
            guest_regs_mapping,
            bind_label,
            continue_label,
            exit_label,
        }
    }
}

#[cfg(target_arch = "arm")]
pub struct JitCondIndirectBranch {
    pub inst_index: usize,
    pub dirty_guest_regs: RegReserve,
    pub guest_regs_mapping: [Reg; GUEST_REGS_LENGTH],
    pub bind_label: Label,
}

#[cfg(target_arch = "arm")]
impl JitCondIndirectBranch {
    pub fn new(inst_index: usize, dirty_guest_regs: RegReserve, guest_regs_mapping: [Reg; GUEST_REGS_LENGTH], bind_label: Label) -> Self {
        JitCondIndirectBranch {
            inst_index,
            dirty_guest_regs,
            guest_regs_mapping,
            bind_label,
        }
    }
}

// Out-of-line quantum stub for a linked external branch (scheduler-at-block-end): when the
// inline cycle check trips, jump here, call exe_scheduler, then branch back to continue_label
// (the resume point after the check). No guest-reg/PC bookkeeping — PC and dirty regs were
// already committed at the branch, and exe_scheduler is the same call pre_branch makes.
#[cfg(target_arch = "arm")]
pub struct JitLinkSchedStub {
    pub bind_label: Label,
    pub continue_label: Label,
    pub current_pc: u32,
}

#[cfg(target_arch = "arm")]
impl JitLinkSchedStub {
    pub fn new(bind_label: Label, continue_label: Label, current_pc: u32) -> Self {
        JitLinkSchedStub {
            bind_label,
            continue_label,
            current_pc,
        }
    }
}

pub struct JitBuf {
    pub guest_pc_start: u32,
    pub insts: Vec<InstInfo>,
    pub insts_cycle_counts: Vec<u16>,
    #[cfg(target_arch = "arm")]
    pub forward_branches: Vec<JitForwardBranch>,
    #[cfg(target_arch = "arm")]
    pub run_scheduler_labels: Vec<JitRunSchedulerLabel>,
    #[cfg(target_arch = "arm")]
    pub cond_indirect_branches: Vec<JitCondIndirectBranch>,
    #[cfg(target_arch = "arm")]
    pub link_sched_stubs: Vec<JitLinkSchedStub>,
    pub debug_info: JitDebugInfo,
}

impl JitBuf {
    fn new() -> Self {
        JitBuf {
            guest_pc_start: 0,
            insts: Vec::new(),
            insts_cycle_counts: Vec::new(),
            #[cfg(target_arch = "arm")]
            forward_branches: Vec::new(),
            #[cfg(target_arch = "arm")]
            run_scheduler_labels: Vec::new(),
            #[cfg(target_arch = "arm")]
            cond_indirect_branches: Vec::new(),
            #[cfg(target_arch = "arm")]
            link_sched_stubs: Vec::new(),
            debug_info: JitDebugInfo::default(),
        }
    }

    fn clear_all(&mut self) {
        self.insts.clear();
        self.insts_cycle_counts.clear();
        #[cfg(target_arch = "arm")]
        {
            self.forward_branches.clear();
            self.run_scheduler_labels.clear();
            self.cond_indirect_branches.clear();
            self.link_sched_stubs.clear();
        }
    }
}

pub const RETURN_STACK_SIZE: usize = 64;
pub const MAX_STACK_DEPTH_SIZE: usize = 9 * 1024 * 1024;

#[bitsize(8)]
#[derive(FromBits)]
struct JitRuntimeDataPacked {
    in_interrupt: bool,
    idle_loop: bool,
    _unused: u6,
}

// Bit of JitRuntimeDataPacked.idle_loop, for the emitted flag store in the ARM7
// idle-loop exit (both backends). Kept next to the struct so a layout change can't
// silently strand the emitters again (the u32→u8 repack left them setting bit 31's
// old byte — a padding byte — so the runtime flag never became true).
pub const IDLE_LOOP_FLAG_MASK: u8 = 0x02;

#[repr(C, align(32))]
pub struct JitRuntimeData {
    pub accumulated_cycles: u16,
    pub pre_cycle_count_sum: u16,
    data_packed: JitRuntimeDataPacked,
    return_stack_ptr: u8,
    pub return_stack: [u32; RETURN_STACK_SIZE],
    pub host_sp: usize,
    pub interrupt_sp: usize,
    #[cfg(debug_assertions)]
    branch_out_pc: u32,
}

impl JitRuntimeData {
    fn new() -> Self {
        debug_assert!(JitRuntimeDataPacked::from(IDLE_LOOP_FLAG_MASK).idle_loop());
        JitRuntimeData {
            pre_cycle_count_sum: 0,
            accumulated_cycles: 0,
            host_sp: 0,
            return_stack_ptr: 0,
            data_packed: JitRuntimeDataPacked::from(0),
            return_stack: [u32::MAX; RETURN_STACK_SIZE],
            interrupt_sp: 0,
            #[cfg(debug_assertions)]
            branch_out_pc: u32::MAX,
        }
    }

    #[cfg(debug_assertions)]
    pub const fn get_branch_out_pc_offset() -> usize {
        mem::offset_of!(JitRuntimeData, branch_out_pc)
    }

    #[cfg(not(debug_assertions))]
    pub const fn get_branch_out_pc_offset() -> u8 {
        panic!()
    }

    #[cfg(debug_assertions)]
    pub fn set_branch_out_pc(&mut self, pc: u32) {
        self.branch_out_pc = pc;
    }

    #[cfg(not(debug_assertions))]
    pub fn set_branch_out_pc(&mut self, _: u32) {
        panic!()
    }

    #[cfg(debug_assertions)]
    pub fn get_branch_out_pc(&self) -> u32 {
        self.branch_out_pc
    }

    #[cfg(not(debug_assertions))]
    pub fn get_branch_out_pc(&self) -> u32 {
        panic!()
    }

    pub const fn get_pre_cycle_count_sum_offset() -> usize {
        mem::offset_of!(JitRuntimeData, pre_cycle_count_sum)
    }

    pub const fn get_accumulated_cycles_offset() -> usize {
        mem::offset_of!(JitRuntimeData, accumulated_cycles)
    }

    pub const fn get_host_sp_offset() -> usize {
        mem::offset_of!(JitRuntimeData, host_sp)
    }

    pub const fn get_data_packed_offset() -> usize {
        mem::offset_of!(JitRuntimeData, data_packed)
    }

    pub const fn get_return_stack_offset() -> usize {
        mem::offset_of!(JitRuntimeData, return_stack)
    }

    pub fn is_idle_loop(&self) -> bool {
        self.data_packed.idle_loop()
    }

    pub fn set_idle_loop(&mut self, idle_loop: bool) {
        self.data_packed.set_idle_loop(idle_loop);
    }

    pub fn is_in_interrupt(&self) -> bool {
        self.data_packed.in_interrupt()
    }

    pub fn set_in_interrupt(&mut self, in_interrupt: bool) {
        self.data_packed.set_in_interrupt(in_interrupt);
    }

    pub fn get_return_stack_ptr(&self) -> usize {
        self.return_stack_ptr as usize
    }

    pub fn push_return_stack(&mut self, value: u32) {
        let mut return_stack_ptr = self.get_return_stack_ptr();
        unsafe { *self.return_stack.get_unchecked_mut(return_stack_ptr) = value };
        return_stack_ptr += 1;
        return_stack_ptr &= RETURN_STACK_SIZE - 1;
        unsafe { *self.return_stack.get_unchecked_mut(return_stack_ptr) = u32::MAX };
        self.return_stack_ptr = return_stack_ptr as u8;
    }

    pub fn pop_return_stack(&mut self) -> u32 {
        let mut return_stack_ptr = self.get_return_stack_ptr();
        return_stack_ptr = return_stack_ptr.wrapping_sub(1);
        return_stack_ptr &= RETURN_STACK_SIZE - 1;
        self.return_stack_ptr = return_stack_ptr as u8;
        unsafe { *self.return_stack.get_unchecked(return_stack_ptr) }
    }

    pub fn get_sp_depth_size(&self) -> usize {
        let mut sp: usize;
        unsafe { asm!("mov {}, sp", out(reg) sp, options(pure, nomem, preserves_flags)) };
        self.host_sp - sp
    }

    pub fn clear_return_stack_ptr(&mut self) {
        self.return_stack_ptr = 0;
        self.return_stack[RETURN_STACK_SIZE - 1] = u32::MAX;
    }
}

pub fn align_guest_pc(guest_pc: u32) -> u32 {
    let thumb = guest_pc & 1 == 1;
    let guest_pc_mask = !(1 | ((!thumb as u32) << 1));
    guest_pc & guest_pc_mask
}

pub extern "C" fn hle_bios_uninterrupt() {
    let asm = unsafe { get_jit_asm_ptr().as_mut_unchecked() };
    let current_pc = thread_regs().pc;
    asm.runtime_data.accumulated_cycles += 3;
    bios::uninterrupt(asm.emu);
    if unlikely(asm.emu.cpu_is_halted()) {
        if IS_DEBUG {
            asm.runtime_data.set_branch_out_pc(current_pc);
        }
        unsafe { exit_guest_context!(asm) };
    } else {
        asm.runtime_data.clear_return_stack_ptr();
        unsafe { call_jit_fun(asm, thread_regs().pc) };
    }
}

#[cfg(target_arch = "arm")]
const_assert_eq!(size_of::<Vec<GuestInstOffset>>(), 12);
#[cfg(target_arch = "arm")]
const_assert_eq!(size_of::<GuestInstOffset>(), 40);

#[cfg(target_arch = "arm")]
const fn jit_emu_offset() -> usize {
    mem::offset_of!(JitAsm, emu)
}

#[cfg(target_arch = "arm")]
const fn jit_mem_mmap_offset() -> usize {
    mem::offset_of!(Emu, jit.mem.ptr)
}

#[cfg(target_arch = "arm")]
const fn jit_guest_inst_offset() -> usize {
    mem::offset_of!(Emu, jit.guest_inst_offsets)
}

#[cfg(target_arch = "arm")]
const fn pre_cycle_count_sum_offset() -> usize {
    mem::offset_of!(JitAsm, runtime_data.pre_cycle_count_sum)
}

#[cfg(target_arch = "arm")]
#[unsafe(naked)]
unsafe extern "C" fn jump_to_other_guest_pc(_: u32, _: u32) {
    #[rustfmt::skip]
    naked_asm!(
        "mov r1, {jit_asm_ptr}",
        "lsrs r0, r0, 1", // r0 = diff >> 1
        "subs r0, r0, 1", // r0 = r0 - 1
        "ldr r2, [r1, {emu_offset}]", // r2 = asm.emu
        "mov r4, {jit_mem_mmap_offset}",
        "ldr r3, [r2, r4]", // r3 = r2.jit.mem.ptr
        "add r0, r0, r0, lsl #2",
        "sub r3, lr, r3", // r3 = lr - r3
        "mov r5, {jit_guest_inst_offset}",
        "lsrs r3, {page_shift}", // r3 = r3 >> PAGE_SHIFT
        "ldr r4, [r2, r5]", // r4 = &r2.jit.guest_inst_offsets
        "add r3, r3, r3, lsl #1",
        "add r4, r4, r3, lsl #2",
        "mov r3, {guest_regs_offset}",
        "ldr r5, [r4, 4]", // r5 = r4[r3], offset by 4, first 4 bytes of vec is capacity
        "add r5, r5, r0, lsl #3",
        "ldmia r5, {{r2, r4, r5, r6, r7, r8, r9, r10, r11}}",
        "ldr r0, [r3, {cpsr_offset}]",
        "msr cpsr_f, r0",
        "ldr r4, [r4]",
        "uxth r0, r2",
        "lsr r2, r2, 16",
        "strh r2, [r1, {pre_cycle_count_sum_offset}]",
        "ldr r5, [r5]",
        "ldr r6, [r6]",
        "ldr r7, [r7]",
        "ldr r8, [r8]",
        "ldr r9, [r9]",
        "ldr r10, [r10]",
        "ldr r11, [r11]",
        "bx lr",
        jit_asm_ptr = const jit_asm_addr(),
        emu_offset = const jit_emu_offset(),
        jit_mem_mmap_offset = const jit_mem_mmap_offset(),
        page_shift = const PAGE_SHIFT,
        jit_guest_inst_offset = const jit_guest_inst_offset(),
        pre_cycle_count_sum_offset = const pre_cycle_count_sum_offset(),
        guest_regs_offset = const guest_regs_addr(),
        cpsr_offset = const Reg::CPSR as usize * 4,
    );
}




#[cold]
pub extern "C" fn emit_code_block(guest_pc: u32) {
    unsafe { (*(&raw mut crate::logging::DISPATCH_RING)).push(guest_pc, 2, 0) };
    let thumb = (guest_pc & 1) == 1;
    let asm = unsafe { get_jit_asm_ptr().as_mut_unchecked() };
    emit_code_block_internal(asm, guest_pc & !1, thumb);
}

/// The block driver: hotness counter (cold blocks go to the interpreter), decode, then
/// compile+insert.
fn emit_code_block_internal(asm: &mut JitAsm, guest_pc: u32, thumb: bool) {
    {
        let count_ptr = asm.emu.jit.jit_memory_map.get_exec_count(guest_pc);
        debug_assert!(
            !count_ptr.is_null(),
            "dispatch to unmapped guest pc {guest_pc:x} thumb {thumb}, last interpreted {:x?}",
            unsafe { crate::jit::interpreter::LAST_INTERPRETED }
        );
        let count = unsafe { (*count_ptr).saturating_add(1) };
        unsafe { *count_ptr = count };
        if count <= crate::jit::interpreter::INTERP_THRESHOLD {
            crate::jit::interpreter::interpret_block(asm, guest_pc, thumb);
            return;
        }
    }

    // Blocks overlapping a proven self-modifying page stay interpreted: compiling them
    // just re-enters the invalidate/recompile thrash the next time the code patches
    // itself (the m4a mixer does this every pass). The block-start check runs BEFORE
    // decoding: sticky blocks dispatch here every execution, and decoding just to
    // throw the result away was the single hottest profile entry.
    if asm.emu.jit.is_sticky_smc(guest_pc) {
        crate::jit::interpreter::interpret_block(asm, guest_pc, thumb);
        return;
    }

    asm.jit_buf.clear_all();
    let guest_pc_end = JitAsm::fill_jit_insts_buf(&mut asm.jit_buf.insts, &mut asm.jit_buf.insts_cycle_counts, asm.emu, guest_pc, thumb, false);

    // Full-span recheck: the block may extend into a sticky page even when it starts
    // outside one
    if asm.emu.jit.any_sticky_smc(guest_pc, guest_pc_end + if thumb { 2 } else { 4 }) {
        crate::jit::interpreter::interpret_block(asm, guest_pc, thumb);
        return;
    }

    // DIAGNOSTIC: force-interpret an address window (ADVANCEDSLOP_INTERP_RANGE=start-end hex)
    #[cfg(debug_assertions)]
    {
        static RANGE: std::sync::OnceLock<Option<(u32, u32)>> = std::sync::OnceLock::new();
        let range = RANGE.get_or_init(|| {
            let v = std::env::var("ADVANCEDSLOP_INTERP_RANGE").ok()?;
            let (a, b) = v.split_once('-')?;
            Some((u32::from_str_radix(a, 16).ok()?, u32::from_str_radix(b, 16).ok()?))
        });
        if let Some((start, end)) = range {
            if guest_pc < *end && guest_pc_end > *start {
                crate::jit::interpreter::interpret_block(asm, guest_pc, thumb);
                return;
            }
        }
    }
    debug_assert!(
        !asm.jit_buf.insts.is_empty(),
        "compiling empty block at {guest_pc:x} thumb {thumb}: execution reached undefined code",
    );

    debug_println!("{thumb} emit code block {guest_pc:x} - {guest_pc_end:x}");
    asm.analyzer.analyze(guest_pc, &asm.jit_buf.insts, thumb);
    asm.jit_buf.guest_pc_start = guest_pc;
    asm.jit_buf.debug_info.resize(asm.analyzer.basic_blocks.len(), asm.jit_buf.insts.len());

    let (jit_entry, flushed) = {
        let pc_step = if thumb { 2 } else { 4 };
        let mut block_asm = BlockAsm::new(thumb, false);
        block_asm.prologue(asm.analyzer.basic_blocks.len());

        if BRANCH_LOG {
            block_asm.emit_enter_block_hook(debug_enter_block as *const ());
        }

        block_asm.emit_entry_pc_dispatch(guest_pc | (thumb as u32), jump_to_other_guest_pc as *const ());

        asm.emit(&mut block_asm, thumb);

        block_asm.finalize();

        let (insert_entry, flushed) = asm.emu.jit_insert_block(block_asm, &asm.jit_buf.debug_info, guest_pc, guest_pc_end + pc_step, thumb);
        let jit_entry: extern "C" fn(u32) = unsafe { mem::transmute(insert_entry) };
        asm.runtime_data.pre_cycle_count_sum = 0;
        (jit_entry, flushed)
    };
    jit_entry(guest_pc | (thumb as u32));
    // The allocation flushed jit memory: host frames above may point into freed blocks —
    // unwind the whole guest context.
    if flushed {
        unsafe { exit_guest_context!(asm) };
    }
}

#[cfg(target_arch = "arm")]
#[unsafe(naked)]
pub unsafe extern "C" fn call_jit_entry(_: u32, _entry: *const fn(), _host_sp: *mut usize) {
    #[rustfmt::skip]
    naked_asm!(
        "push {{r4-r12,lr}}",
        "str sp, [r2]",
        "blx r1",
        "pop {{r4-r12,pc}}",
    );
}

fn execute_internal(guest_pc: u32) -> u16 {
    let asm = unsafe { get_jit_asm_ptr().as_mut_unchecked() };

    unsafe { (*(&raw mut crate::logging::DISPATCH_RING)).push(guest_pc, 1, 0) };
    let thumb = (guest_pc & 1) == 1;
    let guest_pc = align_guest_pc(guest_pc);
    debug_println!("Execute {:x} thumb {}", guest_pc | (thumb as u32), thumb);

    let jit_entry = {
        asm.emu.thread_set_thumb(thumb);

        let jit_entry = asm.emu.jit.get_jit_start_addr(guest_pc);

        debug_println!("Enter jit addr");

        if IS_DEBUG {
            asm.runtime_data.set_branch_out_pc(u32::MAX);
        }
        asm.runtime_data.pre_cycle_count_sum = 0;
        asm.runtime_data.accumulated_cycles = 0;
        asm.runtime_data.clear_return_stack_ptr();
        asm.runtime_data.data_packed = JitRuntimeDataPacked::from(0);
        asm.emu.breakout_imm = false;
        jit_entry
    };

    unsafe { call_jit_entry(guest_pc | (thumb as u32), jit_entry as _, &mut asm.runtime_data.host_sp) };

    if IS_DEBUG {
        unsafe { (*(&raw mut crate::logging::SLICE_RING)).push(guest_pc | (thumb as u32), asm.runtime_data.get_branch_out_pc(), thread_regs().pc) };
    }

    if IS_DEBUG {
        debug_assert_ne!(
            asm.runtime_data.get_branch_out_pc(),
            u32::MAX,
            "idle loop {} return stack ptr {}",
            asm.runtime_data.is_idle_loop(),
            asm.runtime_data.get_return_stack_ptr(),
        );
    }

    if BRANCH_LOG {
        branch_println!(
            "reading opcode of breakout at {:x} executed cycles {}",
            asm.runtime_data.get_branch_out_pc(),
            asm.runtime_data.accumulated_cycles,
        );
        if asm.runtime_data.is_idle_loop() {
            branch_println!("idle loop");
        }
        let inst_info = if asm.emu.thread_is_thumb() {
            let opcode = asm.emu.mem_read::<_>(asm.runtime_data.get_branch_out_pc());
            let (op, func) = lookup_thumb_opcode(opcode);
            InstInfo::from(func(opcode, *op))
        } else {
            let opcode = asm.emu.mem_read::<_>(asm.runtime_data.get_branch_out_pc());
            let (op, func) = lookup_opcode(opcode);
            func(opcode, *op)
        };
        debug_inst_info(asm.emu, asm.runtime_data.get_branch_out_pc(), &format!("breakout\n\t{inst_info:?}"));
    }

    asm.runtime_data.accumulated_cycles
}

#[repr(C)]
pub struct JitAsm<'a> {
    pub runtime_data: JitRuntimeData,
    pub emu: &'a mut Emu,
    pub jit_buf: JitBuf,
    pub analyzer: AsmAnalyzer,
    pub os_irq_handler_addr: u32,
}

impl<'a> JitAsm<'a> {
    #[inline(never)]
    pub fn new(emu: &'a mut Emu) -> Self {
        JitAsm {
            emu,
            jit_buf: JitBuf::new(),
            runtime_data: JitRuntimeData::new(),
            analyzer: AsmAnalyzer::default(),
            os_irq_handler_addr: 0,
        }
    }

    pub fn execute(&mut self) -> u16 {
        let entry = thread_regs().pc;
        execute_internal(entry)
    }

    pub fn fill_jit_insts_buf(insts: &mut Vec<InstInfo>, cycle_counts: &mut Vec<u16>, emu: &mut Emu, guest_pc: u32, thumb: bool, until_bx: bool) -> u32 {
        let mut pc_offset = 0;
        let get_inst_info = if thumb {
            |emu: &mut Emu, pc| {
                let opcode = emu.mem_read::<u16>(pc);
                let (op, func) = lookup_thumb_opcode(opcode);
                InstInfo::from(func(opcode, *op))
            }
        } else {
            |emu: &mut Emu, pc| {
                let opcode = emu.mem_read::<u32>(pc);
                let (op, func) = lookup_opcode(opcode);
                func(opcode, *op)
            }
        };

        let pc_step = if thumb { 2 } else { 4 };

        let mut min_imm_guest_addr = u32::MAX;
        loop {
            let pc = guest_pc + pc_offset;
            let inst_info = get_inst_info(emu, pc);

            if inst_info.op == Op::UnkArm || inst_info.op == Op::UnkThumb || inst_info.cond == Cond::NV {
                break;
            }

            if let Some(last) = cycle_counts.last() {
                debug_assert!(u16::MAX - last >= inst_info.cycle as u16, "{guest_pc:x} {inst_info:?}");
                cycle_counts.push(last + inst_info.cycle as u16);
            } else {
                cycle_counts.push(inst_info.cycle as u16);
                debug_assert!(cycle_counts.len() <= u16::MAX as usize, "{guest_pc:x} {inst_info:?}");
            }

            if let Some(imm_addr) = inst_info.imm_transfer_addr(pc) {
                if imm_addr > pc && imm_addr < min_imm_guest_addr {
                    min_imm_guest_addr = imm_addr;
                }
            }

            let is_uncond_branch = inst_info.is_uncond_branch();
            let cond = inst_info.cond;
            let is_unreturnable_branch = !inst_info.out_regs.is_reserved(Reg::LR) && is_uncond_branch;
            // An unconditional branch skipping exactly one instruction (imm 0 = pipeline-only
            // offset) is de-conditionalized code — some sdk builds emit `blt 1f; b 2f; 1: op; 2:`
            // instead of a conditional op. Keep building: the analyzer resolves it as a local
            // branch, the block doesn't shatter into per-instruction pieces, and the nitro-sdk
            // pattern substitutions can still recognize such function bodies.
            let is_skip_one_branch = is_unreturnable_branch && inst_info.op.is_labelled_branch() && inst_info.operands()[0].as_imm() == Some(0);
            let op = inst_info.op;
            insts.push(inst_info);

            if (matches!(op, Op::Bx | Op::BxRegT) && cond == Cond::AL)
                || (insts.len() >= 500 && op != Op::BlSetupT)
                || (!until_bx && is_unreturnable_branch && !is_skip_one_branch && min_imm_guest_addr == u32::MAX)
            {
                break;
            }

            if guest_pc + pc_offset >= min_imm_guest_addr {
                break;
            }
            pc_offset += pc_step;
        }

        guest_pc + pc_offset
    }
}

fn debug_inst_info(emu: &Emu, pc: u32, append: &str) {
    let mut output = "Executed ".to_owned();

    for reg in reg_reserve!(Reg::SP, Reg::LR, Reg::PC, Reg::CPSR, Reg::SPSR) + RegReserve::gp() {
        let value = if reg != Reg::PC { *emu.thread_get_reg(reg) } else { pc };
        output += &format!("{reg:?}: {value:x}, ");
    }

    debug_println!("{output}{append}");
}

pub unsafe extern "C" fn debug_after_exec_op(pc: u32, opcode: u32) {
    let asm = get_jit_asm_ptr();
    crate::debug_inst_log::log((*asm).emu, pc, opcode);
}

unsafe extern "C" fn debug_enter_block(pc: u32) {
    branch_println!("execute {pc:x}");
    let asm = get_jit_asm_ptr();
    if BRANCH_LOG {
        debug_inst_info((*asm).emu, pc, "enter block");
    }
}
