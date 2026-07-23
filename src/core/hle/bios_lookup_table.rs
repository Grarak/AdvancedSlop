use crate::core::emu::Emu;
use crate::core::hle::bios::*;

// GBA BIOS SWI numbering (comment field of the swi instruction). Out-of-range
// comments clamp to the last entry (unknown).
pub const GBA_SWI_LOOKUP_TABLE: [(&str, fn(&mut Emu)); 40] = [
    ("soft_reset", soft_reset),          // 0x00
    ("register_ram_reset", register_ram_reset), // 0x01
    ("halt", halt),                      // 0x02
    ("stop", sleep),                     // 0x03
    ("interrupt_wait", interrupt_wait),  // 0x04
    ("v_blank_intr_wait", v_blank_intr_wait), // 0x05
    ("divide", divide),                  // 0x06
    ("divide_arm", divide_arm),          // 0x07
    ("square_root", square_root),        // 0x08
    ("arc_tan", arc_tan),                // 0x09
    ("arc_tan2", arc_tan2),              // 0x0A
    ("cpu_set", cpu_set),                // 0x0B
    ("cpu_fast_set", cpu_fast_set),      // 0x0C
    ("bios_checksum", bios_checksum),    // 0x0D
    ("bg_affine_set", bg_affine_set),    // 0x0E
    ("obj_affine_set", obj_affine_set),  // 0x0F
    ("bit_unpack", bit_unpack),          // 0x10
    ("lz77_uncomp", lz77_uncomp),        // 0x11 (wram)
    ("lz77_uncomp", lz77_uncomp),        // 0x12 (vram)
    ("huff_uncomp", huff_uncomp),        // 0x13
    ("runlen_uncomp", runlen_uncomp),    // 0x14 (wram)
    ("runlen_uncomp", runlen_uncomp),    // 0x15 (vram)
    ("diff_unfilt8", diff_unfilt8),      // 0x16 (wram)
    ("diff_unfilt8", diff_unfilt8),      // 0x17 (vram)
    ("diff_unfilt16", diff_unfilt16),    // 0x18
    ("sound_bias", sound_bias),          // 0x19
    ("unknown", unknown),                // 0x1A SoundDriverInit
    ("unknown", unknown),                // 0x1B SoundDriverMode
    ("unknown", unknown),                // 0x1C SoundDriverMain
    ("unknown", unknown),                // 0x1D SoundDriverVSync
    ("unknown", unknown),                // 0x1E SoundChannelClear
    ("midi_key_2_freq", midi_key_2_freq), // 0x1F
    ("unknown", unknown),                // 0x20 MusicPlayerOpen
    ("unknown", unknown),                // 0x21 MusicPlayerStart
    ("unknown", unknown),                // 0x22 MusicPlayerStop
    ("unknown", unknown),                // 0x23 MusicPlayerContinue
    ("unknown", unknown),                // 0x24 MusicPlayerFadeOut
    ("unknown", unknown),                // 0x25 MultiBoot
    ("unknown", unknown),                // 0x26 HardReset
    ("unknown", unknown),                // catch-all
];
