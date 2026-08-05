//! R3000A register presentation for gdb-remote clients.
//!
//! The `g`-packet layout follows GDB's classic raw MIPS numbering so plain
//! GDB works unmodified: r0..r31, sr, lo, hi, badvaddr, cause, pc (38 x u32,
//! no FPU on the R3000A). `target.xml` uses the GDB-standard register names
//! for `org.gnu.gdb.mips.cpu`/`cp0`, with LLDB's extension attributes
//! (`altname`, `generic`, `dwarf_regnum`) so LLDB maps sp/fp/ra correctly.

use psx_core::PsxSystem;

/// Number of registers in the `g` packet.
pub const NUM_REGS: usize = 38;

/// ABI names for r0..r31, used as alt-names and in `qRegisterInfo`.
pub const ABI_NAMES: [&str; 32] = [
    "zero", "at", "v0", "v1", "a0", "a1", "a2", "a3", // 0-7
    "t0", "t1", "t2", "t3", "t4", "t5", "t6", "t7", // 8-15
    "s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7", // 16-23
    "t8", "t9", "k0", "k1", "gp", "sp", "s8", "ra", // 24-31
];

pub fn read(sys: &PsxSystem, i: usize) -> u32 {
    let c = &sys.cpu;
    match i {
        0..=31 => c.regs[i],
        32 => c.cop0.sr,
        33 => c.lo,
        34 => c.hi,
        35 => c.cop0.bad_vaddr,
        36 => c.cop0.cause,
        37 => c.pc,
        _ => 0,
    }
}

pub fn write(sys: &mut PsxSystem, i: usize, v: u32) {
    let c = &mut sys.cpu;
    match i {
        // r0 is hardwired to zero
        1..=31 => c.regs[i] = v,
        32 => c.cop0.sr = v,
        33 => c.lo = v,
        34 => c.hi = v,
        35 => c.cop0.bad_vaddr = v,
        36 => c.cop0.cause = v,
        // Redirecting pc must also cancel any in-flight branch target.
        37 => {
            c.pc = v;
            c.next_pc = v.wrapping_add(4);
        }
        _ => {}
    }
}

/// The `target.xml` document served via `qXfer:features:read`.
pub fn target_xml() -> String {
    let mut xml = String::from(
        r#"<?xml version="1.0"?>
<!DOCTYPE target SYSTEM "gdb-target.dtd">
<target version="1.0">
  <architecture>mips</architecture>
  <feature name="org.gnu.gdb.mips.cpu">
"#,
    );
    for (i, abi) in ABI_NAMES.iter().enumerate() {
        let generic = match i {
            29 => r#" generic="sp""#,
            30 => r#" generic="fp""#,
            31 => r#" generic="ra""#,
            _ => "",
        };
        xml.push_str(&format!(
            "    <reg name=\"r{i}\" altname=\"{abi}\" bitsize=\"32\" regnum=\"{i}\" \
             dwarf_regnum=\"{i}\"{generic}/>\n"
        ));
    }
    xml.push_str(
        r#"    <reg name="lo" bitsize="32" regnum="33"/>
    <reg name="hi" bitsize="32" regnum="34"/>
    <reg name="pc" bitsize="32" regnum="37" type="code_ptr" generic="pc"/>
  </feature>
  <feature name="org.gnu.gdb.mips.cp0">
    <reg name="status" bitsize="32" regnum="32"/>
    <reg name="badvaddr" bitsize="32" regnum="35"/>
    <reg name="cause" bitsize="32" regnum="36"/>
  </feature>
</target>
"#,
    );
    xml
}

/// Reply to LLDB's `qRegisterInfo<n>`: one register description per query,
/// `E45` past the end. Field reference: lldb docs/lldb-gdb-remote.txt.
pub fn register_info(i: usize) -> Option<String> {
    if i >= NUM_REGS {
        return None;
    }
    let (name, set, generic): (&str, &str, Option<&str>) = match i {
        0..=31 => (
            ABI_NAMES[i],
            "General Purpose Registers",
            match i {
                4..=7 => Some(["arg1", "arg2", "arg3", "arg4"][i - 4]),
                29 => Some("sp"),
                30 => Some("fp"),
                31 => Some("ra"),
                _ => None,
            },
        ),
        32 => ("sr", "Control Registers", None),
        33 => ("lo", "General Purpose Registers", None),
        34 => ("hi", "General Purpose Registers", None),
        35 => ("badvaddr", "Control Registers", None),
        36 => ("cause", "Control Registers", None),
        37 => ("pc", "General Purpose Registers", Some("pc")),
        _ => unreachable!(),
    };
    let mut out = format!(
        "name:{name};bitsize:32;offset:{};encoding:uint;format:hex;set:{set};",
        i * 4
    );
    if i <= 31 {
        out.push_str(&format!("alt-name:r{i};dwarf:{i};gcc:{i};"));
    }
    if let Some(g) = generic {
        out.push_str(&format!("generic:{g};"));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sys() -> PsxSystem {
        PsxSystem::new(vec![0; 512 * 1024]).unwrap()
    }

    #[test]
    fn r0_stays_zero() {
        let mut s = sys();
        write(&mut s, 0, 0xdead_beef);
        assert_eq!(read(&s, 0), 0);
    }

    #[test]
    fn pc_write_cancels_pending_branch() {
        let mut s = sys();
        write(&mut s, 37, 0x8010_0000);
        assert_eq!(s.cpu.pc, 0x8010_0000);
        assert_eq!(s.cpu.next_pc, 0x8010_0004);
    }

    #[test]
    fn register_info_covers_all_and_ends() {
        for i in 0..NUM_REGS {
            let info = register_info(i).unwrap();
            assert!(info.contains("bitsize:32"), "{info}");
        }
        assert!(register_info(NUM_REGS).is_none());
    }

    #[test]
    fn target_xml_names_all_gdb_registers() {
        let xml = target_xml();
        for name in ["r0", "r31", "lo", "hi", "pc", "status", "badvaddr", "cause"] {
            assert!(xml.contains(&format!("name=\"{name}\"")), "{name} missing");
        }
    }
}
