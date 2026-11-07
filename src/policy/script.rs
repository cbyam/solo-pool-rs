//! Script parsing and data-carrier byte counting.
//!
//! `get_op` mirrors Bitcoin Core's `GetScriptOp`, and the counting functions
//! are ports of Bitcoin Roots' `OlgaBytes`, `OpNetWitnessBytes` and
//! `ScriptDatacarrierBytes` (`src/policy/policy.cpp`, MIT licensed, Copyright
//! The Bitcoin Core developers). They work on raw script bytes so the results
//! match the C++ byte for byte, including its arithmetic: where the C++ uses
//! `size_t`, the port uses wrapping `usize` operations.

pub(crate) const OP_0: u8 = 0x00;
pub(crate) const OP_PUSHDATA1: u8 = 0x4c;
pub(crate) const OP_PUSHDATA2: u8 = 0x4d;
pub(crate) const OP_PUSHDATA4: u8 = 0x4e;
pub(crate) const OP_1NEGATE: u8 = 0x4f;
pub(crate) const OP_1: u8 = 0x51;
pub(crate) const OP_13: u8 = 0x5d;
pub(crate) const OP_16: u8 = 0x60;
pub(crate) const OP_IF: u8 = 0x63;
pub(crate) const OP_NOTIF: u8 = 0x64;
pub(crate) const OP_ENDIF: u8 = 0x68;
pub(crate) const OP_RETURN: u8 = 0x6a;
pub(crate) const OP_DROP: u8 = 0x75;
pub(crate) const OP_CHECKSIG: u8 = 0xac;
pub(crate) const OP_CHECKSIGVERIFY: u8 = 0xad;
pub(crate) const OP_CHECKMULTISIG: u8 = 0xae;
pub(crate) const OP_INVALIDOPCODE: u8 = 0xff;

/// Core's `MAX_SCRIPT_ELEMENT_SIZE`.
pub(crate) const MAX_SCRIPT_ELEMENT_SIZE: usize = 520;
/// Core's `MAX_SCRIPT_SIZE`.
pub(crate) const MAX_SCRIPT_SIZE: usize = 10_000;
/// Core's `WITNESS_V0_SCRIPTHASH_SIZE`.
const WITNESS_V0_SCRIPTHASH_SIZE: usize = 32;

/// One parsed operation: the opcode and, for a push, the pushed bytes.
pub(crate) struct Op<'a> {
    pub opcode: u8,
    pub data: &'a [u8],
}

/// Parse the operation at `*pc` and advance past it. `None` where Core's
/// `GetScriptOp` returns false: at the end of the script, or on a push whose
/// length runs past the end.
pub(crate) fn get_op<'a>(script: &'a [u8], pc: &mut usize) -> Option<Op<'a>> {
    let opcode = *script.get(*pc)?;
    *pc += 1;
    let mut data: &[u8] = &[];
    if opcode <= OP_PUSHDATA4 {
        let size = if opcode < OP_PUSHDATA1 {
            opcode as usize
        } else {
            let width = match opcode {
                OP_PUSHDATA1 => 1,
                OP_PUSHDATA2 => 2,
                _ => 4,
            };
            let len = script.get(*pc..*pc + width)?;
            *pc += width;
            len.iter()
                .rev()
                .fold(0usize, |acc, &b| (acc << 8) | b as usize)
        };
        let end = pc.checked_add(size)?;
        data = script.get(*pc..end)?;
        *pc = end;
    }
    Some(Op { opcode, data })
}

/// Core's `CScript::IsPushOnly(pc)`: every operation from `start` is a push or
/// a small-number opcode up to `OP_16`. `OP_RESERVED` counts as a push, as it
/// does in Core.
pub(crate) fn is_push_only(script: &[u8], start: usize) -> bool {
    let mut pc = start;
    while pc < script.len() {
        match get_op(script, &mut pc) {
            Some(op) if op.opcode <= OP_16 => {}
            _ => return false,
        }
    }
    true
}

/// Core's `CScript::IsPayToScriptHash`.
pub(crate) fn is_p2sh(script: &[u8]) -> bool {
    script.len() == 23 && script[0] == 0xa9 && script[1] == 0x14 && script[22] == 0x87
}

/// Core's `CScript::IsPayToWitnessScriptHash`.
pub(crate) fn is_p2wsh(script: &[u8]) -> bool {
    script.len() == 34 && script[0] == OP_0 && script[1] == 0x20
}

/// Core's `CScript::IsWitnessProgram`: the witness version and program.
pub(crate) fn witness_program(script: &[u8]) -> Option<(u8, &[u8])> {
    if script.len() < 4 || script.len() > 42 {
        return None;
    }
    let version = match script[0] {
        OP_0 => 0,
        op @ OP_1..=OP_16 => op - (OP_1 - 1),
        _ => return None,
    };
    if script[1] as usize + 2 != script.len() {
        return None;
    }
    Some((version, &script[2..]))
}

/// The stack a push-only script leaves, as Core's `EvalScript` with no
/// verification flags builds it. `None` where `EvalScript` fails: a script
/// over 10,000 bytes, a push over 520 bytes, a parse failure, `OP_RESERVED`,
/// or any non-push opcode (a standard scriptSig never has one).
pub(crate) fn push_only_stack(script: &[u8]) -> Option<Vec<Vec<u8>>> {
    if script.len() > MAX_SCRIPT_SIZE {
        return None;
    }
    let mut stack = Vec::new();
    let mut pc = 0;
    while pc < script.len() {
        let op = get_op(script, &mut pc)?;
        match op.opcode {
            OP_0..=OP_PUSHDATA4 => {
                if op.data.len() > MAX_SCRIPT_ELEMENT_SIZE {
                    return None;
                }
                stack.push(op.data.to_vec());
            }
            OP_1NEGATE => stack.push(vec![0x81]),
            OP_1..=OP_16 => stack.push(vec![op.opcode - (OP_1 - 1)]),
            _ => return None,
        }
    }
    Some(stack)
}

/// Roots' `OlgaBytes`: the data bytes of an Olga (Stamps over P2WSH) payload
/// that starts at this output, or 0. `remaining_outputs` counts this output and
/// every one after it.
pub(crate) fn olga_bytes(script: &[u8], remaining_outputs: usize) -> usize {
    if !is_p2wsh(script) {
        return 0;
    }
    let payload_size = ((script[2] as usize) << 8) | script[3] as usize;
    let outputs = (payload_size + 1) / WITNESS_V0_SCRIPTHASH_SIZE + 1;
    if remaining_outputs < outputs {
        return 0;
    }
    if (script[4] | 0x20) != b's'
        || (script[5] | 0x20) != b't'
        || (script[6] | 0x20) != b'a'
        || (script[7] | 0x20) != b'm'
        || (script[8] | 0x20) != b'p'
        || script[9] != b':'
    {
        return 0;
    }
    outputs * (WITNESS_V0_SCRIPTHASH_SIZE + 1 + 8)
}

/// Roots' `OpNetWitnessBytes`: the data bytes of an OPNet witness, or 0.
pub(crate) fn opnet_witness_bytes(stack: &[&[u8]]) -> usize {
    if stack.len() != 5 || stack[4].len() != 65 {
        return 0;
    }
    let tapscript = stack[3];
    let mut found_opnet = false;
    let mut deduct = 0usize;
    let mut pc = 0;
    let mut opcode = OP_INVALIDOPCODE;
    while pc < tapscript.len() {
        let last_opcode = opcode;
        let Some(op) = get_op(tapscript, &mut pc) else {
            break;
        };
        opcode = op.opcode;
        if op.data == [0x6f, 0x70] {
            found_opnet = true;
        }
        if opcode == OP_CHECKSIGVERIFY && last_opcode == 0x20 {
            deduct = deduct.wrapping_add(34);
        }
    }
    if found_opnet {
        stack[0]
            .len()
            .wrapping_add(stack[3].len())
            .wrapping_sub(deduct)
    } else {
        0
    }
}

/// Roots' `ScriptDatacarrierBytes`: `(standard, nonstandard)` data-carrier
/// bytes in one script. OP_RETURN outputs are standard; data embedded in an
/// executed script (an `OP_FALSE OP_IF ... OP_ENDIF` envelope, a push followed
/// by `OP_DROP`), Olga and OPNet are nonstandard. A script that fails to parse
/// counts entirely as nonstandard. `witness` is the input's witness stack, for
/// the OPNet check; outputs pass `None`.
pub(crate) fn datacarrier_bytes(
    script: &[u8],
    remaining_outputs: usize,
    witness: Option<&[&[u8]]>,
) -> (usize, usize) {
    let olga = olga_bytes(script, remaining_outputs);
    if olga != 0 {
        return (0, olga);
    }
    if let Some(stack) = witness {
        let opnet = opnet_witness_bytes(stack);
        if opnet != 0 {
            return (0, opnet);
        }
    }

    let mut counted = 0usize;
    let mut opcode = OP_INVALIDOPCODE;
    let mut inside_noop = 0u32;
    let mut inside_conditional = 0u32;
    let mut data_began = 0usize;
    let mut pc = 0usize;
    while pc < script.len() {
        let last_opcode = opcode;
        let opcode_at = pc;
        let Some(op) = get_op(script, &mut pc) else {
            return (0, script.len());
        };
        opcode = op.opcode;

        if opcode == OP_IF || opcode == OP_NOTIF {
            inside_conditional += 1;
        } else if opcode == OP_ENDIF {
            if inside_conditional == 0 {
                return (0, script.len());
            }
            inside_conditional -= 1;
        } else if opcode == OP_RETURN && inside_conditional == 0 {
            return (script.len(), 0);
        }

        if inside_noop != 0 {
            if opcode == OP_IF || opcode == OP_NOTIF {
                inside_noop += 1;
            } else if opcode == OP_ENDIF {
                inside_noop -= 1;
                if inside_noop == 0 {
                    counted = counted.wrapping_add(pc - data_began + 1);
                }
            }
        } else if opcode == OP_IF && last_opcode == OP_0 {
            inside_noop = 1;
            data_began = opcode_at;
        } else if opcode <= OP_PUSHDATA4 {
            data_began = opcode_at;
        } else if opcode == OP_DROP && last_opcode <= OP_PUSHDATA4 {
            counted = counted.wrapping_add(pc - data_began);
        }
    }
    (0, counted)
}
