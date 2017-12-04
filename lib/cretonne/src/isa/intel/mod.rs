//! Intel Instruction Set Architectures.

pub mod settings;
mod abi;
mod binemit;
mod enc_tables;
mod registers;

use binemit::{CodeSink, MemoryCodeSink, emit_function};
use super::super::settings as shared_settings;
use isa::enc_tables::{self as shared_enc_tables, lookup_enclist, Encodings};
use isa::Builder as IsaBuilder;
use isa::{TargetIsa, RegInfo, RegClass, EncInfo, RegUnit};
use self::registers::RU;
use ir;
use regalloc;
use result;
use ir::InstBuilder;
use ir::immediates::Imm64;
use stack_layout::layout_stack;
use cursor::{Cursor, EncCursor};


#[allow(dead_code)]
struct Isa {
    shared_flags: shared_settings::Flags,
    isa_flags: settings::Flags,
    cpumode: &'static [shared_enc_tables::Level1Entry<u16>],
}

/// Get an ISA builder for creating Intel targets.
pub fn isa_builder() -> IsaBuilder {
    IsaBuilder {
        setup: settings::builder(),
        constructor: isa_constructor,
    }
}

fn isa_constructor(
    shared_flags: shared_settings::Flags,
    builder: &shared_settings::Builder,
) -> Box<TargetIsa> {
    let level1 = if shared_flags.is_64bit() {
        &enc_tables::LEVEL1_I64[..]
    } else {
        &enc_tables::LEVEL1_I32[..]
    };
    Box::new(Isa {
        isa_flags: settings::Flags::new(&shared_flags, builder),
        shared_flags,
        cpumode: level1,
    })
}

impl TargetIsa for Isa {
    fn name(&self) -> &'static str {
        "intel"
    }

    fn flags(&self) -> &shared_settings::Flags {
        &self.shared_flags
    }

    fn register_info(&self) -> RegInfo {
        registers::INFO.clone()
    }

    fn encoding_info(&self) -> EncInfo {
        enc_tables::INFO.clone()
    }

    fn legal_encodings<'a>(
        &'a self,
        dfg: &'a ir::DataFlowGraph,
        inst: &'a ir::InstructionData,
        ctrl_typevar: ir::Type,
    ) -> Encodings<'a> {
        lookup_enclist(
            ctrl_typevar,
            inst,
            dfg,
            self.cpumode,
            &enc_tables::LEVEL2[..],
            &enc_tables::ENCLISTS[..],
            &enc_tables::LEGALIZE_ACTIONS[..],
            &enc_tables::RECIPE_PREDICATES[..],
            &enc_tables::INST_PREDICATES[..],
            self.isa_flags.predicate_view(),
        )
    }

    fn legalize_signature(&self, sig: &mut ir::Signature, current: bool) {
        abi::legalize_signature(sig, &self.shared_flags, current)
    }

    fn regclass_for_abi_type(&self, ty: ir::Type) -> RegClass {
        abi::regclass_for_abi_type(ty)
    }

    fn allocatable_registers(&self, func: &ir::Function) -> regalloc::AllocatableSet {
        abi::allocatable_registers(func, &self.shared_flags)
    }

    fn emit_inst(
        &self,
        func: &ir::Function,
        inst: ir::Inst,
        divert: &mut regalloc::RegDiversions,
        sink: &mut CodeSink,
    ) {
        binemit::emit_inst(func, inst, divert, sink)
    }

    fn emit_function(&self, func: &ir::Function, sink: &mut MemoryCodeSink) {
        emit_function(func, binemit::emit_inst, sink)
    }

    fn reloc_names(&self) -> &'static [&'static str] {
        &binemit::RELOC_NAMES
    }

    fn prologue_epilogue(&self, func: &mut ir::Function) -> result::CtonResult {
        let word_size = if self.flags().is_64bit() { 8 } else { 4 };
        let csr_type = if self.flags().is_64bit() {
            ir::types::I64
        } else {
            ir::types::I32
        };
        let csrs = abi::callee_saved_registers(&self.shared_flags);
        let csr_stack_size = ((csrs.len() + 1) * word_size as usize) as i32;

        func.create_stack_slot(ir::StackSlotData {
            kind: ir::StackSlotKind::IncomingArg,
            size: csr_stack_size as u32,
            offset: -csr_stack_size,
        });

        let total_stack_size = layout_stack(&mut func.stack_slots, word_size)? as i32;
        let local_stack_size = (total_stack_size - csr_stack_size) as i64;

        // Add CSRs to function signature
        let fp_arg = ir::AbiParam::special_reg(
            csr_type,
            ir::ArgumentPurpose::FramePointer,
            RU::rbp as RegUnit,
        );
        func.signature.params.push(fp_arg);
        func.signature.returns.push(fp_arg);

        for csr in csrs.iter() {
            let csr_arg = ir::AbiParam::special_reg(
                csr_type,
                ir::ArgumentPurpose::CalleeSaved,
                *csr as RegUnit,
            );
            func.signature.params.push(csr_arg);
            func.signature.returns.push(csr_arg);
        }


        let entry_ebb = func.layout.entry_block().expect("missing entry block");
        let mut pos = EncCursor::new(func, self).at_first_insertion_point(entry_ebb);

        self.insert_prologue(&mut pos, local_stack_size, csr_type);
        self.insert_epilogues(&mut pos, local_stack_size, csr_type);

        Ok(())
    }
}

impl Isa {
    fn insert_prologue(&self, pos: &mut EncCursor, stack_size: i64, csr_type: ir::types::Type) {
        // Append param to entry EBB
        let ebb = pos.current_ebb().expect("missing ebb under cursor");
        let fp = pos.func.dfg.append_ebb_param(ebb, csr_type);
        pos.func.locations[fp] = ir::ValueLoc::Reg(RU::rbp as RegUnit);

        pos.ins().x86_push(fp);
        pos.ins().copy_special(
            RU::rsp as RegUnit,
            RU::rbp as RegUnit,
        );

        if stack_size > 0 {
            pos.ins().adjust_sp_imm(Imm64::new(-stack_size));
        }

        let csrs = abi::callee_saved_registers(&self.shared_flags);
        for reg in csrs.iter() {
            // Append param to entry EBB
            let csr_arg = pos.func.dfg.append_ebb_param(ebb, csr_type);

            // Assign it a location
            pos.func.locations[csr_arg] = ir::ValueLoc::Reg(*reg as RegUnit);

            // Remember it so we can push it momentarily
            pos.ins().x86_push(csr_arg);
        }
    }

    fn insert_epilogues(&self, pos: &mut EncCursor, stack_size: i64, csr_type: ir::types::Type) {
        while let Some(ebb) = pos.next_ebb() {
            pos.goto_last_inst(ebb);
            if let Some(inst) = pos.current_inst() {
                if pos.func.dfg[inst].opcode().is_return() {
                    self.insert_epilogue(inst, stack_size, pos, csr_type);
                }
            }
        }

    }

    fn insert_epilogue(
        &self,
        inst: ir::Inst,
        stack_size: i64,
        pos: &mut EncCursor,
        csr_type: ir::types::Type,
    ) {
        if stack_size > 0 {
            pos.ins().adjust_sp_imm(Imm64::new(stack_size));
        }

        let fp_ret = pos.ins().x86_pop(csr_type);
        pos.prev_inst();

        pos.func.locations[fp_ret] = ir::ValueLoc::Reg(RU::rbp as RegUnit);
        pos.func.dfg.append_inst_arg(inst, fp_ret);

        let csrs = abi::callee_saved_registers(&self.shared_flags);
        for reg in csrs.iter() {
            let csr_ret = pos.ins().x86_pop(csr_type);
            pos.prev_inst();

            pos.func.locations[csr_ret] = ir::ValueLoc::Reg(*reg as RegUnit);
            pos.func.dfg.append_inst_arg(inst, csr_ret);
        }
    }
}
