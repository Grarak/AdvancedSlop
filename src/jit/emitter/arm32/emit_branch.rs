
use crate::core::MAX_BRANCH_LOOP_CYCLE_COUNT;
use crate::jit::assembler::block_asm::{BlockAsm, CPSR_TMP_REG};
use crate::jit::inst_branch_handler::{branch_lr, branch_reg, exe_scheduler_external, pre_branch};
use crate::jit::jit_asm::{JitAsm, JitForwardBranch, JitLinkSchedStub, JitRunSchedulerLabel, JitRuntimeData};
use crate::jit::reg::{reg_reserve, Reg};
use crate::jit::{inst_branch_handler, Cond};
use crate::logging::branch_println;
use crate::{BRANCH_LOG, IS_DEBUG};
use std::ptr;
use vixl::{
    BranchHint_kFar, BranchHint_kNear, FlagsUpdate_DontCare, Label, MasmB2, MasmB3, MasmBic5, MasmBlx1, MasmBx1, MasmCmp2, MasmLdr2, MasmLdrb2, MasmMov2, MasmMov4, MasmOrr5, MasmStr2, MasmStrb2,
    MasmStrh2,
};

extern "C" fn debug_branch_label(current_pc: u32, target_pc: u32) {
    branch_println!("branch label from {current_pc:x} to {target_pc:x}")
}

extern "C" fn debug_branch_reg(current_pc: u32, target_pc: u32) {
    branch_println!("branch reg from {current_pc:x} to {target_pc:x}")
}

extern "C" fn debug_idle_loop(current_pc: u32, target_pc: u32) {
    branch_println!("detected idle loop {current_pc:x} to {target_pc:x}")
}

extern "C" fn debug_branch_imm(current_pc: u32, target_pc: u32) {
    branch_println!("branch imm from {current_pc:x} to {target_pc:x}");
}

impl JitAsm<'_> {
    fn emit_set_cpsr_thumb_bit_imm(&mut self, thumb: bool, block_asm: &mut BlockAsm) {
        block_asm.load_guest_reg(Reg::R0, Reg::CPSR);
        if thumb {
            block_asm.orr5(FlagsUpdate_DontCare, Cond::AL, Reg::R0, Reg::R0, &(1 << 5).into());
        } else {
            block_asm.bic5(FlagsUpdate_DontCare, Cond::AL, Reg::R0, Reg::R0, &(1 << 5).into());
        }
        block_asm.store_guest_reg(Reg::R0, Reg::CPSR);
    }

    fn emit_call_jit_addr_imm(&mut self, target_pc: u32, has_return: bool, set_thumb_bit: bool, block_asm: &mut BlockAsm) {
        if set_thumb_bit {
            self.emit_set_cpsr_thumb_bit_imm(target_pc & 1 == 1, block_asm);
        }

        let jit_entry_addr = self.emu.jit.jit_memory_map.get_jit_entry(target_pc);
        block_asm.ldr2(Reg::R0, target_pc);
        block_asm.ldr2(Reg::R1, jit_entry_addr as u32);
        block_asm.ldr2(Reg::R3, &Reg::R1.into());
        if has_return {
            block_asm.blx1(Reg::R3);
        } else {
            block_asm.bx1(Reg::R3);
        }
    }

    pub fn emit_call_branch_imm(&mut self, inst_index: usize, target_pc: u32, has_return: bool, set_thumb_bit: bool, block_asm: &mut BlockAsm) {
        if !has_return {
            // External immediate B/cond-B: inline pre_branch::<false, HLE> — flush_cycles + the
            // per-cpu quantum check + pre_cycle_count_sum := 0 — instead of the C call, and move the
            // quantum's scheduler call to an out-of-line stub at the END of the block so it stays off
            // the hot path (emit_link_sched_stub). Then slot-dispatch. Guest behaviour is identical to
            // routing through the pre_branch C call; measured faster on the Vita.
            block_asm.ldr2(Reg::R0, ptr::addr_of_mut!(self.runtime_data) as u32);
            // flush_cycles: accumulated += total + 2 - pre_cycle_count_sum (R2 = accumulated).
            self.emit_count_cycles(self.jit_buf.insts_cycle_counts[inst_index], block_asm);
            // pre_cycle_count_sum := 0 (flush_cycles clears it; the fresh target entry re-charges).
            block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R1, &0.into());
            block_asm.strh2(Reg::R1, &(Reg::R0, JitRuntimeData::get_pre_cycle_count_sum_offset() as i32).into());

            // check_scheduler: external edges gate on max_loop_cycle_count (128), NOT
            // the local-branch 128 — using 128 here would shift the scheduler quantum.
            let mut exceed_label = Label::new();
            let mut continue_label = Label::new();
            block_asm.cmp2(Reg::R2, &crate::core::MAX_LOOP_CYCLE_COUNT.into());
            block_asm.b3(Cond::HS, &mut exceed_label, BranchHint_kFar);
            block_asm.bind(&mut continue_label);

            if BRANCH_LOG {
                let pc = block_asm.current_pc;
                block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R0, &pc.into());
                block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R1, &target_pc.into());
                block_asm.call(debug_branch_imm as *const ());
            }

            // set_thumb_bit is always false for these edges (BLX-imm is has_return), so the dispatch
            // never needs the guest-regs pointer in R3 — no restore_guest_regs_ptr here.
            block_asm.restore_stack();
            self.emit_call_jit_addr_imm(target_pc, false, set_thumb_bit, block_asm);

            self.jit_buf.link_sched_stubs.push(JitLinkSchedStub::new(exceed_label, continue_label, block_asm.current_pc));
            return;
        }

        // BL/BLX-imm: pre_branch also does check_stack_depth + push_return_stack, so a call edge keeps
        // the C call rather than inlining.
        block_asm.ldr2(Reg::R0, self as *mut _ as u32);
        block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R1, &self.jit_buf.insts_cycle_counts[inst_index].into());
        let lr_reg = block_asm.get_guest_map(Reg::LR);
        block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R2, &lr_reg.into());
        if IS_DEBUG {
            let pc = block_asm.current_pc;
            block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R3, &pc.into());
        }
        block_asm.call(pre_branch::<true> as *const ());

        if BRANCH_LOG {
            let pc = block_asm.current_pc;
            block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R0, &pc.into());
            block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R1, &target_pc.into());
            block_asm.call(debug_branch_imm as *const ());
        }

        block_asm.restore_guest_regs_ptr();
        self.emit_call_jit_addr_imm(target_pc, true, set_thumb_bit, block_asm);
    }

    pub fn emit_call_branch_reg(&mut self, inst_index: usize, target_pc_reg: Reg, has_return: bool, block_asm: &mut BlockAsm) {
        block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R0, &self.jit_buf.insts_cycle_counts[inst_index].into());
        block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R1, &target_pc_reg.into());
        if has_return {
            let lr_reg = block_asm.get_guest_map(Reg::LR);
            block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R2, &lr_reg.into());
        }
        if IS_DEBUG {
            let pc = block_asm.current_pc;
            block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R3, &pc.into());
        }

        if has_return {
            block_asm.call(branch_reg::<true> as *const ());
        } else {
            block_asm.restore_stack();
            block_asm.ldr2(Reg::R12, branch_reg::<false> as *const () as u32);
            block_asm.bx1(Reg::R12);
        }
    }

    pub fn emit_link_sched_stub(&mut self, index: usize, block_asm: &mut BlockAsm) {
        let stub = &mut self.jit_buf.link_sched_stubs[index];
        block_asm.bind(&mut stub.bind_label);

        if IS_DEBUG {
            let pc = stub.current_pc;
            block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R0, &pc.into());
        }
        // exe_scheduler = the same call check_scheduler makes: it exits the guest context
        // and never returns (the branch-back below is then unreachable, which is fine).
        block_asm.call(exe_scheduler_external as *const ());
        block_asm.b2(&mut stub.continue_label, BranchHint_kFar);
    }

    pub fn emit_branch_external_label(&mut self, inst_index: usize, basic_block_index: usize, target_pc: u32, has_return: bool, set_thumb_bit: bool, block_asm: &mut BlockAsm) {
        self.emit_call_branch_imm(inst_index, target_pc, has_return, set_thumb_bit, block_asm);
        // A B/cond-B edge is a tail jump — nothing follows. A BL/BLX-imm edge returns here, so lay
        // down the post-return recovery (re-charge pre_cycle_count_sum, reload guest regs).
        if !has_return {
            return;
        }
        block_asm.ldr2(Reg::R1, ptr::addr_of_mut!(self.runtime_data) as u32);

        if inst_index == self.jit_buf.insts.len() - 1 {
            block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R0, &0.into());
            block_asm.strh2(Reg::R0, &(Reg::R1, JitRuntimeData::get_pre_cycle_count_sum_offset() as i32).into());

            block_asm.restore_guest_regs_ptr();
            block_asm.restore_stack();
            let lr = if block_asm.thumb { block_asm.current_pc + 3 } else { block_asm.current_pc + 4 };
            self.emit_call_jit_addr_imm(lr, false, false, block_asm);
            return;
        }

        block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R0, &self.jit_buf.insts_cycle_counts[inst_index].into());
        block_asm.strh2(Reg::R0, &(Reg::R1, JitRuntimeData::get_pre_cycle_count_sum_offset() as i32).into());

        let mut next_live_regs = self.analyzer.get_next_live_regs(basic_block_index, inst_index);
        if self.jit_buf.insts[inst_index].cond != Cond::AL {
            next_live_regs += Reg::CPSR;
        }
        block_asm.restore_tmp_regs(next_live_regs);

        block_asm.reload_active_guest_regs_all();
    }

    pub fn emit_branch_reg(&mut self, inst_index: usize, basic_block_index: usize, target_pc_reg: Reg, has_return: bool, block_asm: &mut BlockAsm) {
        self.emit_call_branch_reg(inst_index, target_pc_reg, has_return, block_asm);

        if has_return {
            if inst_index == self.jit_buf.insts.len() - 1 {
                block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R0, &0.into());
                block_asm.ldr2(Reg::R1, ptr::addr_of_mut!(self.runtime_data) as u32);
                block_asm.strh2(Reg::R0, &(Reg::R1, JitRuntimeData::get_pre_cycle_count_sum_offset() as i32).into());

                block_asm.restore_guest_regs_ptr();
                block_asm.restore_stack();
                let lr = if block_asm.thumb { block_asm.current_pc + 3 } else { block_asm.current_pc + 4 };
                self.emit_call_jit_addr_imm(lr, false, false, block_asm);
                return;
            }

            let mut next_live_regs = self.analyzer.get_next_live_regs(basic_block_index, inst_index);
            if self.jit_buf.insts[inst_index].cond != Cond::AL {
                next_live_regs += Reg::CPSR;
            }
            block_asm.restore_tmp_regs(next_live_regs);

            block_asm.reload_active_guest_regs_all();
        }
    }

    pub fn emit_branch_return_stack(&mut self, inst_index: usize, target_pc_reg: Reg, block_asm: &mut BlockAsm) {
        if block_asm.is_fs_clear_overlay {
            self.emit_branch_out_metadata(inst_index, true, block_asm);
            block_asm.exit_guest_context(&mut self.runtime_data.host_sp);
            return;
        }

        block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R0, &self.jit_buf.insts_cycle_counts[inst_index].into());
        block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R1, &target_pc_reg.into());
        if IS_DEBUG {
            let pc = block_asm.current_pc;
            block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R2, &pc.into());
        }

        block_asm.restore_stack();
        block_asm.ldr2(Reg::R12, branch_lr as *const () as u32);
        block_asm.bx1(Reg::R12);
    }

    pub fn emit_branch_label(&mut self, inst_index: usize, basic_block_index: usize, target_pc: u32, skip_label: &mut Label, block_asm: &mut BlockAsm) {
        let thumb = target_pc & 1 == 1;
        let aligned_target_pc = target_pc & !1;
        let pc_shift = if thumb { 1 } else { 2 };
        let cond = self.jit_buf.insts[inst_index].cond;
        let metadata = self.analyzer.insts_metadata[inst_index];

        if metadata.idle_loop() || metadata.external_branch() {
            if cond != Cond::AL {
                block_asm.b3(!cond, skip_label, BranchHint_kNear);
            }

            block_asm.ldr2(Reg::R1, target_pc);
            block_asm.store_guest_reg(Reg::R1, Reg::PC);
            block_asm.dirty_guest_regs -= Reg::PC;
            block_asm.save_dirty_guest_regs_additional(true, cond == Cond::AL, reg_reserve!());
        }

        if metadata.idle_loop() {
            if BRANCH_LOG {
                let pc = block_asm.current_pc;
                block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R0, &pc.into());
                block_asm.call(debug_idle_loop as *const ());
            }

            self.emit_branch_out_metadata(inst_index, true, block_asm);
            let mem_operand = (Reg::R0, JitRuntimeData::get_data_packed_offset() as i32).into();
            block_asm.ldrb2(Reg::R1, &mem_operand);
            block_asm.orr5(FlagsUpdate_DontCare, Cond::AL, Reg::R1, Reg::R1, &(crate::jit::jit_asm::IDLE_LOOP_FLAG_MASK as u32).into());
            block_asm.strb2(Reg::R1, &mem_operand);
            block_asm.exit_guest_context(&mut self.runtime_data.host_sp);
        } else if metadata.external_branch() {
            self.emit_branch_external_label(inst_index, basic_block_index, target_pc, false, false, block_asm);
        } else {
            if target_pc > block_asm.current_pc {
                let mut label = Label::new();
                block_asm.b3(cond, &mut label, BranchHint_kFar);
                self.jit_buf
                    .forward_branches
                    .push(JitForwardBranch::new(inst_index, target_pc, block_asm.dirty_guest_regs, block_asm.get_guest_regs_mapping(), label));
                if cond == Cond::AL {
                    block_asm.dirty_guest_regs.clear();
                }
                return;
            }

            if cond != Cond::AL {
                block_asm.b3(!cond, skip_label, BranchHint_kNear);
            }

            block_asm.save_dirty_guest_cpsr(false);

            let jump_to_index = (inst_index as isize + ((aligned_target_pc as isize - block_asm.current_pc as isize) >> pc_shift)) as usize;
            let target_pre_cycle_count_sum = self.jit_buf.insts_cycle_counts[jump_to_index] - self.jit_buf.insts[jump_to_index].cycle as u16;

            block_asm.ldr2(Reg::R0, ptr::addr_of_mut!(self.runtime_data) as u32);
            self.emit_count_cycles(self.jit_buf.insts_cycle_counts[inst_index], block_asm);

            let mut cycles_exceed_label = Label::new();
            let mut continue_label = Label::new();

            block_asm.cmp2(Reg::R2, &MAX_BRANCH_LOOP_CYCLE_COUNT.into());
            block_asm.b3(Cond::HS, &mut cycles_exceed_label, BranchHint_kFar);

            block_asm.bind(&mut continue_label);

            block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R1, &target_pre_cycle_count_sum.into());
            block_asm.strh2(Reg::R1, &(Reg::R0, JitRuntimeData::get_pre_cycle_count_sum_offset() as i32).into());

            if BRANCH_LOG {
                let pc = block_asm.current_pc;
                block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R0, &pc.into());
                block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R1, &target_pc.into());
                block_asm.call(debug_branch_label as *const ());
                block_asm.restore_guest_regs_ptr();
            }

            let basic_block_index = self.analyzer.get_basic_block_from_inst(jump_to_index);
            let basic_block = &self.analyzer.basic_blocks[basic_block_index];
            let basic_block_input_regs = basic_block.get_inputs();
            block_asm.relocate_for_basic_block(FlagsUpdate_DontCare, basic_block.output_regs, basic_block_index);
            if basic_block_input_regs.is_reserved(Reg::CPSR) {
                block_asm.load_guest_cpsr_reg(CPSR_TMP_REG);
            }
            block_asm.b_basic_block(basic_block_index);

            self.jit_buf.run_scheduler_labels.push(JitRunSchedulerLabel::new(
                inst_index,
                target_pc,
                block_asm.dirty_guest_regs,
                block_asm.get_guest_regs_mapping(),
                cycles_exceed_label,
                continue_label,
                None,
            ));

            if cond == Cond::AL {
                block_asm.dirty_guest_regs.clear();
            }
        }
    }

    pub fn emit_forward_branch(&mut self, forward_branch_index: usize, block_asm: &mut BlockAsm) {
        let forward_branch = &mut self.jit_buf.forward_branches[forward_branch_index];
        block_asm.dirty_guest_regs = forward_branch.dirty_guest_regs;
        block_asm.current_pc = self.analyzer.get_pc_from_inst(forward_branch.inst_index);
        block_asm.set_guest_regs_mapping(forward_branch.guest_regs_mapping);

        block_asm.bind(&mut forward_branch.bind_label);

        let inst_index = forward_branch.inst_index;
        let thumb = forward_branch.target_pc & 1 == 1;
        let aligned_target_pc = forward_branch.target_pc & !1;
        let pc_shift = if thumb { 1 } else { 2 };

        block_asm.save_dirty_guest_cpsr(false);

        let jump_to_index = (inst_index as isize + ((aligned_target_pc as isize - block_asm.current_pc as isize) >> pc_shift)) as usize;
        let target_pre_cycle_count_sum = self.jit_buf.insts_cycle_counts[jump_to_index] - self.jit_buf.insts[jump_to_index].cycle as u16;

        block_asm.ldr2(Reg::R0, ptr::addr_of_mut!(self.runtime_data) as u32);
        self.emit_count_cycles(self.jit_buf.insts_cycle_counts[inst_index], block_asm);
        let forward_branch = &mut self.jit_buf.forward_branches[forward_branch_index];

        let mut cycles_exceed_label = Label::new();
        let mut continue_label = Label::new();
        let mut exit_label = Label::new();

        block_asm.cmp2(Reg::R2, &MAX_BRANCH_LOOP_CYCLE_COUNT.into());
        block_asm.b3(Cond::HS, &mut cycles_exceed_label, BranchHint_kFar);

        block_asm.bind(&mut continue_label);

        let target_pc_jit_entry = self.emu.jit.jit_memory_map.get_jit_entry(aligned_target_pc);

        // Check if current block is still valid
        block_asm.ldr2(Reg::R2, target_pc_jit_entry as u32);
        // At compile time we don't know where the block will end up
        // Let jit memory insert the address after allocation
        block_asm.insert_jit_entry(Reg::R1);
        block_asm.ldr2(Reg::R2, &Reg::R2.into());
        block_asm.cmp2(Reg::R1, &Reg::R2.into());
        block_asm.b3(Cond::NE, &mut exit_label, BranchHint_kFar);

        block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R1, &target_pre_cycle_count_sum.into());
        block_asm.strh2(Reg::R1, &(Reg::R0, JitRuntimeData::get_pre_cycle_count_sum_offset() as i32).into());

        if BRANCH_LOG {
            let pc = block_asm.current_pc;
            block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R0, &pc.into());
            block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R1, &forward_branch.target_pc.into());
            block_asm.call(debug_branch_label as *const ());
            block_asm.restore_guest_regs_ptr();
        }

        let basic_block_index = self.analyzer.get_basic_block_from_inst(jump_to_index);
        let basic_block = &self.analyzer.basic_blocks[basic_block_index];
        let basic_block_input_regs = basic_block.get_inputs();
        block_asm.relocate_for_basic_block(FlagsUpdate_DontCare, basic_block.output_regs, basic_block_index);
        if basic_block_input_regs.is_reserved(Reg::CPSR) {
            block_asm.load_guest_cpsr_reg(CPSR_TMP_REG);
        }
        block_asm.b_basic_block(basic_block_index);

        self.jit_buf.run_scheduler_labels.push(JitRunSchedulerLabel::new(
            forward_branch.inst_index,
            forward_branch.target_pc,
            block_asm.dirty_guest_regs,
            block_asm.get_guest_regs_mapping(),
            cycles_exceed_label,
            continue_label,
            Some(exit_label),
        ));
    }

    pub fn emit_run_scheduler(&mut self, run_scheduler_label_index: usize, block_asm: &mut BlockAsm) {
        let jit_asm_ptr = self as *mut _ as u32;
        let run_scheduler_label = &mut self.jit_buf.run_scheduler_labels[run_scheduler_label_index];
        block_asm.bind(&mut run_scheduler_label.bind_label);

        block_asm.dirty_guest_regs = run_scheduler_label.dirty_guest_regs;
        block_asm.current_pc = self.analyzer.get_pc_from_inst(run_scheduler_label.inst_index);
        block_asm.set_guest_regs_mapping(run_scheduler_label.guest_regs_mapping);

        block_asm.ldr2(Reg::R1, run_scheduler_label.target_pc);
        block_asm.store_guest_reg(Reg::R1, Reg::PC);
        block_asm.dirty_guest_regs -= Reg::PC;
        block_asm.save_dirty_guest_regs(false, false);

        if let Some(exit_label) = &mut run_scheduler_label.exit_label {
            block_asm.bind(exit_label);
            // The invalidated-target exit jumps here directly, skipping the quantum path's
            // pc store and dirty save above — persist both for every cpu, or the main loop
            // re-enters at the stale pc with the block's register state lost (the ARM7 GBA
            // SMC case: a store invalidates the branch target's page mid-block, and the
            // stale re-entry replays the block's stores with writeback forever).
            block_asm.ldr2(Reg::R1, run_scheduler_label.target_pc);
            block_asm.store_guest_reg(Reg::R1, Reg::PC);
            block_asm.save_dirty_guest_regs(false, false);
        }

        if IS_DEBUG {
            let pc = block_asm.current_pc;
            block_asm.ldr2(Reg::R0, ptr::addr_of_mut!(self.runtime_data) as u32);
            block_asm.mov4(FlagsUpdate_DontCare, Cond::AL, Reg::R1, &pc.into());
            block_asm.str2(Reg::R1, &(Reg::R0, JitRuntimeData::get_branch_out_pc_offset() as i32).into());
        }
        block_asm.exit_guest_context(&mut self.runtime_data.host_sp);
    }

    pub fn emit_blx_reg(&mut self, inst_index: usize, basic_block_index: usize, block_asm: &mut BlockAsm) {
        let inst = &self.jit_buf.insts[inst_index];
        let op0 = inst.operands()[0].as_reg_no_shift().unwrap();
        let op0_mapped = block_asm.get_guest_map(op0);

        let pc_reg = block_asm.get_guest_map(Reg::PC);
        block_asm.mov2(pc_reg, &op0_mapped.into());

        let lr_reg = block_asm.get_guest_map(Reg::LR);
        let pc = block_asm.current_pc;
        block_asm.ldr2(lr_reg, pc + 4);

        block_asm.save_dirty_guest_regs_additional(true, inst.cond == Cond::AL, reg_reserve!(Reg::LR, Reg::PC));

        self.emit_branch_reg(inst_index, basic_block_index, pc_reg, true, block_asm);
    }

    pub fn emit_bl(&mut self, inst_index: usize, basic_block_index: usize, block_asm: &mut BlockAsm) {
        let inst = &self.jit_buf.insts[inst_index];
        let relative_pc = inst.operands()[0].as_imm().unwrap() as i32 + 8;
        let target_pc = (block_asm.current_pc as i32 + relative_pc) as u32;

        let pc_reg = block_asm.get_guest_map(Reg::PC);
        block_asm.ldr2(pc_reg, target_pc);

        let lr_reg = block_asm.get_guest_map(Reg::LR);
        let pc = block_asm.current_pc;
        block_asm.ldr2(lr_reg, pc + 4);
        block_asm.save_dirty_guest_regs_additional(true, inst.cond == Cond::AL, reg_reserve!(Reg::LR, Reg::PC));

        self.emit_branch_external_label(inst_index, basic_block_index, target_pc, true, false, block_asm);
    }

    pub fn emit_b(&mut self, inst_index: usize, basic_block_index: usize, skip_label: &mut Label, block_asm: &mut BlockAsm) {
        let inst = &self.jit_buf.insts[inst_index];
        let relative_pc = inst.operands()[0].as_imm().unwrap() as i32 + 8;
        let target_pc = (block_asm.current_pc as i32 + relative_pc) as u32;

        self.emit_branch_label(inst_index, basic_block_index, target_pc, skip_label, block_asm);
    }

    pub fn emit_bx(&mut self, inst_index: usize, basic_block_index: usize, block_asm: &mut BlockAsm) {
        let inst = &self.jit_buf.insts[inst_index];
        let op0 = inst.operands()[0].as_reg_no_shift().unwrap();
        let op0_mapped = block_asm.get_guest_map(op0);

        let pc_reg = block_asm.get_guest_map(Reg::PC);
        block_asm.mov2(pc_reg, &op0_mapped.into());
        block_asm.save_dirty_guest_regs_additional(true, inst.cond == Cond::AL, reg_reserve!(Reg::PC));

        if op0 == Reg::LR {
            self.emit_branch_return_stack(inst_index, pc_reg, block_asm);
        } else {
            self.emit_branch_reg(inst_index, basic_block_index, pc_reg, false, block_asm);
        }
    }

    pub fn emit_blx(&mut self, _inst_index: usize, _basic_block_index: usize, _block_asm: &mut BlockAsm) {
        // ARM BLX imm lives in the cond-0xF space — ARMv5 only, never decoded on the ARMv4T GBA.
    }
}
