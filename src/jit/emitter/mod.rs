// The arm32 emitter owns its instruction-selection files entirely (the ~117 masm
// mnemonics it uses ARE the ARM32 ISA — there is no meaningful shared facade at this
// level, see the port plan D3).
#[cfg(target_arch = "arm")]
mod arm32;

