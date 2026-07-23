use crate::core::emu::Emu;
pub use crate::inst_log_format::MemLogKind;
use crate::utils::Convert;

#[inline]
pub fn count_vblank_delivery() {}

#[inline]
pub fn is_logging() -> bool {
    false
}

#[inline]
pub fn log_text(_: &str) {}

#[inline]
pub fn log(_: &Emu, _: u32, _: u32) {}

#[inline]
pub fn log_mem(_: MemLogKind, _: u32, _: u32) {}

#[inline]
pub fn log_mem_slice<T: Convert>(_: MemLogKind, _: u32, _: &[T]) {}

#[inline]
pub fn flush() {}

pub fn init(_: &str) {}

pub fn init_lazy(_: &str) {}

pub fn decode_file(_: &str) {
    eprintln!("decode-inst-log requires a debug build");
}
