#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ExceptionVector {
    Reset = 0x0,
    UndefinedInstruction = 0x4,
    SoftwareInterrupt = 0x8,
    PrefetchAbort = 0xC,
    DataAbort = 0x10,
    AddressExceeds26Bit = 0x14,
    NormalInterrupt = 0x18,
    FastInterrupt = 0x1C,
}

mod handler {
    use crate::core::emu::Emu;
    use crate::core::exception_handler::ExceptionVector;
    use crate::core::hle::bios;
        use crate::logging::debug_panic;

    // HLE-only BIOS: swi and irq route straight into the hle handlers
    pub fn handle(emu: &mut Emu, comment: u8, vector: ExceptionVector) {
        match vector {
            ExceptionVector::SoftwareInterrupt => bios::swi(comment, emu),
            ExceptionVector::NormalInterrupt => bios::interrupt(emu),
            _ => debug_panic!("unhandled exception vector: {vector:?}"),
        }
    }
}

pub use handler::handle;
