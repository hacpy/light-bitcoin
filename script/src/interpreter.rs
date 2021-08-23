#[cfg(not(feature = "std"))]
use alloc::{vec, vec::Vec};
use light_bitcoin_chain::H256;
use light_bitcoin_keys::{Message, Public, Signature};

use core::{cmp, mem};
use light_bitcoin_primitives::Bytes;

use crate::script::{MAX_SCRIPT_ELEMENT_SIZE, MAX_STACK_SIZE};
use crate::sign::{Sighash, SignatureVersion};
use crate::{
    script, stack::Stack, Builder, Error, Num, Opcode, Script, ScriptWitness, SignatureChecker,
    VerificationFlags,
};
use light_bitcoin_crypto::{dhash160, dhash256, ripemd160, sha1, sha256};

pub const SEQUENCE_LOCKTIME_DISABLE_FLAG: u32 = 1u32 << 31;
pub const SCRIPT_VERIFY_TAPROOT: u32 = 1u32 << 17;
pub const ANNEX_TAG: u8 = 0x50;

#[derive(Debug, Default)]
pub struct ScriptExecutionData {
    // Whether m_tapleaf_hash is initialized.
    pub m_tapleaf_hash_init: bool,
    // The tapleaf hash.
    pub m_tapleaf_hash: H256,

    // Whether m_codeseparator_pos is initialized.
    pub m_codeseparator_pos_init: bool,
    // Opcode position of the last executed OP_CODESEPARATOR (or 0xFFFFFFFF if none executed).
    pub m_codeseparator_pos: u32,

    // Whether m_annex_present and (when needed) m_annex_hash are initialized.
    pub m_annex_init: bool,
    // Whether an annex is present.
    pub m_annex_present: bool,
    // Hash of the annex data.
    pub m_annex_hash: H256,

    // Whether m_validation_weight_left is initialized.
    pub m_validation_weight_left_init: bool,
    // How much validation weight is left (decremented for every successful non-empty signature check).
    pub m_validation_weight_left: i64,
}

/// Helper function.
fn check_signature(
    checker: &dyn SignatureChecker,
    script_sig: &Vec<u8>,
    public: &Vec<u8>,
    script_code: &Script,
    version: SignatureVersion,
) -> bool {
    let public = match Public::from_slice(&public) {
        Ok(public) => public,
        _ => return false,
    };

    if let Some((hash_type, sig)) = script_sig.split_last() {
        checker.check_signature(
            &sig.into(),
            &public,
            script_code,
            *hash_type as u32,
            version,
        )
    } else {
        return false;
    }
}

/// Helper function.
fn verify_signature(
    checker: &dyn SignatureChecker,
    signature: Vec<u8>,
    public: Vec<u8>,
    message: Message,
) -> bool {
    let public = match Public::from_slice(&public) {
        Ok(public) => public,
        _ => return false,
    };

    if signature.is_empty() {
        return false;
    }

    checker.verify_signature(&signature.into(), &public, &message.into())
}

fn is_public_key(v: &[u8]) -> bool {
    match v.len() {
        33 if v[0] == 2 || v[0] == 3 => true,
        65 if v[0] == 4 => true,
        _ => false,
    }
}

/// A canonical signature exists of: <30> <total len> <02> <len R> <R> <02> <len S> <S> <hashtype>
/// Where R and S are not negative (their first byte has its highest bit not set), and not
/// excessively padded (do not start with a 0 byte, unless an otherwise negative number follows,
/// in which case a single 0 byte is necessary and even required).
///
/// See https://bitcointalk.org/index.php?topic=8392.msg127623#msg127623
///
/// This function is consensus-critical since BIP66.
fn is_valid_signature_encoding(sig: &[u8]) -> bool {
    // Format: 0x30 [total-length] 0x02 [R-length] [R] 0x02 [S-length] [S] [sighash]
    // * total-length: 1-byte length descriptor of everything that follows,
    //   excluding the sighash byte.
    // * R-length: 1-byte length descriptor of the R value that follows.
    // * R: arbitrary-length big-endian encoded R value. It must use the shortest
    //   possible encoding for a positive integers (which means no null bytes at
    //   the start, except a single one when the next byte has its highest bit set).
    // * S-length: 1-byte length descriptor of the S value that follows.
    // * S: arbitrary-length big-endian encoded S value. The same rules apply.
    // * sighash: 1-byte value indicating what data is hashed (not part of the DER
    //   signature)

    // Minimum and maximum size constraints
    if sig.len() < 9 || sig.len() > 73 {
        return false;
    }

    // A signature is of type 0x30 (compound)
    if sig[0] != 0x30 {
        return false;
    }

    // Make sure the length covers the entire signature.
    if sig[1] as usize != sig.len() - 3 {
        return false;
    }

    // Extract the length of the R element.
    let len_r = sig[3] as usize;

    // Make sure the length of the S element is still inside the signature.
    if len_r + 5 >= sig.len() {
        return false;
    }

    // Extract the length of the S element.
    let len_s = sig[len_r + 5] as usize;

    // Verify that the length of the signature matches the sum of the length
    if len_r + len_s + 7 != sig.len() {
        return false;
    }

    // Check whether the R element is an integer.
    if sig[2] != 2 {
        return false;
    }

    // Zero-length integers are not allowed for R.
    if len_r == 0 {
        return false;
    }

    // Negative numbers are not allowed for R.
    if (sig[4] & 0x80) != 0 {
        return false;
    }

    // Null bytes at the start of R are not allowed, unless R would
    // otherwise be interpreted as a negative number.
    if len_r > 1 && sig[4] == 0 && (sig[5] & 0x80) == 0 {
        return false;
    }

    // Check whether the S element is an integer.
    if sig[len_r + 4] != 2 {
        return false;
    }

    // Zero-length integers are not allowed for S.
    if len_s == 0 {
        return false;
    }

    // Negative numbers are not allowed for S.
    if (sig[len_r + 6] & 0x80) != 0 {
        return false;
    }

    // Null bytes at the start of S are not allowed, unless S would otherwise be
    // interpreted as a negative number.
    if len_s > 1 && (sig[len_r + 6] == 0) && (sig[len_r + 7] & 0x80) == 0 {
        return false;
    }

    true
}

fn is_low_der_signature(sig: &[u8]) -> Result<(), Error> {
    if !is_valid_signature_encoding(sig) {
        return Err(Error::SignatureDer);
    }

    let signature: Signature = sig.into();
    if !signature.check_low_s() {
        return Err(Error::SignatureHighS);
    }

    Ok(())
}

fn is_defined_hashtype_signature(version: SignatureVersion, sig: &[u8]) -> bool {
    if sig.is_empty() {
        return false;
    }

    Sighash::is_defined(version, sig[sig.len() - 1] as u32)
}

fn parse_hash_type(version: SignatureVersion, sig: &[u8]) -> Sighash {
    Sighash::from_u32(
        version,
        if sig.is_empty() {
            0
        } else {
            sig[sig.len() - 1] as u32
        },
    )
}

fn check_signature_encoding(
    sig: &[u8],
    flags: &VerificationFlags,
    version: SignatureVersion,
) -> Result<(), Error> {
    // Empty signature. Not strictly DER encoded, but allowed to provide a
    // compact way to provide an invalid signature for use with CHECK(MULTI)SIG

    if sig.is_empty() {
        return Ok(());
    }

    if (flags.verify_dersig || flags.verify_low_s || flags.verify_strictenc)
        && !is_valid_signature_encoding(sig)
    {
        return Err(Error::SignatureDer);
    }

    if flags.verify_low_s {
        is_low_der_signature(sig)?;
    }

    if flags.verify_strictenc && !is_defined_hashtype_signature(version, sig) {
        return Err(Error::SignatureHashtype);
    }

    // verify_strictenc is currently enabled for BitcoinCash only
    if flags.verify_strictenc {
        let uses_fork_id = parse_hash_type(version, sig).fork_id;
        let enabled_fork_id = version == SignatureVersion::ForkId;
        if uses_fork_id && !enabled_fork_id {
            return Err(Error::SignatureIllegalForkId);
        } else if !uses_fork_id && enabled_fork_id {
            return Err(Error::SignatureMustUseForkId);
        }
    }

    Ok(())
}

fn check_pubkey_encoding(v: &[u8], flags: &VerificationFlags) -> Result<(), Error> {
    if flags.verify_strictenc && !is_public_key(v) {
        return Err(Error::PubkeyType);
    }

    Ok(())
}

fn check_minimal_push(data: &[u8], opcode: Opcode) -> bool {
    if data.is_empty() {
        // Could have used OP_0.
        opcode == Opcode::OP_0
    } else if data.len() == 1 && data[0] >= 1 && data[0] <= 16 {
        // Could have used OP_1 .. OP_16.
        opcode as u8 == Opcode::OP_1 as u8 + (data[0] - 1)
    } else if data.len() == 1 && data[0] == 0x81 {
        // Could have used OP_1NEGATE
        opcode == Opcode::OP_1NEGATE
    } else if data.len() <= 75 {
        // Could have used a direct push (opcode indicating number of bytes pushed + those bytes).
        opcode as usize == data.len()
    } else if data.len() <= 255 {
        // Could have used OP_PUSHDATA.
        opcode == Opcode::OP_PUSHDATA1
    } else if data.len() <= 65535 {
        // Could have used OP_PUSHDATA2.
        opcode == Opcode::OP_PUSHDATA2
    } else {
        true
    }
}

fn cast_to_bool(data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }

    if data[..data.len() - 1].iter().any(|x| x != &0) {
        return true;
    }

    let last = data[data.len() - 1];
    !(last == 0 || last == 0x80)
}

/// Verifies script signature and pubkey
pub fn verify_script(
    script_sig: &Script,
    script_pubkey: &Script,
    witness: &ScriptWitness,
    flags: &VerificationFlags,
    checker: &dyn SignatureChecker,
    version: SignatureVersion,
) -> Result<(), Error> {
    if flags.verify_sigpushonly && !script_sig.is_push_only() {
        return Err(Error::SignaturePushOnly);
    }

    let mut stack = Stack::new();
    let mut stack_copy = Stack::new();
    let mut had_witness = false;

    eval_script(&mut stack, script_sig, flags, checker, version)?;

    if flags.verify_p2sh {
        stack_copy = stack.clone();
    }

    let res = eval_script(&mut stack, script_pubkey, flags, checker, version)?;
    if !res {
        return Err(Error::EvalFalse);
    }

    // Verify witness program
    let mut verify_cleanstack = flags.verify_cleanstack;
    if flags.verify_witness {
        if let Some((witness_version, witness_program)) = script_pubkey.parse_witness_program() {
            if !script_sig.is_empty() {
                return Err(Error::WitnessMalleated);
            }

            had_witness = true;
            verify_cleanstack = false;
            if !verify_witness_program(witness, witness_version, witness_program, flags, checker)? {
                return Err(Error::EvalFalse);
            }
        }
    }

    // Additional validation for spend-to-script-hash transactions:
    if flags.verify_p2sh && script_pubkey.is_pay_to_script_hash() {
        if !script_sig.is_push_only() {
            return Err(Error::SignaturePushOnly);
        }

        mem::swap(&mut stack, &mut stack_copy);

        // stack cannot be empty here, because if it was the
        // P2SH  HASH <> EQUAL  scriptPubKey would be evaluated with
        // an empty stack and the EvalScript above would return false.
        assert!(!stack.is_empty());

        let pubkey2: Script = stack.pop()?.into();

        let res = eval_script(&mut stack, &pubkey2, flags, checker, version)?;
        if !res {
            return Err(Error::EvalFalse);
        }

        if flags.verify_witness {
            if let Some((witness_version, witness_program)) = pubkey2.parse_witness_program() {
                if script_sig != &Builder::default().push_data(&pubkey2).into_script() {
                    return Err(Error::WitnessMalleatedP2SH);
                }

                had_witness = true;
                verify_cleanstack = false;
                if !verify_witness_program(
                    witness,
                    witness_version,
                    witness_program,
                    flags,
                    checker,
                )? {
                    return Err(Error::EvalFalse);
                }
            }
        }
    }

    // The CLEANSTACK check is only performed after potential P2SH evaluation,
    // as the non-P2SH evaluation of a P2SH script will obviously not result in
    // a clean stack (the P2SH inputs remain). The same holds for witness evaluation.
    if verify_cleanstack {
        // Disallow CLEANSTACK without P2SH, as otherwise a switch CLEANSTACK->P2SH+CLEANSTACK
        // would be possible, which is not a softfork (and P2SH should be one).
        assert!(flags.verify_p2sh);
        if stack.len() != 1 {
            return Err(Error::Cleanstack);
        }
    }

    if flags.verify_witness {
        // We can't check for correct unexpected witness data if P2SH was off, so require
        // that WITNESS implies P2SH. Otherwise, going from WITNESS->P2SH+WITNESS would be
        // possible, which is not a softfork.
        assert!(flags.verify_p2sh);
        if !had_witness && !witness.is_empty() {
            return Err(Error::WitnessUnexpected);
        }
    }

    Ok(())
}

fn execute_witness_script(
    stack: &mut Stack<Bytes>,
    script: &Script,
    flags: &VerificationFlags,
    checker: &dyn SignatureChecker,
    version: SignatureVersion,
) -> Result<bool, Error> {
    if version == SignatureVersion::TapScript {
        // OP_SUCCESSx processing overrides everything, including stack element size limits
        for i in 0..script.len() {
            // Note how this condition would not be reached if an unknown OP_SUCCESSx was found
            let s = script.get_opcode(i)?;

            // New opcodes will be listed here. May use a different sigversion to modify existing opcodes.
            if s.is_success() {
                if flags.verify_discourage_op_success {
                    return Err(Error::DiscourageUpgradableOpSuccess);
                }
                return Ok(true);
            }
        }

        // Tapscript enforces initial stack size limits (altstack is empty here)
        if stack.len() > MAX_STACK_SIZE {
            return Err(Error::StackSize);
        }
    }

    // Disallow stack item size > MAX_SCRIPT_ELEMENT_SIZE in witness stack
    if stack.iter().any(|s| s.len() > MAX_SCRIPT_ELEMENT_SIZE) {
        return Err(Error::PushSize);
    }

    // Run the script interpreter.
    if !eval_script(stack, &script, flags, checker, version)? {
        return Ok(false);
    }

    // Scripts inside witness implicitly require cleanstack behaviour
    if stack.len() != 1 {
        return Err(Error::EvalFalse);
    }

    let success = cast_to_bool(
        stack
            .last()
            .expect("stack.len() == 1; last() only returns errors when stack is empty; qed"),
    );
    Ok(success)
}

fn verify_witness_program(
    witness: &ScriptWitness,
    witness_version: u8,
    witness_program: &[u8],
    flags: &VerificationFlags,
    checker: &dyn SignatureChecker,
) -> Result<bool, Error> {
    let witness_stack = witness;
    let witness_stack_len = witness_stack.len();

    if witness_version != 0 {
        if flags.verify_discourage_upgradable_witness_program {
            return Err(Error::DiscourageUpgradableWitnessProgram);
        }

        return Ok(true);
    }

    let (mut stack, script_pubkey): (Stack<_>, Script) = match witness_program.len() {
        32 => {
            if witness_stack_len == 0 {
                return Err(Error::WitnessProgramWitnessEmpty);
            }

            let script_pubkey = &witness_stack[witness_stack_len - 1];
            let stack = &witness_stack[0..witness_stack_len - 1];
            let exec_script = sha256(script_pubkey);

            if exec_script.as_bytes() != &witness_program[0..32] {
                return Err(Error::WitnessProgramMismatch);
            }

            (
                stack.iter().cloned().collect::<Vec<_>>().into(),
                Script::new(script_pubkey.clone()),
            )
        }
        20 => {
            if witness_stack_len != 2 {
                return Err(Error::WitnessProgramMismatch);
            }

            let exec_script = Builder::default()
                .push_opcode(Opcode::OP_DUP)
                .push_opcode(Opcode::OP_HASH160)
                .push_data(witness_program)
                .push_opcode(Opcode::OP_EQUALVERIFY)
                .push_opcode(Opcode::OP_CHECKSIG)
                .into_script();

            (witness_stack.clone().into(), exec_script)
        }
        _ => return Err(Error::WitnessProgramWrongLength),
    };

    if stack.iter().any(|s| s.len() > MAX_SCRIPT_ELEMENT_SIZE) {
        return Err(Error::PushSize);
    }

    if !eval_script(
        &mut stack,
        &script_pubkey,
        flags,
        checker,
        SignatureVersion::WitnessV0,
    )? {
        return Ok(false);
    }

    if stack.len() != 1 {
        return Err(Error::EvalFalse);
    }

    let success = cast_to_bool(
        stack
            .last()
            .expect("stack.len() == 1; last() only returns errors when stack is empty; qed"),
    );
    Ok(success)
}

/// Verify Witness v1 (Taproot)
fn verify_witnessv1_program(
    witness: &ScriptWitness,
    witness_version: u8,
    witness_program: &[u8],
    flags: &VerificationFlags,
    checker: &dyn SignatureChecker,
) -> Result<bool, Error> {
    let witness_stack = witness;
    let witness_stack_len = witness_stack.len();
    let mut execdata = ScriptExecutionData::default();

    if witness_version == 0 {
        // BIP141 P2WSH: 32-byte witness v0 program (which encodes SHA256(script))
        if witness_program.len() == 32 {
            if witness_stack_len == 0 {
                return Err(Error::WitnessProgramWitnessEmpty);
            }

            let script_pubkey = &witness_stack[witness_stack_len - 1];
            let stack = &witness_stack[0..witness_stack_len - 1];
            let exec_script = sha256(script_pubkey);

            if exec_script.as_bytes() != &witness_program[0..32] {
                return Err(Error::WitnessProgramMismatch);
            }

            let (mut stack, script_pubkey): (Stack<_>, Script) = (
                stack.iter().cloned().collect::<Vec<_>>().into(),
                Script::new(script_pubkey.clone()),
            );
            execute_witness_script(
                &mut stack,
                &script_pubkey,
                flags,
                checker,
                SignatureVersion::WitnessV0,
            )
        }
        // BIP141 P2WPKH: 20-byte witness v0 program (which encodes Hash160(pubkey))
        else if witness_program.len() == 20 {
            if witness_stack_len != 2 {
                return Err(Error::WitnessProgramMismatch);
            }

            let exec_script = Builder::default()
                .push_opcode(Opcode::OP_DUP)
                .push_opcode(Opcode::OP_HASH160)
                .push_data(witness_program)
                .push_opcode(Opcode::OP_EQUALVERIFY)
                .push_opcode(Opcode::OP_CHECKSIG)
                .into_script();
            let mut stack = witness_stack.clone().into();
            execute_witness_script(
                &mut stack,
                &exec_script,
                flags,
                checker,
                SignatureVersion::WitnessV0,
            )
        } else {
            Err(Error::WitnessProgramWrongLength)
        }
    }
    // Make sure the version is witnessv1 and 32 bytes long and that it is not p2sh
    // BIP341 Taproot: 32-byte non-P2SH witness v1 program (which encodes a P2C-tweaked pubkey)
    else if witness_version == 1 && witness_program.len() == 32 && !flags.verify_p2sh {
        if witness_stack_len == 0 {
            return Err(Error::WitnessProgramWitnessEmpty);
        }
        // Drop annex (this is non-standard; see IsWitnessStandard)
        let stack = if witness_stack_len >= 2
            && witness_stack.last().is_some()
            && witness_program.last() == Some(&ANNEX_TAG)
        {
            &witness_stack[0..witness_stack_len - 1]
        } else {
            &witness_stack[..]
        };

        if witness_stack_len == 1 {
            // Key path spending (stack size is 1 after removing optional annex)
            // TODO: Check Schnorr Signature
            Ok(true)
        } else {
            // Script path spending (stack size is >1 after removing optional annex)
            let control = stack.last().unwrap();
            let script = &stack[stack.len() - 1];

            if control.len() < 33 || control.len() > 4129 || (control.len() - 33) % 32 != 0 {
                // taproot control size wrong
                return Err(Error::WitnessProgramWrongLength);
            }
            Ok(true)
        }
    } else {
        if flags.verify_discourage_upgradable_witness_program {
            return Err(Error::DiscourageUpgradableWitnessProgram);
        }
        Ok(true)
    }
}

/// Evaluautes the script
#[cfg_attr(feature = "cargo-clippy", allow(match_same_arms))]
pub fn eval_script(
    stack: &mut Stack<Bytes>,
    script: &Script,
    flags: &VerificationFlags,
    checker: &dyn SignatureChecker,
    version: SignatureVersion,
) -> Result<bool, Error> {
    if script.len() > script::MAX_SCRIPT_SIZE {
        return Err(Error::ScriptSize);
    }

    let mut pc = 0;
    let mut op_count = 0;
    let mut begincode = 0;
    let mut exec_stack = Vec::<bool>::new();
    let mut altstack = Stack::<Bytes>::new();

    while pc < script.len() {
        let executing = exec_stack.iter().all(|x| *x);
        let instruction = match script.get_instruction(pc) {
            Ok(i) => i,
            Err(Error::BadOpcode) if !executing => {
                pc += 1;
                continue;
            }
            Err(err) => return Err(err),
        };
        let opcode = instruction.opcode;

        if let Some(data) = instruction.data {
            if data.len() > script::MAX_SCRIPT_ELEMENT_SIZE {
                return Err(Error::PushSize);
            }

            if executing && flags.verify_minimaldata && !check_minimal_push(data, opcode) {
                return Err(Error::Minimaldata);
            }
        }

        if opcode.is_countable() {
            op_count += 1;
            if op_count > script::MAX_OPS_PER_SCRIPT {
                return Err(Error::OpCount);
            }
        }

        if opcode.is_disabled(flags) {
            return Err(Error::DisabledOpcode(opcode));
        }

        pc += instruction.step;
        if !(executing || (Opcode::OP_IF <= opcode && opcode <= Opcode::OP_ENDIF)) {
            continue;
        }

        match opcode {
            Opcode::OP_PUSHDATA1
            | Opcode::OP_PUSHDATA2
            | Opcode::OP_PUSHDATA4
            | Opcode::OP_0
            | Opcode::OP_PUSHBYTES_1
            | Opcode::OP_PUSHBYTES_2
            | Opcode::OP_PUSHBYTES_3
            | Opcode::OP_PUSHBYTES_4
            | Opcode::OP_PUSHBYTES_5
            | Opcode::OP_PUSHBYTES_6
            | Opcode::OP_PUSHBYTES_7
            | Opcode::OP_PUSHBYTES_8
            | Opcode::OP_PUSHBYTES_9
            | Opcode::OP_PUSHBYTES_10
            | Opcode::OP_PUSHBYTES_11
            | Opcode::OP_PUSHBYTES_12
            | Opcode::OP_PUSHBYTES_13
            | Opcode::OP_PUSHBYTES_14
            | Opcode::OP_PUSHBYTES_15
            | Opcode::OP_PUSHBYTES_16
            | Opcode::OP_PUSHBYTES_17
            | Opcode::OP_PUSHBYTES_18
            | Opcode::OP_PUSHBYTES_19
            | Opcode::OP_PUSHBYTES_20
            | Opcode::OP_PUSHBYTES_21
            | Opcode::OP_PUSHBYTES_22
            | Opcode::OP_PUSHBYTES_23
            | Opcode::OP_PUSHBYTES_24
            | Opcode::OP_PUSHBYTES_25
            | Opcode::OP_PUSHBYTES_26
            | Opcode::OP_PUSHBYTES_27
            | Opcode::OP_PUSHBYTES_28
            | Opcode::OP_PUSHBYTES_29
            | Opcode::OP_PUSHBYTES_30
            | Opcode::OP_PUSHBYTES_31
            | Opcode::OP_PUSHBYTES_32
            | Opcode::OP_PUSHBYTES_33
            | Opcode::OP_PUSHBYTES_34
            | Opcode::OP_PUSHBYTES_35
            | Opcode::OP_PUSHBYTES_36
            | Opcode::OP_PUSHBYTES_37
            | Opcode::OP_PUSHBYTES_38
            | Opcode::OP_PUSHBYTES_39
            | Opcode::OP_PUSHBYTES_40
            | Opcode::OP_PUSHBYTES_41
            | Opcode::OP_PUSHBYTES_42
            | Opcode::OP_PUSHBYTES_43
            | Opcode::OP_PUSHBYTES_44
            | Opcode::OP_PUSHBYTES_45
            | Opcode::OP_PUSHBYTES_46
            | Opcode::OP_PUSHBYTES_47
            | Opcode::OP_PUSHBYTES_48
            | Opcode::OP_PUSHBYTES_49
            | Opcode::OP_PUSHBYTES_50
            | Opcode::OP_PUSHBYTES_51
            | Opcode::OP_PUSHBYTES_52
            | Opcode::OP_PUSHBYTES_53
            | Opcode::OP_PUSHBYTES_54
            | Opcode::OP_PUSHBYTES_55
            | Opcode::OP_PUSHBYTES_56
            | Opcode::OP_PUSHBYTES_57
            | Opcode::OP_PUSHBYTES_58
            | Opcode::OP_PUSHBYTES_59
            | Opcode::OP_PUSHBYTES_60
            | Opcode::OP_PUSHBYTES_61
            | Opcode::OP_PUSHBYTES_62
            | Opcode::OP_PUSHBYTES_63
            | Opcode::OP_PUSHBYTES_64
            | Opcode::OP_PUSHBYTES_65
            | Opcode::OP_PUSHBYTES_66
            | Opcode::OP_PUSHBYTES_67
            | Opcode::OP_PUSHBYTES_68
            | Opcode::OP_PUSHBYTES_69
            | Opcode::OP_PUSHBYTES_70
            | Opcode::OP_PUSHBYTES_71
            | Opcode::OP_PUSHBYTES_72
            | Opcode::OP_PUSHBYTES_73
            | Opcode::OP_PUSHBYTES_74
            | Opcode::OP_PUSHBYTES_75 => {
                if let Some(data) = instruction.data {
                    stack.push(data.to_vec().into());
                }
            }
            Opcode::OP_1NEGATE
            | Opcode::OP_1
            | Opcode::OP_2
            | Opcode::OP_3
            | Opcode::OP_4
            | Opcode::OP_5
            | Opcode::OP_6
            | Opcode::OP_7
            | Opcode::OP_8
            | Opcode::OP_9
            | Opcode::OP_10
            | Opcode::OP_11
            | Opcode::OP_12
            | Opcode::OP_13
            | Opcode::OP_14
            | Opcode::OP_15
            | Opcode::OP_16 => {
                let value = (opcode as i32).wrapping_sub(Opcode::OP_1 as i32 - 1);
                stack.push(Num::from(value).to_bytes());
            }
            Opcode::OP_CAT if flags.verify_concat => {
                let mut value_to_append = stack.pop()?;
                let value_to_update = stack.last_mut()?;
                if value_to_update.len() + value_to_append.len() > script::MAX_SCRIPT_ELEMENT_SIZE {
                    return Err(Error::PushSize);
                }
                value_to_update.append(&mut value_to_append);
            }
            // OP_SPLIT replaces OP_SUBSTR
            Opcode::OP_SUBSTR if flags.verify_split => {
                let n = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                if n.is_negative() {
                    return Err(Error::InvalidStackOperation);
                }
                let n: usize = n.into();
                let splitted_value = {
                    let value_to_split = stack.last_mut()?;
                    if n > value_to_split.len() {
                        return Err(Error::InvalidSplitRange);
                    }
                    value_to_split.split_off(n)
                };
                stack.push(splitted_value);
            }
            Opcode::OP_AND if flags.verify_and => {
                let mask = stack.pop()?;
                let mask_len = mask.len();
                let value_to_update = stack.last_mut()?;
                if mask_len != value_to_update.len() {
                    return Err(Error::InvalidOperandSize);
                }
                for (byte_to_update, byte_mask) in (*value_to_update).iter_mut().zip(mask.iter()) {
                    *byte_to_update = *byte_to_update & byte_mask;
                }
            }
            Opcode::OP_OR if flags.verify_or => {
                let mask = stack.pop()?;
                let mask_len = mask.len();
                let value_to_update = stack.last_mut()?;
                if mask_len != value_to_update.len() {
                    return Err(Error::InvalidOperandSize);
                }
                for (byte_to_update, byte_mask) in (*value_to_update).iter_mut().zip(mask.iter()) {
                    *byte_to_update = *byte_to_update | byte_mask;
                }
            }
            Opcode::OP_XOR if flags.verify_xor => {
                let mask = stack.pop()?;
                let mask_len = mask.len();
                let value_to_update = stack.last_mut()?;
                if mask_len != value_to_update.len() {
                    return Err(Error::InvalidOperandSize);
                }
                for (byte_to_update, byte_mask) in (*value_to_update).iter_mut().zip(mask.iter()) {
                    *byte_to_update = *byte_to_update ^ byte_mask;
                }
            }
            Opcode::OP_DIV if flags.verify_div => {
                let v1 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v2 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                if v2.is_zero() {
                    return Err(Error::DivisionByZero);
                }
                stack.push((v1 / v2).to_bytes());
            }
            Opcode::OP_MOD if flags.verify_mod => {
                let v1 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v2 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                if v2.is_zero() {
                    return Err(Error::DivisionByZero);
                }
                stack.push((v1 % v2).to_bytes());
            }
            // OP_BIN2NUM replaces OP_RIGHT
            Opcode::OP_RIGHT if flags.verify_bin2num => {
                let bin = stack.pop()?;
                let n = Num::minimally_encode(&bin, 4)?;
                stack.push(n.to_bytes());
            }
            // OP_NUM2BIN replaces OP_LEFT
            Opcode::OP_LEFT if flags.verify_num2bin => {
                let bin_size = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                if bin_size.is_negative() || bin_size > MAX_SCRIPT_ELEMENT_SIZE.into() {
                    return Err(Error::PushSize);
                }

                let bin_size: usize = bin_size.into();
                let num = Num::minimally_encode(&stack.pop()?, 4)?;
                let mut num = num.to_bytes();

                // check if we can fit number into array of bin_size length
                if num.len() > bin_size {
                    return Err(Error::ImpossibleEncoding);
                }

                // check if we need to extend binary repr with zero-bytes
                if num.len() < bin_size {
                    let sign_byte = num
                        .last_mut()
                        .map(|last_byte| {
                            let sign_byte = *last_byte & 0x80;
                            *last_byte = *last_byte & 0x7f;
                            sign_byte
                        })
                        .unwrap_or(0x00);

                    num.resize(bin_size - 1, 0x00);
                    num.push(sign_byte);
                }

                stack.push(num);
            }
            Opcode::OP_CAT
            | Opcode::OP_SUBSTR
            | Opcode::OP_LEFT
            | Opcode::OP_RIGHT
            | Opcode::OP_INVERT
            | Opcode::OP_AND
            | Opcode::OP_OR
            | Opcode::OP_XOR
            | Opcode::OP_2MUL
            | Opcode::OP_2DIV
            | Opcode::OP_MUL
            | Opcode::OP_DIV
            | Opcode::OP_MOD
            | Opcode::OP_LSHIFT
            | Opcode::OP_RSHIFT => {
                return Err(Error::DisabledOpcode(opcode));
            }
            Opcode::OP_NOP => (),
            Opcode::OP_CHECKLOCKTIMEVERIFY => {
                if flags.verify_locktime {
                    // Note that elsewhere numeric opcodes are limited to
                    // operands in the range -2**31+1 to 2**31-1, however it is
                    // legal for opcodes to produce results exceeding that
                    // range. This limitation is implemented by CScriptNum's
                    // default 4-byte limit.
                    //
                    // If we kept to that limit we'd have a year 2038 problem,
                    // even though the nLockTime field in transactions
                    // themselves is uint32 which only becomes meaningless
                    // after the year 2106.
                    //
                    // Thus as a special case we tell CScriptNum to accept up
                    // to 5-byte bignums, which are good until 2**39-1, well
                    // beyond the 2**32-1 limit of the nLockTime field itself.
                    let lock_time = Num::from_slice(stack.last()?, flags.verify_minimaldata, 5)?;

                    // In the rare event that the argument may be < 0 due to
                    // some arithmetic being done first, you can always use
                    // 0 MAX CHECKLOCKTIMEVERIFY.
                    if lock_time.is_negative() {
                        return Err(Error::NegativeLocktime);
                    }

                    if !checker.check_lock_time(lock_time) {
                        return Err(Error::UnsatisfiedLocktime);
                    }
                } else if flags.verify_discourage_upgradable_nops {
                    return Err(Error::DiscourageUpgradableNops);
                }
            }
            Opcode::OP_CHECKSEQUENCEVERIFY => {
                if flags.verify_checksequence {
                    let sequence = Num::from_slice(stack.last()?, flags.verify_minimaldata, 5)?;

                    if sequence.is_negative() {
                        return Err(Error::NegativeLocktime);
                    }

                    if (sequence & (SEQUENCE_LOCKTIME_DISABLE_FLAG as i64).into()).is_zero()
                        && !checker.check_sequence(sequence)
                    {
                        return Err(Error::UnsatisfiedLocktime);
                    }
                } else if flags.verify_discourage_upgradable_nops {
                    return Err(Error::DiscourageUpgradableNops);
                }
            }
            Opcode::OP_NOP1
            | Opcode::OP_NOP4
            | Opcode::OP_NOP5
            | Opcode::OP_NOP6
            | Opcode::OP_NOP7
            | Opcode::OP_NOP8
            | Opcode::OP_NOP9
            | Opcode::OP_NOP10 => {
                if flags.verify_discourage_upgradable_nops {
                    return Err(Error::DiscourageUpgradableNops);
                }
            }
            Opcode::OP_IF | Opcode::OP_NOTIF => {
                let mut exec_value = false;
                if executing {
                    exec_value =
                        cast_to_bool(&stack.pop().map_err(|_| Error::UnbalancedConditional)?);
                    if opcode == Opcode::OP_NOTIF {
                        exec_value = !exec_value;
                    }
                }
                exec_stack.push(exec_value);
            }
            Opcode::OP_ELSE => {
                if exec_stack.is_empty() {
                    return Err(Error::UnbalancedConditional);
                }
                let last_index = exec_stack.len() - 1;
                let last = exec_stack[last_index];
                exec_stack[last_index] = !last;
            }
            Opcode::OP_ENDIF => {
                if exec_stack.is_empty() {
                    return Err(Error::UnbalancedConditional);
                }
                exec_stack.pop();
            }
            Opcode::OP_VERIFY => {
                let exec_value = cast_to_bool(&stack.pop()?);
                if !exec_value {
                    return Err(Error::Verify);
                }
            }
            Opcode::OP_RETURN => {
                return Err(Error::ReturnOpcode);
            }
            Opcode::OP_TOALTSTACK => {
                altstack.push(stack.pop()?);
            }
            Opcode::OP_FROMALTSTACK => {
                stack.push(
                    altstack
                        .pop()
                        .map_err(|_| Error::InvalidAltstackOperation)?,
                );
            }
            Opcode::OP_2DROP => {
                stack.drop(2)?;
            }
            Opcode::OP_2DUP => {
                stack.dup(2)?;
            }
            Opcode::OP_3DUP => {
                stack.dup(3)?;
            }
            Opcode::OP_2OVER => {
                stack.over(2)?;
            }
            Opcode::OP_2ROT => {
                stack.rot(2)?;
            }
            Opcode::OP_2SWAP => {
                stack.swap(2)?;
            }
            Opcode::OP_IFDUP => {
                if cast_to_bool(stack.last()?) {
                    stack.dup(1)?;
                }
            }
            Opcode::OP_DEPTH => {
                let depth = Num::from(stack.len());
                stack.push(depth.to_bytes());
            }
            Opcode::OP_DROP => {
                stack.pop()?;
            }
            Opcode::OP_DUP => {
                stack.dup(1)?;
            }
            Opcode::OP_NIP => {
                stack.nip()?;
            }
            Opcode::OP_OVER => {
                stack.over(1)?;
            }
            Opcode::OP_PICK | Opcode::OP_ROLL => {
                let n: i64 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?.into();
                if n < 0 || n >= stack.len() as i64 {
                    return Err(Error::InvalidStackOperation);
                }

                let v = match opcode {
                    Opcode::OP_PICK => stack.top(n as usize)?.clone(),
                    _ => stack.remove(n as usize)?,
                };

                stack.push(v);
            }
            Opcode::OP_ROT => {
                stack.rot(1)?;
            }
            Opcode::OP_SWAP => {
                stack.swap(1)?;
            }
            Opcode::OP_TUCK => {
                stack.tuck()?;
            }
            Opcode::OP_SIZE => {
                let n = Num::from(stack.last()?.len());
                stack.push(n.to_bytes());
            }
            Opcode::OP_EQUAL => {
                let v1 = stack.pop()?;
                let v2 = stack.pop()?;
                if v1 == v2 {
                    stack.push(vec![1].into());
                } else {
                    stack.push(Bytes::new());
                }
            }
            Opcode::OP_EQUALVERIFY => {
                let equal = stack.pop()? == stack.pop()?;
                if !equal {
                    return Err(Error::EqualVerify);
                }
            }
            Opcode::OP_1ADD => {
                let n = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)? + 1.into();
                stack.push(n.to_bytes());
            }
            Opcode::OP_1SUB => {
                let n = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)? - 1.into();
                stack.push(n.to_bytes());
            }
            Opcode::OP_NEGATE => {
                let n = -Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                stack.push(n.to_bytes());
            }
            Opcode::OP_ABS => {
                let n = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?.abs();
                stack.push(n.to_bytes());
            }
            Opcode::OP_NOT => {
                let n = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?.is_zero();
                let n = Num::from(n);
                stack.push(n.to_bytes());
            }
            Opcode::OP_0NOTEQUAL => {
                let n = !Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?.is_zero();
                let n = Num::from(n);
                stack.push(n.to_bytes());
            }
            Opcode::OP_ADD => {
                let v1 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v2 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                stack.push((v1 + v2).to_bytes());
            }
            Opcode::OP_SUB => {
                let v1 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v2 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                stack.push((v2 - v1).to_bytes());
            }
            Opcode::OP_BOOLAND => {
                let v1 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v2 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v = Num::from(!v1.is_zero() && !v2.is_zero());
                stack.push(v.to_bytes());
            }
            Opcode::OP_BOOLOR => {
                let v1 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v2 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v = Num::from(!v1.is_zero() || !v2.is_zero());
                stack.push(v.to_bytes());
            }
            Opcode::OP_NUMEQUAL => {
                let v1 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v2 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v = Num::from(v1 == v2);
                stack.push(v.to_bytes());
            }
            Opcode::OP_NUMEQUALVERIFY => {
                let v1 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v2 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                if v1 != v2 {
                    return Err(Error::NumEqualVerify);
                }
            }
            Opcode::OP_NUMNOTEQUAL => {
                let v1 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v2 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v = Num::from(v1 != v2);
                stack.push(v.to_bytes());
            }
            Opcode::OP_LESSTHAN => {
                let v1 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v2 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v = Num::from(v1 > v2);
                stack.push(v.to_bytes());
            }
            Opcode::OP_GREATERTHAN => {
                let v1 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v2 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v = Num::from(v1 < v2);
                stack.push(v.to_bytes());
            }
            Opcode::OP_LESSTHANOREQUAL => {
                let v1 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v2 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v = Num::from(v1 >= v2);
                stack.push(v.to_bytes());
            }
            Opcode::OP_GREATERTHANOREQUAL => {
                let v1 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v2 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v = Num::from(v1 <= v2);
                stack.push(v.to_bytes());
            }
            Opcode::OP_MIN => {
                let v1 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v2 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                stack.push(cmp::min(v1, v2).to_bytes());
            }
            Opcode::OP_MAX => {
                let v1 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v2 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                stack.push(cmp::max(v1, v2).to_bytes());
            }
            Opcode::OP_WITHIN => {
                let v1 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v2 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                let v3 = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                if v2 <= v3 && v3 < v1 {
                    stack.push(vec![1].into());
                } else {
                    stack.push(Bytes::new());
                }
            }
            Opcode::OP_RIPEMD160 => {
                let v = ripemd160(&stack.pop()?);
                stack.push(v.as_bytes().into());
            }
            Opcode::OP_SHA1 => {
                let v = sha1(&stack.pop()?);
                stack.push(v.as_bytes().into());
            }
            Opcode::OP_SHA256 => {
                let v = sha256(&stack.pop()?);
                stack.push(v.as_bytes().into());
            }
            Opcode::OP_HASH160 => {
                let v = dhash160(&stack.pop()?);
                stack.push(v.as_bytes().into());
            }
            Opcode::OP_HASH256 => {
                let v = dhash256(&stack.pop()?);
                stack.push(v.as_bytes().into());
            }
            Opcode::OP_CODESEPARATOR => {
                begincode = pc;
            }
            Opcode::OP_CHECKSIG | Opcode::OP_CHECKSIGVERIFY => {
                let pubkey = stack.pop()?;
                let signature = stack.pop()?;
                let sighash = parse_hash_type(version, &signature);
                let mut subscript = script.subscript(begincode);
                match version {
                    SignatureVersion::ForkId if sighash.fork_id => (),
                    SignatureVersion::WitnessV0 => (),
                    SignatureVersion::Base | SignatureVersion::ForkId => {
                        let signature_script =
                            Builder::default().push_data(&*signature).into_script();
                        subscript = subscript.find_and_delete(&*signature_script);
                    }
                    SignatureVersion::Taproot => todo!(),
                    SignatureVersion::TapScript => todo!(),
                }

                check_signature_encoding(&signature, flags, version)?;
                check_pubkey_encoding(&pubkey, flags)?;

                let success = check_signature(checker, &signature, &pubkey, &subscript, version);
                match opcode {
                    Opcode::OP_CHECKSIG => {
                        if success {
                            stack.push(vec![1].into());
                        } else {
                            stack.push(Bytes::new());
                        }
                    }
                    Opcode::OP_CHECKSIGVERIFY if !success => {
                        return Err(Error::CheckSigVerify);
                    }
                    _ => {}
                }
            }
            Opcode::OP_CHECKMULTISIG | Opcode::OP_CHECKMULTISIGVERIFY => {
                let keys_count = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                if keys_count < 0.into() || keys_count > script::MAX_PUBKEYS_PER_MULTISIG.into() {
                    return Err(Error::PubkeyCount);
                }

                let keys_count: usize = keys_count.into();
                let keys = (0..keys_count)
                    .into_iter()
                    .map(|_| stack.pop())
                    .collect::<Result<Vec<_>, _>>()?;

                let sigs_count = Num::from_slice(&stack.pop()?, flags.verify_minimaldata, 4)?;
                if sigs_count < 0.into() || sigs_count > keys_count.into() {
                    return Err(Error::SigCount);
                }

                let sigs_count: usize = sigs_count.into();
                let sigs = (0..sigs_count)
                    .into_iter()
                    .map(|_| stack.pop())
                    .collect::<Result<Vec<_>, _>>()?;

                let mut subscript = script.subscript(begincode);

                for signature in &sigs {
                    let sighash = parse_hash_type(version, &signature);
                    match version {
                        SignatureVersion::ForkId if sighash.fork_id => (),
                        SignatureVersion::WitnessV0 => (),
                        SignatureVersion::Base | SignatureVersion::ForkId => {
                            let signature_script =
                                Builder::default().push_data(&*signature).into_script();
                            subscript = subscript.find_and_delete(&*signature_script);
                        }
                        SignatureVersion::Taproot => todo!(),
                        SignatureVersion::TapScript => todo!(),
                    }
                }

                let mut success = true;
                let mut k = 0;
                let mut s = 0;
                while s < sigs.len() && success {
                    let key = &keys[k];
                    let sig = &sigs[s];

                    check_signature_encoding(sig, flags, version)?;
                    check_pubkey_encoding(key, flags)?;

                    let ok = check_signature(checker, sig, key, &subscript, version);
                    if ok {
                        s += 1;
                    }
                    k += 1;

                    success = sigs.len() - s <= keys.len() - k;
                }

                if !stack.pop()?.is_empty() && flags.verify_nulldummy {
                    return Err(Error::SignatureNullDummy);
                }

                match opcode {
                    Opcode::OP_CHECKMULTISIG => {
                        if success {
                            stack.push(vec![1].into());
                        } else {
                            stack.push(Bytes::new());
                        }
                    }
                    Opcode::OP_CHECKMULTISIGVERIFY if !success => {
                        return Err(Error::CheckSigVerify);
                    }
                    _ => {}
                }
            }
            Opcode::OP_RESERVED | Opcode::OP_VER | Opcode::OP_RESERVED1 | Opcode::OP_RESERVED2 => {
                if executing {
                    return Err(Error::DisabledOpcode(opcode));
                }
            }
            Opcode::OP_VERIF | Opcode::OP_VERNOTIF => {
                return Err(Error::DisabledOpcode(opcode));
            }
            _ => todo!(),
        }

        if stack.len() + altstack.len() > 1000 {
            return Err(Error::StackSize);
        }
    }

    if !exec_stack.is_empty() {
        return Err(Error::UnbalancedConditional);
    }

    let success = !stack.is_empty() && {
        let last = stack.last()?;
        cast_to_bool(last)
    };

    Ok(success)
}

#[cfg(test)]
mod tests {
    use light_bitcoin_chain::{
        h256_rev, Bytes, OutPoint, Transaction, TransactionInput, TransactionOutput,
    };
    use light_bitcoin_keys::{KeyPair, Network, Private};

    use crate::{
        interpreter::verify_script, Builder, Error, Opcode, Script, ScriptWitness,
        SignatureVersion, TransactionInputSigner, TransactionSignatureChecker,
        UnsignedTransactionInput, VerificationFlags,
    };

    // https://blockchain.info/rawtx/3f285f083de7c0acabd9f106a43ec42687ab0bebe2e6f0d529db696794540fea
    #[test]
    fn test_check_transaction_signature() {
        let tx: Transaction = "0100000001484d40d45b9ea0d652fca8258ab7caa42541eb52975857f96fb50cd732c8b481000000008a47304402202cb265bf10707bf49346c3515dd3d16fc454618c58ec0a0ff448a676c54ff71302206c6624d762a1fcef4618284ead8f08678ac05b13c84235f1654e6ad168233e8201410414e301b2328f17442c0b8310d787bf3d8a404cfbd0704f135b6ad4b2d3ee751310f981926e53a6e8c39bd7d3fefd576c543cce493cbac06388f2651d1aacbfcdffffffff0162640100000000001976a914c8e90996c7c6080ee06284600c684ed904d14c5c88ac00000000".parse().unwrap();
        let signer: TransactionInputSigner = tx.into();
        let checker = TransactionSignatureChecker {
            signer,
            input_index: 0,
            input_amount: 0,
        };
        let input: Script = "47304402202cb265bf10707bf49346c3515dd3d16fc454618c58ec0a0ff448a676c54ff71302206c6624d762a1fcef4618284ead8f08678ac05b13c84235f1654e6ad168233e8201410414e301b2328f17442c0b8310d787bf3d8a404cfbd0704f135b6ad4b2d3ee751310f981926e53a6e8c39bd7d3fefd576c543cce493cbac06388f2651d1aacbfcd".parse().unwrap();
        let output: Script = "76a914df3bd30160e6c6145baaf2c88a8844c13a00d1d588ac"
            .parse()
            .unwrap();
        let flags = VerificationFlags::default().verify_p2sh(true);
        assert_eq!(
            verify_script(
                &input,
                &output,
                &ScriptWitness::default(),
                &flags,
                &checker,
                SignatureVersion::Base
            ),
            Ok(())
        );
    }

    // https://blockchain.info/rawtx/02b082113e35d5386285094c2829e7e2963fa0b5369fb7f4b79c4c90877dcd3d
    #[test]
    fn test_check_transaction_multisig() {
        let tx: Transaction = "01000000013dcd7d87904c9cb7f4b79f36b5a03f96e2e729284c09856238d5353e1182b00200000000fd5e0100483045022100deeb1f13b5927b5e32d877f3c42a4b028e2e0ce5010fdb4e7f7b5e2921c1dcd2022068631cb285e8c1be9f061d2968a18c3163b780656f30a049effee640e80d9bff01483045022100ee80e164622c64507d243bd949217d666d8b16486e153ac6a1f8e04c351b71a502203691bef46236ca2b4f5e60a82a853a33d6712d6a1e7bf9a65e575aeb7328db8c014cc9524104a882d414e478039cd5b52a92ffb13dd5e6bd4515497439dffd691a0f12af9575fa349b5694ed3155b136f09e63975a1700c9f4d4df849323dac06cf3bd6458cd41046ce31db9bdd543e72fe3039a1f1c047dab87037c36a669ff90e28da1848f640de68c2fe913d363a51154a0c62d7adea1b822d05035077418267b1a1379790187410411ffd36c70776538d079fbae117dc38effafb33304af83ce4894589747aee1ef992f63280567f52f5ba870678b4ab4ff6c8ea600bd217870a8b4f1f09f3a8e8353aeffffffff0130d90000000000001976a914569076ba39fc4ff6a2291d9ea9196d8c08f9c7ab88ac00000000".parse().unwrap();
        let signer: TransactionInputSigner = tx.into();
        let checker = TransactionSignatureChecker {
            signer,
            input_index: 0,
            input_amount: 0,
        };
        let input: Script = "00483045022100deeb1f13b5927b5e32d877f3c42a4b028e2e0ce5010fdb4e7f7b5e2921c1dcd2022068631cb285e8c1be9f061d2968a18c3163b780656f30a049effee640e80d9bff01483045022100ee80e164622c64507d243bd949217d666d8b16486e153ac6a1f8e04c351b71a502203691bef46236ca2b4f5e60a82a853a33d6712d6a1e7bf9a65e575aeb7328db8c014cc9524104a882d414e478039cd5b52a92ffb13dd5e6bd4515497439dffd691a0f12af9575fa349b5694ed3155b136f09e63975a1700c9f4d4df849323dac06cf3bd6458cd41046ce31db9bdd543e72fe3039a1f1c047dab87037c36a669ff90e28da1848f640de68c2fe913d363a51154a0c62d7adea1b822d05035077418267b1a1379790187410411ffd36c70776538d079fbae117dc38effafb33304af83ce4894589747aee1ef992f63280567f52f5ba870678b4ab4ff6c8ea600bd217870a8b4f1f09f3a8e8353ae".parse().unwrap();
        let output: Script = "a9141a8b0026343166625c7475f01e48b5ede8c0252e87"
            .parse()
            .unwrap();
        let flags = VerificationFlags::default().verify_p2sh(true);
        assert_eq!(
            verify_script(
                &input,
                &output,
                &ScriptWitness::default(),
                &flags,
                &checker,
                SignatureVersion::Base
            ),
            Ok(())
        );
    }

    // https://blockchain.info/en/tx/12b5633bad1f9c167d523ad1aa1947b2732a865bf5414eab2f9e5ae5d5c191ba?show_adv=true
    #[test]
    fn test_transaction_with_high_s_signature() {
        let tx: Transaction = "010000000173805864da01f15093f7837607ab8be7c3705e29a9d4a12c9116d709f8911e590100000049483045022052ffc1929a2d8bd365c6a2a4e3421711b4b1e1b8781698ca9075807b4227abcb0221009984107ddb9e3813782b095d0d84361ed4c76e5edaf6561d252ae162c2341cfb01ffffffff0200e1f50500000000434104baa9d36653155627c740b3409a734d4eaf5dcca9fb4f736622ee18efcf0aec2b758b2ec40db18fbae708f691edb2d4a2a3775eb413d16e2e3c0f8d4c69119fd1ac009ce4a60000000043410411db93e1dcdb8a016b49840f8c53bc1eb68a382e97b1482ecad7b148a6909a5cb2e0eaddfb84ccf9744464f82e160bfa9b8b64f9d4c03f999b8643f656b412a3ac00000000".parse().unwrap();
        let signer: TransactionInputSigner = tx.into();
        let checker = TransactionSignatureChecker {
            signer,
            input_index: 0,
            input_amount: 0,
        };
        let input: Script = "483045022052ffc1929a2d8bd365c6a2a4e3421711b4b1e1b8781698ca9075807b4227abcb0221009984107ddb9e3813782b095d0d84361ed4c76e5edaf6561d252ae162c2341cfb01".parse().unwrap();
        let output: Script = "410411db93e1dcdb8a016b49840f8c53bc1eb68a382e97b1482ecad7b148a6909a5cb2e0eaddfb84ccf9744464f82e160bfa9b8b64f9d4c03f999b8643f656b412a3ac".parse().unwrap();
        let flags = VerificationFlags::default().verify_p2sh(true);
        assert_eq!(
            verify_script(
                &input,
                &output,
                &ScriptWitness::default(),
                &flags,
                &checker,
                SignatureVersion::Base
            ),
            Ok(())
        );
    }

    // https://blockchain.info/rawtx/fb0a1d8d34fa5537e461ac384bac761125e1bfa7fec286fa72511240fa66864d
    #[test]
    fn test_transaction_from_124276() {
        let tx: Transaction = "01000000012316aac445c13ff31af5f3d1e2cebcada83e54ba10d15e01f49ec28bddc285aa000000008e4b3048022200002b83d59c1d23c08efd82ee0662fec23309c3adbcbd1f0b8695378db4b14e736602220000334a96676e58b1bb01784cb7c556dd8ce1c220171904da22e18fe1e7d1510db5014104d0fe07ff74c9ef5b00fed1104fad43ecf72dbab9e60733e4f56eacf24b20cf3b8cd945bcabcc73ba0158bf9ce769d43e94bd58c5c7e331a188922b3fe9ca1f5affffffff01c0c62d00000000001976a9147a2a3b481ca80c4ba7939c54d9278e50189d94f988ac00000000".parse().unwrap();
        let signer: TransactionInputSigner = tx.into();
        let checker = TransactionSignatureChecker {
            signer,
            input_index: 0,
            input_amount: 0,
        };
        let input: Script = "4b3048022200002b83d59c1d23c08efd82ee0662fec23309c3adbcbd1f0b8695378db4b14e736602220000334a96676e58b1bb01784cb7c556dd8ce1c220171904da22e18fe1e7d1510db5014104d0fe07ff74c9ef5b00fed1104fad43ecf72dbab9e60733e4f56eacf24b20cf3b8cd945bcabcc73ba0158bf9ce769d43e94bd58c5c7e331a188922b3fe9ca1f5a".parse().unwrap();
        let output: Script = "76a9147a2a3b481ca80c4ba7939c54d9278e50189d94f988ac"
            .parse()
            .unwrap();
        let flags = VerificationFlags::default().verify_p2sh(true);
        assert_eq!(
            verify_script(
                &input,
                &output,
                &ScriptWitness::default(),
                &flags,
                &checker,
                SignatureVersion::Base
            ),
            Ok(())
        );
    }

    // https://blockchain.info/rawtx/eb3b82c0884e3efa6d8b0be55b4915eb20be124c9766245bcc7f34fdac32bccb
    #[test]
    fn test_transaction_bip65() {
        let tx: Transaction = "01000000024de8b0c4c2582db95fa6b3567a989b664484c7ad6672c85a3da413773e63fdb8000000006b48304502205b282fbc9b064f3bc823a23edcc0048cbb174754e7aa742e3c9f483ebe02911c022100e4b0b3a117d36cab5a67404dddbf43db7bea3c1530e0fe128ebc15621bd69a3b0121035aa98d5f77cd9a2d88710e6fc66212aff820026f0dad8f32d1f7ce87457dde50ffffffff4de8b0c4c2582db95fa6b3567a989b664484c7ad6672c85a3da413773e63fdb8010000006f004730440220276d6dad3defa37b5f81add3992d510d2f44a317fd85e04f93a1e2daea64660202200f862a0da684249322ceb8ed842fb8c859c0cb94c81e1c5308b4868157a428ee01ab51210232abdc893e7f0631364d7fd01cb33d24da45329a00357b3a7886211ab414d55a51aeffffffff02e0fd1c00000000001976a914380cb3c594de4e7e9b8e18db182987bebb5a4f7088acc0c62d000000000017142a9bc5447d664c1d0141392a842d23dba45c4f13b17500000000".parse().unwrap();
        let signer: TransactionInputSigner = tx.into();
        let checker = TransactionSignatureChecker {
            signer,
            input_index: 1,
            input_amount: 0,
        };
        let input: Script = "004730440220276d6dad3defa37b5f81add3992d510d2f44a317fd85e04f93a1e2daea64660202200f862a0da684249322ceb8ed842fb8c859c0cb94c81e1c5308b4868157a428ee01ab51210232abdc893e7f0631364d7fd01cb33d24da45329a00357b3a7886211ab414d55a51ae".parse().unwrap();
        let output: Script = "142a9bc5447d664c1d0141392a842d23dba45c4f13b175"
            .parse()
            .unwrap();

        let flags = VerificationFlags::default().verify_p2sh(true);
        assert_eq!(
            verify_script(
                &input,
                &output,
                &ScriptWitness::default(),
                &flags,
                &checker,
                SignatureVersion::Base
            ),
            Ok(())
        );

        let flags = VerificationFlags::default()
            .verify_p2sh(true)
            .verify_locktime(true);
        assert_eq!(
            verify_script(
                &input,
                &output,
                &ScriptWitness::default(),
                &flags,
                &checker,
                SignatureVersion::Base
            ),
            Err(Error::NumberOverflow)
        );
    }

    // https://blockchain.info/rawtx/54fabd73f1d20c980a0686bf0035078e07f69c58437e4d586fb29aa0bee9814f
    #[test]
    fn test_arithmetic_correct_arguments_order() {
        let tx: Transaction = "01000000010c0e314bd7bb14721b3cfd8e487cd6866173354f87ca2cf4d13c8d3feb4301a6000000004a483045022100d92e4b61452d91a473a43cde4b469a472467c0ba0cbd5ebba0834e4f4762810402204802b76b7783db57ac1f61d2992799810e173e91055938750815b6d8a675902e014fffffffff0140548900000000001976a914a86e8ee2a05a44613904e18132e49b2448adc4e688ac00000000".parse().unwrap();
        let signer: TransactionInputSigner = tx.into();
        let checker = TransactionSignatureChecker {
            signer,
            input_index: 0,
            input_amount: 0,
        };
        let input: Script = "483045022100d92e4b61452d91a473a43cde4b469a472467c0ba0cbd5ebba0834e4f4762810402204802b76b7783db57ac1f61d2992799810e173e91055938750815b6d8a675902e014f".parse().unwrap();
        let output: Script = "76009f69905160a56b210378d430274f8c5ec1321338151e9f27f4c676a008bdf8638d07c0b6be9ab35c71ad6c".parse().unwrap();
        let flags = VerificationFlags::default();
        assert_eq!(
            verify_script(
                &input,
                &output,
                &ScriptWitness::default(),
                &flags,
                &checker,
                SignatureVersion::Base
            ),
            Ok(())
        );
    }

    // https://webbtc.com/tx/5df1375ffe61ac35ca178ebb0cab9ea26dedbd0e96005dfcee7e379fa513232f
    #[test]
    fn test_transaction_find_and_delete() {
        let tx: Transaction = "0100000002f9cbafc519425637ba4227f8d0a0b7160b4e65168193d5af39747891de98b5b5000000006b4830450221008dd619c563e527c47d9bd53534a770b102e40faa87f61433580e04e271ef2f960220029886434e18122b53d5decd25f1f4acb2480659fea20aabd856987ba3c3907e0121022b78b756e2258af13779c1a1f37ea6800259716ca4b7f0b87610e0bf3ab52a01ffffffff42e7988254800876b69f24676b3e0205b77be476512ca4d970707dd5c60598ab00000000fd260100483045022015bd0139bcccf990a6af6ec5c1c52ed8222e03a0d51c334df139968525d2fcd20221009f9efe325476eb64c3958e4713e9eefe49bf1d820ed58d2112721b134e2a1a53034930460221008431bdfa72bc67f9d41fe72e94c88fb8f359ffa30b33c72c121c5a877d922e1002210089ef5fc22dd8bfc6bf9ffdb01a9862d27687d424d1fefbab9e9c7176844a187a014c9052483045022015bd0139bcccf990a6af6ec5c1c52ed8222e03a0d51c334df139968525d2fcd20221009f9efe325476eb64c3958e4713e9eefe49bf1d820ed58d2112721b134e2a1a5303210378d430274f8c5ec1321338151e9f27f4c676a008bdf8638d07c0b6be9ab35c71210378d430274f8c5ec1321338151e9f27f4c676a008bdf8638d07c0b6be9ab35c7153aeffffffff01a08601000000000017a914d8dacdadb7462ae15cd906f1878706d0da8660e68700000000".parse().unwrap();
        let signer: TransactionInputSigner = tx.into();
        let checker = TransactionSignatureChecker {
            signer,
            input_index: 1,
            input_amount: 0,
        };
        let input: Script = "00483045022015BD0139BCCCF990A6AF6EC5C1C52ED8222E03A0D51C334DF139968525D2FCD20221009F9EFE325476EB64C3958E4713E9EEFE49BF1D820ED58D2112721B134E2A1A53034930460221008431BDFA72BC67F9D41FE72E94C88FB8F359FFA30B33C72C121C5A877D922E1002210089EF5FC22DD8BFC6BF9FFDB01A9862D27687D424D1FEFBAB9E9C7176844A187A014C9052483045022015BD0139BCCCF990A6AF6EC5C1C52ED8222E03A0D51C334DF139968525D2FCD20221009F9EFE325476EB64C3958E4713E9EEFE49BF1D820ED58D2112721B134E2A1A5303210378D430274F8C5EC1321338151E9F27F4C676A008BDF8638D07C0B6BE9AB35C71210378D430274F8C5EC1321338151E9F27F4C676A008BDF8638D07C0B6BE9AB35C7153AE".parse().unwrap();
        let output: Script = "A914D8DACDADB7462AE15CD906F1878706D0DA8660E687"
            .parse()
            .unwrap();

        let flags = VerificationFlags::default().verify_p2sh(true);
        assert_eq!(
            verify_script(
                &input,
                &output,
                &ScriptWitness::default(),
                &flags,
                &checker,
                SignatureVersion::Base
            ),
            Ok(())
        );
    }

    #[test]
    fn test_script_with_forkid_signature() {
        let key_pair = KeyPair::from_private(Private {
            network: Network::Mainnet,
            secret: h256_rev("1"),
            compressed: false,
        })
        .unwrap();
        let redeem_script = Builder::default()
            .push_data(key_pair.public())
            .push_opcode(Opcode::OP_CHECKSIG)
            .into_script();

        let amount = 12345000000000;
        let sighashtype = 0x41; // All + ForkId
        let checker = TransactionSignatureChecker {
            input_index: 0,
            input_amount: amount,
            signer: TransactionInputSigner {
                version: 1,
                inputs: vec![UnsignedTransactionInput {
                    previous_output: OutPoint {
                        txid: h256_rev("0"),
                        index: 0xffffffff,
                    },
                    sequence: 0xffffffff,
                }],
                outputs: vec![TransactionOutput {
                    value: amount,
                    script_pubkey: redeem_script.to_bytes(),
                }],
                lock_time: 0,
            },
        };

        let script_pubkey = redeem_script;
        let flags = VerificationFlags::default();

        // valid signature
        {
            let signed_input = checker.signer.signed_input(
                &key_pair,
                0,
                amount,
                &script_pubkey,
                SignatureVersion::ForkId,
                sighashtype,
            );
            let script_sig = signed_input.script_sig.into();

            assert_eq!(
                verify_script(
                    &script_sig,
                    &script_pubkey,
                    &ScriptWitness::default(),
                    &flags,
                    &checker,
                    SignatureVersion::ForkId
                ),
                Ok(())
            );
        }

        // signature with wrong amount
        {
            let signed_input = checker.signer.signed_input(
                &key_pair,
                0,
                amount + 1,
                &script_pubkey,
                SignatureVersion::ForkId,
                sighashtype,
            );
            let script_sig = signed_input.script_sig.into();

            assert_eq!(
                verify_script(
                    &script_sig,
                    &script_pubkey,
                    &ScriptWitness::default(),
                    &flags,
                    &checker,
                    SignatureVersion::ForkId
                ),
                Err(Error::EvalFalse)
            );
        }

        // fork-id signature passed when not expected
        {
            let signed_input = checker.signer.signed_input(
                &key_pair,
                0,
                amount + 1,
                &script_pubkey,
                SignatureVersion::ForkId,
                sighashtype,
            );
            let script_sig = signed_input.script_sig.into();

            assert_eq!(
                verify_script(
                    &script_sig,
                    &script_pubkey,
                    &ScriptWitness::default(),
                    &flags,
                    &checker,
                    SignatureVersion::Base
                ),
                Err(Error::EvalFalse)
            );
        }

        // non-fork-id signature passed when expected
        {
            let signed_input = checker.signer.signed_input(
                &key_pair,
                0,
                amount + 1,
                &script_pubkey,
                SignatureVersion::Base,
                1,
            );
            let script_sig = signed_input.script_sig.into();

            assert_eq!(
                verify_script(
                    &script_sig,
                    &script_pubkey,
                    &ScriptWitness::default(),
                    &flags.verify_strictenc(true),
                    &checker,
                    SignatureVersion::ForkId
                ),
                Err(Error::SignatureMustUseForkId)
            );
        }
    }

    fn run_witness_test(
        script_sig: Script,
        script_pubkey: Script,
        script_witness: Vec<Bytes>,
        flags: VerificationFlags,
        amount: u64,
    ) -> Result<(), Error> {
        let tx1 = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                previous_output: OutPoint {
                    txid: Default::default(),
                    index: 0xffffffff,
                },
                script_sig: Builder::default()
                    .push_num(0.into())
                    .push_num(0.into())
                    .into_bytes(),
                sequence: 0xffffffff,
                script_witness: vec![],
            }],
            outputs: vec![TransactionOutput {
                value: amount,
                script_pubkey: script_pubkey.to_bytes(),
            }],
            lock_time: 0,
        };
        let tx2 = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                previous_output: OutPoint {
                    txid: tx1.hash(),
                    index: 0,
                },
                script_sig: script_sig.to_bytes(),
                sequence: 0xffffffff,
                script_witness: script_witness.clone(),
            }],
            outputs: vec![TransactionOutput {
                value: amount,
                script_pubkey: Builder::default().into_bytes(),
            }],
            lock_time: 0,
        };

        let checker = TransactionSignatureChecker {
            input_index: 0,
            input_amount: amount,
            signer: tx2.into(),
        };

        verify_script(
            &script_sig,
            &script_pubkey,
            &script_witness,
            &flags,
            &checker,
            SignatureVersion::Base,
        )
    }

    fn run_witness_test_tx_test(
        script_pubkey: Script,
        tx: &Transaction,
        flags: &VerificationFlags,
        amount: u64,
        index: usize,
    ) -> Result<(), Error> {
        let checker = TransactionSignatureChecker {
            input_index: index,
            input_amount: amount,
            signer: tx.clone().into(),
        };

        verify_script(
            &tx.inputs[index].script_sig.clone().into(),
            &script_pubkey,
            &tx.inputs[index].script_witness,
            flags,
            &checker,
            SignatureVersion::Base,
        )
    }

    #[test]
    fn op_equal_push_empty_bytes_to_stack() {
        // tx #95 from testnet block:
        // https://testnet.blockexplorer.com/block/00000000c7169675fc165bfeceb11b572129977ca9f9e6ca5953e3184cb403dd
        // https://tbtc.bitaps.com/raw/transaction/27c94c0ca2f66fcc09d11b510e04d21adfe19f459673029e709024d7d9a7f4b4
        // and its donor tx:
        // https://tbtc.bitaps.com/raw/transaction/25e140942ecf79d24619908185a881f95d9ecb23a7be050f7c44cd378aae26eb
        //
        // the issue was that our comparison ops implementation (OP_EQUAL, OP_WITHIN, OP_CHECKSIG, ***)
        // were pushing non-empty value (vec![0]) when comparison has failed
        // => in combination with verify_nnulldummy this caused consensus issue

        let tx: Transaction = "0100000001eb26ae8a37cd447c0f05bea723cb9e5df981a88581901946d279cf2e9440e1250000000091473044022057e887c4cb773a6ec513b285dde1209ee4213209c21bb9da9e284ffe7477979302201aba367cf84bf2c6ccfd1b18d2bec0d705e2acacfeb42324cdc0fe63fbe2524a01483045022100e3f2e5e2a0b6bb75f2a506d7b190d8ba48b1e9108dd4fc4a740fbc921d0067a3022070fccd6eec2415d6d75f7aa3d0604988ee84d856db2acde4cc01d9c43f0237a301ffffffff0100350c00000000001976a9149e2be3b4d5e7274e8fd739b09fc6fd223054616088ac00000000".parse().unwrap();
        let signer: TransactionInputSigner = tx.into();
        let checker = TransactionSignatureChecker {
            signer,
            input_index: 0,
            input_amount: 1000000,
        };
        let input: Script = "473044022057e887c4cb773a6ec513b285dde1209ee4213209c21bb9da9e284ffe7477979302201aba367cf84bf2c6ccfd1b18d2bec0d705e2acacfeb42324cdc0fe63fbe2524a01483045022100e3f2e5e2a0b6bb75f2a506d7b190d8ba48b1e9108dd4fc4a740fbc921d0067a3022070fccd6eec2415d6d75f7aa3d0604988ee84d856db2acde4cc01d9c43f0237a301".parse().unwrap();
        let output: Script = "5253877c5121027fe085933328a89d0ad069071dee3bd4c908fddc852032356a318324c9ab0f6c210321e7c9eea060c099747ddcf741e9498a2b90fe8f362e2c85370722df0f88d1782102a5bc779306b40927648e73e144d430dc1b7c0730f6a3ab5bbd130374d8fe4a5a53af2102a70faff961b367875336396076a72293bf3adaa084404f8a5cbec23f41645b87ac".parse().unwrap();
        let flags = VerificationFlags::default().verify_nulldummy(true);
        assert_eq!(
            verify_script(
                &input,
                &output,
                &ScriptWitness::default(),
                &flags,
                &checker,
                SignatureVersion::Base
            ),
            Ok(())
        );
    }
}