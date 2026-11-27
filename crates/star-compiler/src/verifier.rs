//! Bounded verifier for the emitted `.starc` version 1.0 subset.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;

use num_bigint::BigInt;
use num_traits::{One, ToPrimitive, Zero};
use sha2::{Digest, Sha256};

use crate::intrinsics::IrOp;

/// A capability required before a resource-sensitive intrinsic may be loaded.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Capability {
    FsRead,
    FsWrite,
    NetConnect,
    NetListen,
    ProcessSpawn,
    EnvRead,
    Ffi,
    Clock,
    Random,
}

impl Capability {
    pub const fn name(self) -> &'static str {
        match self {
            Self::FsRead => "fs.read",
            Self::FsWrite => "fs.write",
            Self::NetConnect => "net.connect",
            Self::NetListen => "net.listen",
            Self::ProcessSpawn => "process.spawn",
            Self::EnvRead => "env.read",
            Self::Ffi => "ffi",
            Self::Clock => "clock",
            Self::Random => "random",
        }
    }
}

/// Host-provided ceilings checked while parsing and analyzing bytecode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationLimits {
    pub max_file_bytes: usize,
    pub max_table_entries: u32,
    pub max_instructions: u64,
    pub max_stack_depth: u32,
}

/// Host authorization and resource limits for one verification operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationPolicy {
    pub limits: VerificationLimits,
    pub granted_capabilities: BTreeSet<Capability>,
}

/// A bytecode file that passed all checks in `verify_bytecode`.
#[derive(Clone, Debug)]
pub struct VerifiedProgram<'a> {
    bytes: &'a [u8],
    pub module_count: u32,
    pub entry_module_index: u32,
    pub function_count: u32,
    pub instruction_count: u64,
    pub required_stack_depth: u32,
    /// Statically required operand-stack depth for each function-table entry.
    pub function_stack_depths: Vec<u32>,
}

impl<'a> VerifiedProgram<'a> {
    /// Returns the exact validated `.starc` bytes.
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

/// A malformed file, policy violation, or unsupported bytecode feature.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationError {
    pub offset: Option<usize>,
    pub message: String,
}

impl fmt::Display for VerificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.offset {
            Some(offset) => write!(
                formatter,
                "bytecode verification failed at byte {offset}: {}",
                self.message
            ),
            None => write!(formatter, "bytecode verification failed: {}", self.message),
        }
    }
}

impl std::error::Error for VerificationError {}

fn failure(offset: Option<usize>, message: impl Into<String>) -> VerificationError {
    VerificationError {
        offset,
        message: message.into(),
    }
}

#[derive(Clone, Copy, Debug)]
struct Section {
    offset: usize,
    length: usize,
}

impl Section {
    fn end(self) -> usize {
        self.offset + self.length
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
    base: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8], base: usize) -> Self {
        Self {
            bytes,
            position: 0,
            base,
        }
    }

    fn offset(&self) -> usize {
        self.base + self.position
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], VerificationError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| failure(Some(self.offset()), "read length overflow"))?;
        let result = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| failure(Some(self.offset()), "truncated bytecode record"))?;
        self.position = end;
        Ok(result)
    }

    fn u8(&mut self) -> Result<u8, VerificationError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, VerificationError> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("fixed length"),
        ))
    }

    fn u32(&mut self) -> Result<u32, VerificationError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("fixed length"),
        ))
    }

    fn u64(&mut self) -> Result<u64, VerificationError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("fixed length"),
        ))
    }

    fn finish(&self, label: &str) -> Result<(), VerificationError> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(failure(
                Some(self.offset()),
                format!("trailing bytes in {label}"),
            ))
        }
    }

    fn uleb128(&mut self) -> Result<u64, VerificationError> {
        let start = self.position;
        let mut value = 0_u64;
        for index in 0..10 {
            let byte = self.u8()?;
            let payload = u64::from(byte & 0x7F);
            let shift = index * 7;
            if shift == 63 && payload > 1 {
                return Err(failure(
                    Some(self.base + start),
                    "ULEB128 value overflows u64",
                ));
            }
            value |= payload << shift;
            if byte & 0x80 == 0 {
                let encoded = encode_uleb128(value);
                if self.bytes[start..self.position] != encoded {
                    return Err(failure(
                        Some(self.base + start),
                        "noncanonical ULEB128 encoding",
                    ));
                }
                return Ok(value);
            }
        }
        Err(failure(
            Some(self.base + start),
            "ULEB128 encoding exceeds ten bytes",
        ))
    }

    fn sleb128(&mut self) -> Result<BigInt, VerificationError> {
        let start = self.position;
        let mut value = BigInt::zero();
        let mut shift = 0_usize;
        loop {
            let byte = self.u8()?;
            value += BigInt::from(byte & 0x7F) << shift;
            shift = shift
                .checked_add(7)
                .ok_or_else(|| failure(Some(self.base + start), "SLEB128 shift overflow"))?;
            if byte & 0x80 == 0 {
                if byte & 0x40 != 0 {
                    value -= BigInt::one() << shift;
                }
                if encode_sleb128(&value) != self.bytes[start..self.position] {
                    return Err(failure(
                        Some(self.base + start),
                        "noncanonical SLEB128 encoding",
                    ));
                }
                return Ok(value);
            }
        }
    }
}

fn encode_uleb128(mut value: u64) -> Vec<u8> {
    let mut output = Vec::new();
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            return output;
        }
    }
}

fn encode_sleb128(value: &BigInt) -> Vec<u8> {
    let mut remaining = value.clone();
    let mask = BigInt::from(0x7F_u8);
    let minus_one = BigInt::from(-1);
    let mut output = Vec::new();
    loop {
        let byte = (&remaining & &mask).to_u8().expect("masked to seven bits");
        remaining >>= 7_usize;
        let done = (remaining.is_zero() && byte & 0x40 == 0)
            || (remaining == minus_one && byte & 0x40 != 0);
        output.push(if done { byte } else { byte | 0x80 });
        if done {
            return output;
        }
    }
}

fn charge_entries(
    budget: &mut u64,
    count: u32,
    limit: u32,
    label: &str,
    offset: usize,
) -> Result<(), VerificationError> {
    *budget = budget
        .checked_add(u64::from(count))
        .ok_or_else(|| failure(Some(offset), "table entry count overflow"))?;
    if *budget > u64::from(limit) {
        return Err(failure(
            Some(offset),
            format!("aggregate table entries exceed host limit while reading {label}"),
        ));
    }
    Ok(())
}

fn parse_container<'a>(
    bytes: &'a [u8],
    limits: &VerificationLimits,
) -> Result<(HashMap<u32, Section>, u32), VerificationError> {
    if bytes.len() > limits.max_file_bytes {
        return Err(failure(None, "file exceeds host byte limit"));
    }
    let mut reader = Reader::new(bytes, 0);
    if reader.take(4)? != b"STAR" {
        return Err(failure(Some(0), "invalid .starc magic"));
    }
    let major = reader.u16()?;
    let minor = reader.u16()?;
    if major != 1 || minor != 0 {
        return Err(failure(Some(4), "only .starc version 1.0 is supported"));
    }
    if reader.u32()? != 0 {
        return Err(failure(Some(8), "version 1.0 file flags must be zero"));
    }
    let section_count = reader.u32()?;
    if section_count != 7 && section_count != 8 {
        return Err(failure(
            Some(12),
            "version 1.0 requires seven or eight sections",
        ));
    }
    let directory_length = usize::try_from(section_count)
        .ok()
        .and_then(|count| count.checked_mul(24))
        .ok_or_else(|| failure(Some(12), "section directory size overflow"))?;
    let directory_start = reader.position;
    reader.take(directory_length)?;
    let directory_end = reader.position;
    let mut directory_reader = Reader::new(&bytes[directory_start..directory_end], directory_start);
    let mut sections = HashMap::new();
    let mut previous_id = None;
    let mut expected_payload_offset = directory_end;
    for _ in 0..section_count {
        let id_offset = directory_reader.offset();
        let id = directory_reader.u32()?;
        let flags = directory_reader.u32()?;
        let payload_offset = directory_reader.u64()?;
        let payload_length = directory_reader.u64()?;
        if previous_id.is_some_and(|previous| id <= previous) {
            return Err(failure(
                Some(id_offset),
                "section IDs are not strictly increasing",
            ));
        }
        previous_id = Some(id);
        let expected_flags = u32::from(id != 7);
        if flags != expected_flags {
            return Err(failure(
                Some(id_offset + 4),
                "invalid section required/reserved flags",
            ));
        }
        if !matches!(id, 1..=7 | u32::MAX) {
            return Err(failure(Some(id_offset), "unknown section in version 1.0"));
        }
        let offset = usize::try_from(payload_offset)
            .map_err(|_| failure(Some(id_offset + 8), "section offset does not fit host"))?;
        let length = usize::try_from(payload_length)
            .map_err(|_| failure(Some(id_offset + 16), "section length does not fit host"))?;
        if offset != expected_payload_offset {
            return Err(failure(
                Some(id_offset + 8),
                "section payloads must be contiguous",
            ));
        }
        let end = offset
            .checked_add(length)
            .ok_or_else(|| failure(Some(id_offset + 8), "section range overflow"))?;
        if end > bytes.len() {
            return Err(failure(
                Some(id_offset + 8),
                "section payload exceeds file bounds",
            ));
        }
        if id == u32::MAX && length != 32 {
            return Err(failure(Some(offset), "Checksum section must be 32 bytes"));
        }
        expected_payload_offset = end;
        sections.insert(id, Section { offset, length });
    }
    directory_reader.finish("section directory")?;
    if expected_payload_offset != bytes.len() {
        return Err(failure(
            Some(expected_payload_offset),
            "trailing or unreferenced file bytes",
        ));
    }
    let expected_ids: &[u32] = if section_count == 7 {
        &[1, 2, 3, 4, 5, 6, u32::MAX]
    } else {
        &[1, 2, 3, 4, 5, 6, 7, u32::MAX]
    };
    if sections.len() != expected_ids.len()
        || expected_ids.iter().any(|id| !sections.contains_key(id))
    {
        return Err(failure(Some(16), "missing or duplicate defined section"));
    }
    let checksum = sections[&u32::MAX];
    let mut digest = Sha256::new();
    digest.update(&bytes[..checksum.offset]);
    digest.update([0_u8; 32]);
    digest.update(&bytes[checksum.end()..]);
    let expected_digest = digest.finalize();
    if bytes[checksum.offset..checksum.end()] != expected_digest[..] {
        return Err(failure(Some(checksum.offset), "Checksum SHA-256 mismatch"));
    }
    Ok((sections, section_count))
}

#[derive(Clone, Debug)]
struct Constant {
    tag: u8,
    integer: Option<BigInt>,
    string: Option<String>,
    reference: Option<u32>,
}

fn parse_constants(
    bytes: &[u8],
    section: Section,
    limits: &VerificationLimits,
    budget: &mut u64,
) -> Result<Vec<Constant>, VerificationError> {
    let mut reader = Reader::new(&bytes[section.offset..section.end()], section.offset);
    let count_offset = reader.offset();
    let count = reader.u32()?;
    charge_entries(
        budget,
        count,
        limits.max_table_entries,
        "ConstantPool",
        count_offset,
    )?;
    let mut constants = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let entry_offset = reader.offset();
        let tag = reader.u8()?;
        let length = usize::try_from(reader.uleb128()?).map_err(|_| {
            failure(
                Some(entry_offset),
                "constant payload length exceeds host size",
            )
        })?;
        let payload_offset = reader.offset();
        let payload = reader.take(length)?;
        let mut value_reader = Reader::new(payload, payload_offset);
        let (integer, string, reference) = match tag {
            0x00 if payload.is_empty() => (None, None, None),
            0x01 if payload.len() == 1 && payload[0] <= 1 => (None, None, None),
            0x02 => (Some(value_reader.sleb128()?), None, None),
            0x03 if payload.len() == 8 => (None, None, None),
            0x04 => (
                None,
                Some(
                    std::str::from_utf8(payload)
                        .map_err(|_| {
                            failure(Some(payload_offset), "String constant is not valid UTF-8")
                        })?
                        .to_owned(),
                ),
                None,
            ),
            0x05 => (None, None, None),
            0x06 => {
                let scalar = value_reader.uleb128()?;
                if char::from_u32(u32::try_from(scalar).unwrap_or(u32::MAX)).is_none() {
                    return Err(failure(
                        Some(payload_offset),
                        "Char constant is not a Unicode scalar",
                    ));
                }
                (None, None, None)
            }
            0x07 | 0x08 if payload.len() == 4 => (None, None, Some(value_reader.u32()?)),
            0x00..=0x08 => {
                return Err(failure(
                    Some(entry_offset),
                    "constant tag has an invalid payload",
                ))
            }
            _ => return Err(failure(Some(entry_offset), "unknown constant tag")),
        };
        if matches!(tag, 0x02 | 0x06 | 0x07 | 0x08) {
            value_reader.finish("constant payload")?;
        }
        constants.push(Constant {
            tag,
            integer,
            string,
            reference,
        });
    }
    reader.finish("ConstantPool")?;
    Ok(constants)
}

fn read_count(
    reader: &mut Reader<'_>,
    budget: &mut u64,
    limits: &VerificationLimits,
    label: &str,
) -> Result<u32, VerificationError> {
    let offset = reader.offset();
    let count = reader.u32()?;
    charge_entries(budget, count, limits.max_table_entries, label, offset)?;
    Ok(count)
}

fn constant_at<'a>(
    constants: &'a [Constant],
    index: u32,
    offset: usize,
) -> Result<&'a Constant, VerificationError> {
    constants.get(index as usize).ok_or_else(|| {
        failure(
            Some(offset),
            format!("constant index {index} is out of range"),
        )
    })
}

fn string_constant<'a>(
    constants: &'a [Constant],
    index: u32,
    offset: usize,
) -> Result<&'a str, VerificationError> {
    let constant = constant_at(constants, index, offset)?;
    if constant.tag != 0x04 {
        return Err(failure(
            Some(offset),
            "table name index does not refer to a String",
        ));
    }
    Ok(constant
        .string
        .as_deref()
        .expect("String constant has text"))
}

fn nonnegative_int(
    constants: &[Constant],
    index: u32,
    offset: usize,
) -> Result<BigInt, VerificationError> {
    let constant = constant_at(constants, index, offset)?;
    let value = constant
        .integer
        .as_ref()
        .ok_or_else(|| failure(Some(offset), "identifier index does not refer to an Int"))?;
    if value.sign() == num_bigint::Sign::Minus {
        return Err(failure(Some(offset), "identifier Int must be nonnegative"));
    }
    Ok(value.clone())
}

#[derive(Clone, Debug)]
struct ModuleRecord {
    initializer_index: u32,
    first_function_index: u32,
    function_count: u32,
}

fn parse_modules(
    bytes: &[u8],
    section: Section,
    constants: &[Constant],
    limits: &VerificationLimits,
    budget: &mut u64,
) -> Result<(Vec<ModuleRecord>, u32), VerificationError> {
    let mut reader = Reader::new(&bytes[section.offset..section.end()], section.offset);
    let module_count = read_count(&mut reader, budget, limits, "ModuleTable modules")?;
    if module_count == 0 {
        return Err(failure(
            Some(section.offset),
            "ModuleTable must contain an entry module",
        ));
    }
    let entry_index = reader.u32()?;
    if entry_index >= module_count {
        return Err(failure(
            Some(reader.offset() - 4),
            "entry module index is out of range",
        ));
    }
    let mut modules = Vec::with_capacity(module_count as usize);
    for _ in 0..module_count {
        let record_offset = reader.offset();
        let name_index = reader.u32()?;
        string_constant(constants, name_index, record_offset)?;
        let initializer_index = reader.u32()?;
        let first_function_index = reader.u32()?;
        let function_count = reader.u32()?;
        let flags = reader.u32()?;
        if flags != 0 {
            return Err(failure(
                Some(record_offset + 16),
                "ModuleTable flags must be zero",
            ));
        }

        let global_count = read_count(&mut reader, budget, limits, "module globals")?;
        let mut global_ids = HashSet::new();
        for _ in 0..global_count {
            let item_offset = reader.offset();
            let id_index = reader.u32()?;
            let id = nonnegative_int(constants, id_index, item_offset)?;
            if !global_ids.insert(id) {
                return Err(failure(Some(item_offset), "duplicate global ID in module"));
            }
            let global_flags = reader.u32()?;
            if global_flags & !1 != 0 {
                return Err(failure(
                    Some(item_offset + 4),
                    "reserved global flags must be zero",
                ));
            }
        }

        let import_count = read_count(&mut reader, budget, limits, "module imports")?;
        for _ in 0..import_count {
            let item_offset = reader.offset();
            let module_name_index = reader.u32()?;
            string_constant(constants, module_name_index, item_offset)?;
            let symbol_name_index = reader.u32()?;
            string_constant(constants, symbol_name_index, item_offset + 4)?;
            let local_id_index = reader.u32()?;
            nonnegative_int(constants, local_id_index, item_offset + 8)?;
            let kind = reader.u8()?;
            if kind > 2 {
                return Err(failure(Some(item_offset + 12), "unknown import kind"));
            }
            if reader.take(3)? != [0, 0, 0] {
                return Err(failure(
                    Some(item_offset + 13),
                    "import reserved bytes must be zero",
                ));
            }
        }

        let export_count = read_count(&mut reader, budget, limits, "module exports")?;
        for _ in 0..export_count {
            let item_offset = reader.offset();
            let export_name_index = reader.u32()?;
            string_constant(constants, export_name_index, item_offset)?;
            let id_index = reader.u32()?;
            nonnegative_int(constants, id_index, item_offset + 4)?;
            let kind = reader.u8()?;
            if kind > 2 {
                return Err(failure(Some(item_offset + 8), "unknown export kind"));
            }
            if reader.take(3)? != [0, 0, 0] {
                return Err(failure(
                    Some(item_offset + 9),
                    "export reserved bytes must be zero",
                ));
            }
        }
        modules.push(ModuleRecord {
            initializer_index,
            first_function_index,
            function_count,
        });
    }
    reader.finish("ModuleTable")?;
    Ok((modules, entry_index))
}

#[derive(Clone, Debug)]
struct FunctionRecord {
    module_index: u32,
    id: Option<BigInt>,
    initializer: bool,
    parameter_ids: Vec<BigInt>,
    rest_parameter_code: u32,
    max_stack: u32,
    code_offset: usize,
    code_length: usize,
}

fn parse_functions(
    bytes: &[u8],
    section: Section,
    code: Section,
    modules: &[ModuleRecord],
    constants: &[Constant],
    limits: &VerificationLimits,
    budget: &mut u64,
) -> Result<Vec<FunctionRecord>, VerificationError> {
    let mut reader = Reader::new(&bytes[section.offset..section.end()], section.offset);
    let function_count = read_count(&mut reader, budget, limits, "FunctionTable")?;
    if function_count == 0 {
        return Err(failure(
            Some(section.offset),
            "FunctionTable must contain module initializers",
        ));
    }
    let mut functions = Vec::with_capacity(function_count as usize);
    let mut code_cursor = 0_usize;
    let mut identifiers: Vec<HashSet<BigInt>> = vec![HashSet::new(); modules.len()];
    for _ in 0..function_count {
        let record_offset = reader.offset();
        let module_index = reader.u32()?;
        if module_index as usize >= modules.len() {
            return Err(failure(
                Some(record_offset),
                "function module index is out of range",
            ));
        }
        let id_index = reader.u32()?;
        let name_index = reader.u32()?;
        let flags_offset = reader.offset();
        let flags = reader.u32()?;
        if flags & !3 != 0 {
            return Err(failure(
                Some(flags_offset),
                "reserved function flags must be zero",
            ));
        }
        let initializer = flags & 2 != 0;
        let asynchronous = flags & 1 != 0;
        let parameter_count = read_count(&mut reader, budget, limits, "function parameters")?;
        let rest_parameter_code = reader.u32()?;
        let mut parameter_ids = Vec::with_capacity(parameter_count as usize);
        let mut parameter_set = HashSet::new();
        for _ in 0..parameter_count {
            let id_offset = reader.offset();
            let id_index = reader.u32()?;
            let id = nonnegative_int(constants, id_index, id_offset)?;
            if !parameter_set.insert(id.clone()) {
                return Err(failure(
                    Some(id_offset),
                    "duplicate parameter ID in function",
                ));
            }
            parameter_ids.push(id);
        }
        let max_stack_offset = reader.offset();
        let max_stack = reader.u32()?;
        if max_stack > limits.max_stack_depth {
            return Err(failure(
                Some(max_stack_offset),
                "function max_stack exceeds host limit",
            ));
        }
        let code_offset_u64 = reader.u64()?;
        let code_length_u64 = reader.u64()?;
        let code_offset = usize::try_from(code_offset_u64).map_err(|_| {
            failure(
                Some(reader.offset() - 16),
                "function code offset exceeds host size",
            )
        })?;
        let code_length = usize::try_from(code_length_u64).map_err(|_| {
            failure(
                Some(reader.offset() - 8),
                "function code length exceeds host size",
            )
        })?;
        if code_length == 0 || code_offset != code_cursor {
            return Err(failure(
                Some(record_offset + 32),
                "function code ranges must be nonempty and contiguous",
            ));
        }
        code_cursor = code_offset
            .checked_add(code_length)
            .ok_or_else(|| failure(Some(record_offset + 32), "function code range overflow"))?;
        if code_cursor > code.length {
            return Err(failure(
                Some(record_offset + 32),
                "function code range exceeds Code section",
            ));
        }
        if name_index != u32::MAX {
            string_constant(constants, name_index, record_offset + 8)?;
        }
        let id = if id_index == u32::MAX {
            None
        } else {
            Some(nonnegative_int(constants, id_index, record_offset + 4)?)
        };
        if initializer != id.is_none() {
            return Err(failure(
                Some(record_offset + 4),
                "only initializers may omit a function ID",
            ));
        }
        if initializer && (asynchronous || parameter_count != 0 || rest_parameter_code != 0) {
            return Err(failure(
                Some(record_offset),
                "module initializer has invalid flags or parameters",
            ));
        }
        if !initializer && rest_parameter_code != 0 && rest_parameter_code != parameter_count {
            return Err(failure(
                Some(record_offset + 20),
                "rest_parameter_code must be zero or parameter_count",
            ));
        }
        if let Some(id) = &id {
            if !identifiers[module_index as usize].insert(id.clone()) {
                return Err(failure(
                    Some(record_offset + 4),
                    "duplicate function ID in module",
                ));
            }
        }
        functions.push(FunctionRecord {
            module_index,
            id,
            initializer,
            parameter_ids,
            rest_parameter_code,
            max_stack,
            code_offset,
            code_length,
        });
    }
    reader.finish("FunctionTable")?;
    if code_cursor != code.length {
        return Err(failure(
            Some(code.offset + code_cursor),
            "Code section contains unreferenced bytes",
        ));
    }

    let mut expected_first = 0_u32;
    for (module_index, module) in modules.iter().enumerate() {
        let end = module
            .first_function_index
            .checked_add(module.function_count)
            .ok_or_else(|| failure(Some(section.offset), "module function range overflow"))?;
        if module.first_function_index != expected_first
            || module.function_count == 0
            || end as usize > functions.len()
        {
            return Err(failure(
                Some(section.offset),
                "module function ranges are not contiguous",
            ));
        }
        if module.initializer_index != module.first_function_index {
            return Err(failure(
                Some(section.offset),
                "module initializer must be its first function",
            ));
        }
        let init = functions
            .get(module.initializer_index as usize)
            .expect("range checked");
        if init.module_index as usize != module_index || !init.initializer {
            return Err(failure(
                Some(section.offset),
                "module initializer reference is inconsistent",
            ));
        }
        let mut previous_id: Option<&BigInt> = None;
        for (position, function) in functions[module.first_function_index as usize..end as usize]
            .iter()
            .enumerate()
        {
            if function.module_index as usize != module_index {
                return Err(failure(
                    Some(section.offset),
                    "function module ownership is inconsistent",
                ));
            }
            if position == 0 {
                if !function.initializer {
                    return Err(failure(
                        Some(section.offset),
                        "module function range does not start with initializer",
                    ));
                }
            } else {
                if function.initializer {
                    return Err(failure(
                        Some(section.offset),
                        "module has more than one initializer",
                    ));
                }
                let id = function.id.as_ref().expect("non-initializer has ID");
                if previous_id.is_some_and(|previous| previous >= id) {
                    return Err(failure(
                        Some(section.offset),
                        "functions are not ordered by numeric ID",
                    ));
                }
                previous_id = Some(id);
            }
        }
        expected_first = end;
    }
    if expected_first as usize != functions.len() {
        return Err(failure(
            Some(section.offset),
            "FunctionTable contains functions outside module ranges",
        ));
    }
    Ok(functions)
}

#[derive(Clone, Debug)]
struct TypeRecord {
    module_index: u32,
    id: BigInt,
    is_interface: bool,
    implemented_interfaces: Vec<u32>,
}

fn parse_types(
    bytes: &[u8],
    section: Section,
    modules: &[ModuleRecord],
    functions: &[FunctionRecord],
    constants: &[Constant],
    limits: &VerificationLimits,
    budget: &mut u64,
) -> Result<Vec<TypeRecord>, VerificationError> {
    let mut reader = Reader::new(&bytes[section.offset..section.end()], section.offset);
    let type_count = read_count(&mut reader, budget, limits, "TypeTable")?;
    let mut types = Vec::with_capacity(type_count as usize);
    let mut type_ids: Vec<HashSet<BigInt>> = vec![HashSet::new(); modules.len()];
    for _ in 0..type_count {
        let record_offset = reader.offset();
        let module_index = reader.u32()?;
        if module_index as usize >= modules.len() {
            return Err(failure(
                Some(record_offset),
                "type module index is out of range",
            ));
        }
        let type_id_index = reader.u32()?;
        let type_id = nonnegative_int(constants, type_id_index, record_offset + 4)?;
        if !type_ids[module_index as usize].insert(type_id.clone()) {
            return Err(failure(
                Some(record_offset + 4),
                "duplicate type ID in module",
            ));
        }
        let name_index = reader.u32()?;
        string_constant(constants, name_index, record_offset + 8)?;
        let kind = reader.u8()?;
        if kind > 1 {
            return Err(failure(Some(record_offset + 12), "unknown type kind"));
        }
        let flags = reader.u8()?;
        let reserved = reader.u16()?;
        if flags != 0 || reserved != 0 {
            return Err(failure(
                Some(record_offset + 13),
                "type flags and reserved field must be zero",
            ));
        }
        let is_interface = kind == 1;
        let field_count = read_count(&mut reader, budget, limits, "type fields")?;
        if is_interface && field_count != 0 {
            return Err(failure(
                Some(reader.offset() - 4),
                "Interface type cannot contain fields",
            ));
        }
        let mut member_ids = HashSet::new();
        for _ in 0..field_count {
            let field_offset = reader.offset();
            let id_index = reader.u32()?;
            if !member_ids.insert(nonnegative_int(constants, id_index, field_offset)?) {
                return Err(failure(
                    Some(field_offset),
                    "duplicate field or method ID in type",
                ));
            }
            let field_name = reader.u32()?;
            string_constant(constants, field_name, field_offset + 4)?;
        }
        let method_count = read_count(&mut reader, budget, limits, "type methods")?;
        for _ in 0..method_count {
            let method_offset = reader.offset();
            let id_index = reader.u32()?;
            if !member_ids.insert(nonnegative_int(constants, id_index, method_offset)?) {
                return Err(failure(
                    Some(method_offset),
                    "duplicate field or method ID in type",
                ));
            }
            let method_name = reader.u32()?;
            string_constant(constants, method_name, method_offset + 4)?;
            let function_index = reader.u32()?;
            if is_interface {
                if function_index != u32::MAX {
                    return Err(failure(
                        Some(method_offset + 8),
                        "Interface method must use the absent-function sentinel",
                    ));
                }
            } else {
                let function = functions.get(function_index as usize).ok_or_else(|| {
                    failure(
                        Some(method_offset + 8),
                        "method function index is out of range",
                    )
                })?;
                if function.module_index != module_index || function.initializer {
                    return Err(failure(
                        Some(method_offset + 8),
                        "method function belongs to a different module or is an initializer",
                    ));
                }
            }
        }
        let implemented_count = read_count(&mut reader, budget, limits, "implemented interfaces")?;
        if is_interface && implemented_count != 0 {
            return Err(failure(
                Some(reader.offset() - 4),
                "Interface type cannot implement interfaces",
            ));
        }
        let mut implemented_interfaces = Vec::with_capacity(implemented_count as usize);
        for _ in 0..implemented_count {
            implemented_interfaces.push(reader.u32()?);
        }
        if types.last().is_some_and(|previous: &TypeRecord| {
            (previous.module_index, &previous.id) >= (module_index, &type_id)
        }) {
            return Err(failure(
                Some(record_offset),
                "types are not ordered by module and numeric ID",
            ));
        }
        types.push(TypeRecord {
            module_index,
            id: type_id,
            is_interface,
            implemented_interfaces,
        });
    }
    reader.finish("TypeTable")?;
    for type_record in &types {
        for &interface_index in &type_record.implemented_interfaces {
            let interface = types.get(interface_index as usize).ok_or_else(|| {
                failure(
                    Some(section.offset),
                    "implemented interface index is out of range",
                )
            })?;
            if !interface.is_interface {
                return Err(failure(
                    Some(section.offset),
                    "implemented type reference is not an Interface",
                ));
            }
        }
    }
    Ok(types)
}

#[derive(Clone, Debug)]
struct ExceptionRecord {
    function_index: u32,
    protected_start: u64,
    protected_end: u64,
    handler_offset: u64,
    handler_end: u64,
    stack_depth: u32,
}

fn parse_exceptions(
    bytes: &[u8],
    section: Section,
    functions: &[FunctionRecord],
    constants: &[Constant],
    limits: &VerificationLimits,
    budget: &mut u64,
) -> Result<Vec<ExceptionRecord>, VerificationError> {
    let mut reader = Reader::new(&bytes[section.offset..section.end()], section.offset);
    let count = read_count(&mut reader, budget, limits, "ExceptionTable")?;
    let mut records = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let offset = reader.offset();
        let function_index = reader.u32()?;
        let function = functions
            .get(function_index as usize)
            .ok_or_else(|| failure(Some(offset), "exception function index is out of range"))?;
        let protected_start = reader.u64()?;
        let protected_end = reader.u64()?;
        let handler_offset = reader.u64()?;
        let handler_end = reader.u64()?;
        let stack_depth = reader.u32()?;
        let type_constant_index = reader.u32()?;
        let flags = reader.u32()?;
        if flags != 0 {
            return Err(failure(Some(offset + 44), "exception flags must be zero"));
        }
        if protected_start >= protected_end
            || protected_end > function.code_length as u64
            || handler_offset >= handler_end
            || handler_end > function.code_length as u64
            || (handler_offset < protected_end && protected_start < handler_end)
        {
            return Err(failure(
                Some(offset + 4),
                "exception protected or handler range is invalid",
            ));
        }
        if stack_depth > function.max_stack {
            return Err(failure(
                Some(offset + 36),
                "exception stack depth exceeds function max_stack",
            ));
        }
        if type_constant_index != u32::MAX {
            let constant = constant_at(constants, type_constant_index, offset + 40)?;
            if constant.tag != 0x08 {
                return Err(failure(
                    Some(offset + 40),
                    "exception type must reference a TypeRef constant",
                ));
            }
        }
        if records.last().is_some_and(|previous: &ExceptionRecord| {
            (
                previous.function_index,
                previous.protected_start,
                previous.protected_end,
            ) > (function_index, protected_start, protected_end)
        }) {
            return Err(failure(
                Some(offset),
                "exception handlers are not ordered by function, start, and end",
            ));
        }
        records.push(ExceptionRecord {
            function_index,
            protected_start,
            protected_end,
            handler_offset,
            handler_end,
            stack_depth,
        });
    }
    reader.finish("ExceptionTable")?;
    Ok(records)
}

#[derive(Clone, Debug)]
struct DebugLocation {
    function_index: u32,
    code_offset: u64,
    source_file_index: u32,
    start_byte_offset: u64,
    end_byte_offset: u64,
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
}

#[derive(Clone, Debug)]
struct DebugInfo {
    source_file_count: u32,
    locations: Vec<DebugLocation>,
}

fn parse_debug_info(
    bytes: &[u8],
    section: Option<Section>,
    constants: &[Constant],
    limits: &VerificationLimits,
    budget: &mut u64,
) -> Result<DebugInfo, VerificationError> {
    let Some(section) = section else {
        return Ok(DebugInfo {
            source_file_count: 0,
            locations: Vec::new(),
        });
    };
    let mut reader = Reader::new(&bytes[section.offset..section.end()], section.offset);
    let source_count = read_count(&mut reader, budget, limits, "DebugInfo source files")?;
    for _ in 0..source_count {
        let offset = reader.offset();
        string_constant(constants, reader.u32()?, offset)?;
    }
    let location_count = read_count(&mut reader, budget, limits, "DebugInfo locations")?;
    let mut locations = Vec::with_capacity(location_count as usize);
    let mut previous_location = None;
    for _ in 0..location_count {
        let offset = reader.offset();
        let location = DebugLocation {
            function_index: reader.u32()?,
            code_offset: reader.u64()?,
            source_file_index: reader.u32()?,
            start_byte_offset: reader.u64()?,
            end_byte_offset: reader.u64()?,
            start_line: reader.u32()?,
            start_column: reader.u32()?,
            end_line: reader.u32()?,
            end_column: reader.u32()?,
        };
        if location.source_file_index >= source_count {
            return Err(failure(
                Some(offset + 12),
                "DebugInfo source file index is out of range",
            ));
        }
        if location.start_byte_offset > location.end_byte_offset
            || location.start_line == 0
            || location.start_column == 0
            || location.end_line == 0
            || location.end_column == 0
            || (location.end_line, location.end_column)
                < (location.start_line, location.start_column)
        {
            return Err(failure(
                Some(offset + 16),
                "DebugInfo source span is invalid",
            ));
        }
        let key = (location.function_index, location.code_offset);
        if previous_location.is_some_and(|previous| previous >= key) {
            return Err(failure(
                Some(offset),
                "DebugInfo locations are not strictly ordered",
            ));
        }
        previous_location = Some(key);
        locations.push(location);
    }
    reader.finish("DebugInfo")?;
    Ok(DebugInfo {
        source_file_count: source_count,
        locations,
    })
}

#[derive(Clone, Debug)]
struct DecodedFunction {
    instructions: Vec<Instruction>,
    offsets: HashMap<usize, usize>,
}

#[derive(Clone, Debug)]
struct Instruction {
    offset: usize,
    kind: InstructionKind,
}

#[derive(Clone, Debug)]
enum InstructionKind {
    PushConst(u32),
    Intrinsic(IrOp),
    Jump {
        target: u32,
        unwind_scopes: u32,
    },
    BranchIfFalse {
        false_target: u32,
        true_target: u32,
    },
    ForInit {
        loop_state: u32,
    },
    ForNext {
        loop_state: u32,
        body_target: u32,
        exit_target: u32,
    },
    ScopeEnter,
    ScopeExit,
    ReturnTop {
        unwind_to_frame_base: u32,
    },
    Trap,
    Rethrow,
    Halt,
}

impl InstructionKind {
    fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::ReturnTop { .. } | Self::Trap | Self::Rethrow | Self::Halt
        ) || matches!(self, Self::Intrinsic(IrOp::TailCall))
    }
}

fn require_capability(
    policy: &VerificationPolicy,
    capability: Capability,
    offset: usize,
) -> Result<(), VerificationError> {
    if policy.granted_capabilities.contains(&capability) {
        Ok(())
    } else {
        Err(failure(
            Some(offset),
            format!("intrinsic requires capability {}", capability.name()),
        ))
    }
}

fn check_intrinsic_capabilities(
    operation: IrOp,
    policy: &VerificationPolicy,
    offset: usize,
) -> Result<(), VerificationError> {
    match operation {
        // Import requirements depend on the actual resolution path. The module
        // resolver checks fs.read on file access and env.read only if it reads
        // STAR_PATH; an opcode-only pass cannot determine either requirement.
        IrOp::Import | IrOp::ImportDynamic => {}
        IrOp::Sleep => require_capability(policy, Capability::Clock, offset)?,
        IrOp::NativeLoad
        | IrOp::NativeSymbol
        | IrOp::NativeCall
        | IrOp::NativeClose
        | IrOp::NativeCallback
        | IrOp::NativeBuffer => require_capability(policy, Capability::Ffi, offset)?,
        _ => {}
    }
    Ok(())
}

fn decode_functions(
    bytes: &[u8],
    code_section: Section,
    functions: &[FunctionRecord],
    constants: &[Constant],
    policy: &VerificationPolicy,
) -> Result<(Vec<DecodedFunction>, u64), VerificationError> {
    let mut total_instructions = 0_u64;
    let mut decoded_functions = Vec::with_capacity(functions.len());
    for function in functions {
        let function_start = code_section
            .offset
            .checked_add(function.code_offset)
            .ok_or_else(|| failure(None, "function bytecode start overflow"))?;
        let function_end = function_start
            .checked_add(function.code_length)
            .ok_or_else(|| failure(Some(function_start), "function bytecode end overflow"))?;
        let mut reader = Reader::new(&bytes[function_start..function_end], function_start);
        let mut instructions = Vec::new();
        let mut offsets = HashMap::new();
        let mut initialized_loop_ids = HashSet::new();
        while reader.position < reader.bytes.len() {
            if total_instructions >= policy.limits.max_instructions {
                return Err(failure(
                    Some(reader.offset()),
                    "decoded instructions exceed host limit",
                ));
            }
            let offset = reader.position;
            let absolute_offset = reader.offset();
            let opcode = reader.u8()?;
            let kind = match opcode {
                0x01 => {
                    let constant_index = reader.u32()?;
                    constant_at(constants, constant_index, absolute_offset + 1)?;
                    InstructionKind::PushConst(constant_index)
                }
                0xE0 => {
                    let intrinsic_id = reader.u16()?;
                    let operation = IrOp::from_intrinsic_id(intrinsic_id).ok_or_else(|| {
                        failure(Some(absolute_offset + 1), "unknown intrinsic ID")
                    })?;
                    check_intrinsic_capabilities(operation, policy, absolute_offset)?;
                    InstructionKind::Intrinsic(operation)
                }
                0xE1 => InstructionKind::Jump {
                    target: reader.u32()?,
                    unwind_scopes: reader.u32()?,
                },
                0xE2 => InstructionKind::BranchIfFalse {
                    false_target: reader.u32()?,
                    true_target: reader.u32()?,
                },
                0xE3 => {
                    let loop_state = reader.u32()?;
                    if !initialized_loop_ids.insert(loop_state) {
                        return Err(failure(
                            Some(absolute_offset + 1),
                            "loop-state ID is initialized more than once in a function",
                        ));
                    }
                    InstructionKind::ForInit { loop_state }
                }
                0xE4 => InstructionKind::ForNext {
                    loop_state: reader.u32()?,
                    body_target: reader.u32()?,
                    exit_target: reader.u32()?,
                },
                0xE5 => InstructionKind::ScopeEnter,
                0xE6 => InstructionKind::ScopeExit,
                0xE7 => InstructionKind::ReturnTop {
                    unwind_to_frame_base: reader.u32()?,
                },
                0xE8 => InstructionKind::Trap,
                0xE9 => InstructionKind::Rethrow,
                0xFF => InstructionKind::Halt,
                _ => {
                    return Err(failure(
                        Some(absolute_offset),
                        format!("unknown VM opcode 0x{opcode:02X}"),
                    ))
                }
            };
            offsets.insert(offset, instructions.len());
            instructions.push(Instruction { offset, kind });
            total_instructions += 1;
        }
        reader.finish("function code")?;
        if !instructions.last().is_some_and(|instruction| {
            instruction.kind.is_terminal()
                || matches!(
                    instruction.kind,
                    InstructionKind::Jump { .. }
                        | InstructionKind::BranchIfFalse { .. }
                        | InstructionKind::ForNext { .. }
                )
        }) {
            return Err(failure(
                Some(function_end.saturating_sub(1)),
                "function code falls through its range",
            ));
        }
        for instruction in &instructions {
            match instruction.kind {
                InstructionKind::Jump { target, .. } => {
                    require_target(
                        target,
                        &offsets,
                        function.code_length,
                        function_start + instruction.offset,
                    )?;
                }
                InstructionKind::BranchIfFalse {
                    false_target,
                    true_target,
                } => {
                    require_target(
                        false_target,
                        &offsets,
                        function.code_length,
                        function_start + instruction.offset,
                    )?;
                    require_target(
                        true_target,
                        &offsets,
                        function.code_length,
                        function_start + instruction.offset,
                    )?;
                }
                InstructionKind::ForNext {
                    loop_state,
                    body_target,
                    exit_target,
                } => {
                    if !initialized_loop_ids.contains(&loop_state) {
                        return Err(failure(
                            Some(function_start + instruction.offset),
                            "FOR_NEXT references an undefined loop-state ID",
                        ));
                    }
                    require_target(
                        body_target,
                        &offsets,
                        function.code_length,
                        function_start + instruction.offset,
                    )?;
                    require_target(
                        exit_target,
                        &offsets,
                        function.code_length,
                        function_start + instruction.offset,
                    )?;
                }
                _ => {}
            }
        }
        decoded_functions.push(DecodedFunction {
            instructions,
            offsets,
        });
    }
    Ok((decoded_functions, total_instructions))
}

fn require_target(
    target: u32,
    offsets: &HashMap<usize, usize>,
    code_length: usize,
    instruction_offset: usize,
) -> Result<(), VerificationError> {
    if target as usize >= code_length || !offsets.contains_key(&(target as usize)) {
        return Err(failure(
            Some(instruction_offset),
            "branch target is not an instruction start in this function",
        ));
    }
    Ok(())
}

/// Validates a `.starc` version 1.0 file under explicit host limits and permissions.
pub fn verify_bytecode<'a>(
    bytes: &'a [u8],
    policy: &VerificationPolicy,
) -> Result<VerifiedProgram<'a>, VerificationError> {
    let (sections, _) = parse_container(bytes, &policy.limits)?;
    let mut table_budget = 0_u64;
    let constants = parse_constants(bytes, sections[&1], &policy.limits, &mut table_budget)?;
    let (modules, entry_module) = parse_modules(
        bytes,
        sections[&2],
        &constants,
        &policy.limits,
        &mut table_budget,
    )?;
    let functions = parse_functions(
        bytes,
        sections[&3],
        sections[&4],
        &modules,
        &constants,
        &policy.limits,
        &mut table_budget,
    )?;
    let types = parse_types(
        bytes,
        sections[&5],
        &modules,
        &functions,
        &constants,
        &policy.limits,
        &mut table_budget,
    )?;
    for constant in &constants {
        if let Some(reference) = constant.reference {
            match constant.tag {
                0x07 if reference as usize >= functions.len() => {
                    return Err(failure(None, "FunctionRef constant index is out of range"));
                }
                0x08 if reference as usize >= types.len() => {
                    return Err(failure(None, "TypeRef constant index is out of range"));
                }
                _ => {}
            }
        }
    }
    let exceptions = parse_exceptions(
        bytes,
        sections[&6],
        &functions,
        &constants,
        &policy.limits,
        &mut table_budget,
    )?;
    let debug = parse_debug_info(
        bytes,
        sections.get(&7).copied(),
        &constants,
        &policy.limits,
        &mut table_budget,
    )?;
    let (decoded, instruction_count) =
        decode_functions(bytes, sections[&4], &functions, &constants, policy)?;
    let function_stack_depths = verify_function_flow(
        &functions,
        &decoded,
        &exceptions,
        &constants,
        sections[&4].offset,
        policy,
    )?;
    verify_exception_and_debug(&sections, &functions, &decoded, &exceptions, &debug)?;
    let required_stack_depth = function_stack_depths.iter().copied().max().unwrap_or(0);
    Ok(VerifiedProgram {
        bytes,
        module_count: modules.len() as u32,
        entry_module_index: entry_module,
        function_count: functions.len() as u32,
        instruction_count,
        required_stack_depth,
        function_stack_depths,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FlowState {
    stack: Vec<Option<BigInt>>,
    opaque_prefix: bool,
    scope_depth: u32,
    loop_states: BTreeSet<u32>,
    pending_captures: bool,
    handler_range: Option<(u64, u64)>,
}

impl FlowState {
    fn new() -> Self {
        Self {
            stack: Vec::new(),
            opaque_prefix: false,
            scope_depth: 0,
            loop_states: BTreeSet::new(),
            pending_captures: false,
            handler_range: None,
        }
    }

    fn peek(
        &self,
        depth_from_top: usize,
        offset: usize,
    ) -> Result<Option<BigInt>, VerificationError> {
        if depth_from_top == 0 {
            return Err(failure(
                Some(offset),
                "internal verifier error: zero-based stack peek",
            ));
        }
        if let Some(index) = self.stack.len().checked_sub(depth_from_top) {
            Ok(self.stack[index].clone())
        } else if self.opaque_prefix {
            Ok(None)
        } else {
            Err(failure(Some(offset), "operand stack underflow"))
        }
    }

    fn pop_many(&mut self, count: usize, offset: usize) -> Result<(), VerificationError> {
        if count <= self.stack.len() {
            self.stack.truncate(self.stack.len() - count);
        } else if self.opaque_prefix {
            self.stack.clear();
        } else {
            return Err(failure(Some(offset), "operand stack underflow"));
        }
        Ok(())
    }

    fn push(&mut self, value: Option<BigInt>) {
        self.stack.push(value);
    }

    fn make_prefix_opaque(&mut self) {
        self.stack.clear();
        self.opaque_prefix = true;
    }
}

fn dynamic_count(
    state: &FlowState,
    depth: usize,
    offset: usize,
) -> Result<Option<usize>, VerificationError> {
    match state.peek(depth, offset)? {
        Some(value) if value.sign() == num_bigint::Sign::Minus => Err(failure(
            Some(offset),
            "statically known dynamic operand count is negative",
        )),
        Some(value) => value.to_usize().map(Some).ok_or_else(|| {
            failure(
                Some(offset),
                "statically known dynamic operand count exceeds host range",
            )
        }),
        None => Ok(None),
    }
}

fn consume_dynamic(
    state: &mut FlowState,
    count_depth: usize,
    fixed_values: usize,
    values_per_item: usize,
    outputs: usize,
    offset: usize,
) -> Result<(), VerificationError> {
    let Some(count) = dynamic_count(state, count_depth, offset)? else {
        state.pop_many(fixed_values, offset)?;
        state.make_prefix_opaque();
        for _ in 0..outputs {
            state.push(None);
        }
        return Ok(());
    };
    let consumed = count
        .checked_mul(values_per_item)
        .and_then(|items| items.checked_add(fixed_values))
        .ok_or_else(|| failure(Some(offset), "dynamic operand count overflows host range"))?;
    state.pop_many(consumed, offset)?;
    for _ in 0..outputs {
        state.push(None);
    }
    Ok(())
}

fn fixed_effect(
    state: &mut FlowState,
    inputs: usize,
    outputs: usize,
    offset: usize,
) -> Result<(), VerificationError> {
    state.pop_many(inputs, offset)?;
    for _ in 0..outputs {
        state.push(None);
    }
    Ok(())
}

fn find_function<'a>(
    functions: &'a [FunctionRecord],
    module_index: u32,
    id: &BigInt,
) -> Option<&'a FunctionRecord> {
    functions
        .iter()
        .find(|function| function.module_index == module_index && function.id.as_ref() == Some(id))
}

fn check_call_arity(
    function: &FunctionRecord,
    argument_count: Option<usize>,
    offset: usize,
) -> Result<(), VerificationError> {
    let Some(argument_count) = argument_count else {
        return Ok(());
    };
    let parameter_count = function.parameter_ids.len();
    let valid = if function.rest_parameter_code == 0 {
        argument_count == parameter_count
    } else {
        argument_count >= parameter_count.saturating_sub(1)
    };
    if valid {
        Ok(())
    } else {
        Err(failure(
            Some(offset),
            "direct call argument count does not match the function descriptor",
        ))
    }
}

fn validate_direct_call(
    state: &FlowState,
    current: &FunctionRecord,
    functions: &[FunctionRecord],
    function_id_depth: usize,
    argument_count_depth: usize,
    offset: usize,
) -> Result<(), VerificationError> {
    let function_id = state.peek(function_id_depth, offset)?.ok_or_else(|| {
        failure(
            Some(offset),
            "direct call function ID is not statically known",
        )
    })?;
    let function =
        find_function(functions, current.module_index, &function_id).ok_or_else(|| {
            failure(
                Some(offset),
                "direct call references an unknown function ID",
            )
        })?;
    let argument_count = dynamic_count(state, argument_count_depth, offset)?;
    check_call_arity(function, argument_count, offset)
}

fn apply_intrinsic(
    operation: IrOp,
    state: &mut FlowState,
    current: &FunctionRecord,
    functions: &[FunctionRecord],
    offset: usize,
) -> Result<(), VerificationError> {
    use IrOp::*;
    match operation {
        Add | Sub | Mul | Div | Mod | Pow | Eq | Gt | Lt | And | Or | Xor | BitAnd | BitOr
        | BitXor | Shl | Shr | TypeCheck | IntToBytes | GetItem | Contains | Slice | Concat
        | FieldGet | ObjectIs | Implement | ModuleGet | ChannelSend => {
            let outputs = if matches!(operation, Implement) { 0 } else { 1 };
            let inputs = if matches!(operation, Slice) { 3 } else { 2 };
            fixed_effect(state, inputs, outputs, offset)?;
        }
        Print | Store | StoreGlobal | Declare | Const | Delete | TypeAnnotate | SetItem
        | Append | FieldSet | MethodDef | Export | NativeClose | Yield | MutexUnlock => {
            let inputs = match operation {
                SetItem | FieldSet => 3,
                MethodDef => 3,
                Yield => 0,
                Print | Delete | Export | NativeClose | MutexUnlock => 1,
                _ => 2,
            };
            fixed_effect(state, inputs, 0, offset)?;
        }
        Input | PushNull | PushTrue | PushFalse | MutexNew => fixed_effect(state, 0, 1, offset)?,
        Load | LoadGlobal | ToInt | ToFloat | ToBool | ToString | ToBytes | Utf8Decode
        | Utf8Encode | FloatFromBits | TypeOf | ToChar | ToByte | Neg | Abs | BitNot | Length
        | Iter | ObjectClone | Import | ImportDynamic | Await | Spawn | Sleep | Join | Cancel
        | ChannelNew | ChannelRecv | MutexLock | NativeLoad | NativeBuffer => {
            fixed_effect(state, 1, 1, offset)?;
        }
        Not => fixed_effect(state, 1, 1, offset)?,
        Call | TailCall => {
            validate_direct_call(state, current, functions, 1, 2, offset)?;
            consume_dynamic(
                state,
                2,
                2,
                1,
                if operation == Call { 1 } else { 0 },
                offset,
            )?;
        }
        CallValue => consume_dynamic(state, 2, 2, 1, 1, offset)?,
        Lambda => {
            let function_id = state
                .peek(2, offset)?
                .ok_or_else(|| failure(Some(offset), "LAMBDA body ID is not statically known"))?;
            let function = find_function(functions, current.module_index, &function_id)
                .ok_or_else(|| {
                    failure(Some(offset), "LAMBDA references an unknown body function")
                })?;
            let parameter_count = dynamic_count(state, 1, offset)?;
            if parameter_count.is_some_and(|count| count != function.parameter_ids.len()) {
                return Err(failure(
                    Some(offset),
                    "LAMBDA parameter count does not match its body",
                ));
            }
            fixed_effect(state, 2, 1, offset)?;
        }
        Capture => fixed_effect(state, 1, 0, offset)?,
        ArrayNew | SetNew | TupleNew => consume_dynamic(state, 1, 1, 1, 1, offset)?,
        MapNew => consume_dynamic(state, 1, 1, 2, 1, offset)?,
        StructDef | InterfaceDef => consume_dynamic(state, 1, 2, 1, 0, offset)?,
        ObjectNew => consume_dynamic(state, 2, 2, 1, 1, offset)?,
        MethodCall => consume_dynamic(state, 3, 3, 1, 1, offset)?,
        ModuleCall => consume_dynamic(state, 3, 3, 1, 1, offset)?,
        NativeCall => consume_dynamic(state, 2, 2, 1, 1, offset)?,
        ModuleInit => fixed_effect(state, 1, 0, offset)?,
        Next => fixed_effect(state, 1, 2, offset)?,
        Remove => fixed_effect(state, 2, 1, offset)?,
        NativeSymbol => fixed_effect(state, 3, 1, offset)?,
        NativeCallback => fixed_effect(state, 2, 1, offset)?,
    }
    Ok(())
}

fn merge_flow_state(
    current: &mut FlowState,
    incoming: &FlowState,
    offset: usize,
) -> Result<bool, VerificationError> {
    if current.stack.len() != incoming.stack.len()
        || current.opaque_prefix != incoming.opaque_prefix
        || current.scope_depth != incoming.scope_depth
        || current.loop_states != incoming.loop_states
        || current.pending_captures != incoming.pending_captures
    {
        return Err(failure(
            Some(offset),
            "incompatible stack height, scope depth, opaque prefix, or loop state at control-flow join",
        ));
    }
    let mut changed = false;
    for (current_value, incoming_value) in current.stack.iter_mut().zip(&incoming.stack) {
        if current_value.is_some() && current_value != incoming_value {
            *current_value = None;
            changed = true;
        }
    }
    let handler_range = match (current.handler_range, incoming.handler_range) {
        (Some((current_start, current_end)), Some((incoming_start, incoming_end))) => {
            let start = current_start.max(incoming_start);
            let end = current_end.min(incoming_end);
            (start < end).then_some((start, end))
        }
        _ => None,
    };
    if handler_range != current.handler_range {
        current.handler_range = handler_range;
        changed = true;
    }
    Ok(changed)
}

fn verify_function_flow(
    functions: &[FunctionRecord],
    decoded_functions: &[DecodedFunction],
    exceptions: &[ExceptionRecord],
    constants: &[Constant],
    code_base: usize,
    policy: &VerificationPolicy,
) -> Result<Vec<u32>, VerificationError> {
    let mut function_stack_depths = Vec::with_capacity(functions.len());
    for (function_index, (function, decoded)) in functions.iter().zip(decoded_functions).enumerate()
    {
        if decoded.instructions.is_empty() {
            return Err(failure(
                Some(function.code_offset),
                "function has empty code",
            ));
        }
        let mut states: Vec<Option<FlowState>> = vec![None; decoded.instructions.len()];
        let mut worklist = VecDeque::new();
        states[0] = Some(FlowState::new());
        worklist.push_back(0_usize);
        let mut required_stack = 0_usize;

        while let Some(instruction_index) = worklist.pop_front() {
            let instruction = &decoded.instructions[instruction_index];
            let mut state = states[instruction_index]
                .clone()
                .expect("queued flow state is present");
            let instruction_offset = instruction.offset as u64;
            if !state
                .handler_range
                .is_some_and(|(start, end)| start <= instruction_offset && instruction_offset < end)
            {
                state.handler_range = None;
            }
            let absolute_offset = code_base + function.code_offset + instruction.offset;
            required_stack = required_stack.max(state.stack.len());
            let mut successors: Vec<(usize, FlowState)> = Vec::with_capacity(2);
            let mut terminal = false;

            for exception in exceptions.iter().filter(|exception| {
                exception.function_index as usize == function_index
                    && exception.protected_start == instruction_offset
            }) {
                if state.opaque_prefix || state.stack.len() != exception.stack_depth as usize {
                    return Err(failure(
                        Some(absolute_offset),
                        "exception stack depth does not match protected-entry stack state",
                    ));
                }
                let handler_offset = usize::try_from(exception.handler_offset)
                    .map_err(|_| failure(Some(absolute_offset), "handler offset overflow"))?;
                let Some(handler_index) = decoded.offsets.get(&handler_offset).copied() else {
                    return Err(failure(
                        Some(absolute_offset),
                        "exception handler is not an instruction",
                    ));
                };
                let mut handler_state = state.clone();
                let handler_start = exception.handler_offset;
                let handler_end = exception.handler_end;
                handler_state.handler_range = Some(match state.handler_range {
                    Some((start, end)) if start <= handler_start && handler_start < end => {
                        (start.min(handler_start), end.max(handler_end))
                    }
                    _ => (handler_start, handler_end),
                });
                successors.push((handler_index, handler_state));
            }

            match instruction.kind {
                InstructionKind::PushConst(constant_index) => {
                    let constant = constant_at(constants, constant_index, absolute_offset)?;
                    state.push(constant.integer.clone());
                }
                InstructionKind::Intrinsic(operation) => {
                    if operation == IrOp::TailCall && function.initializer {
                        return Err(failure(
                            Some(absolute_offset),
                            "module initializer cannot tail-call",
                        ));
                    }
                    apply_intrinsic(operation, &mut state, function, functions, absolute_offset)?;
                    match operation {
                        IrOp::Capture => state.pending_captures = true,
                        IrOp::Lambda => state.pending_captures = false,
                        _ => {}
                    }
                    if operation == IrOp::TailCall && state.pending_captures {
                        return Err(failure(
                            Some(absolute_offset),
                            "TAIL_CALL exits with pending closure captures",
                        ));
                    }
                    terminal = operation == IrOp::TailCall;
                }
                InstructionKind::Jump {
                    target,
                    unwind_scopes,
                } => {
                    if unwind_scopes > state.scope_depth {
                        return Err(failure(
                            Some(absolute_offset),
                            "JUMP unwinds more scopes than are active",
                        ));
                    }
                    state.scope_depth -= unwind_scopes;
                    let target_index = decoded.offsets[&(target as usize)];
                    successors.push((target_index, state.clone()));
                }
                InstructionKind::BranchIfFalse {
                    false_target,
                    true_target,
                } => {
                    state.pop_many(1, absolute_offset)?;
                    successors.push((decoded.offsets[&(false_target as usize)], state.clone()));
                    successors.push((decoded.offsets[&(true_target as usize)], state.clone()));
                }
                InstructionKind::ForInit { loop_state } => {
                    state.pop_many(2, absolute_offset)?;
                    state.scope_depth = state
                        .scope_depth
                        .checked_add(1)
                        .ok_or_else(|| failure(Some(absolute_offset), "scope depth overflow"))?;
                    state.loop_states.insert(loop_state);
                }
                InstructionKind::ForNext {
                    loop_state,
                    body_target,
                    exit_target,
                } => {
                    if !state.loop_states.contains(&loop_state) {
                        return Err(failure(
                            Some(absolute_offset),
                            "FOR_NEXT is reachable before its FOR_INIT",
                        ));
                    }
                    successors.push((decoded.offsets[&(body_target as usize)], state.clone()));
                    let mut exit_state = state.clone();
                    exit_state.loop_states.remove(&loop_state);
                    successors.push((decoded.offsets[&(exit_target as usize)], exit_state));
                }
                InstructionKind::ScopeEnter => {
                    state.scope_depth = state
                        .scope_depth
                        .checked_add(1)
                        .ok_or_else(|| failure(Some(absolute_offset), "scope depth overflow"))?;
                }
                InstructionKind::ScopeExit => {
                    state.scope_depth = state.scope_depth.checked_sub(1).ok_or_else(|| {
                        failure(
                            Some(absolute_offset),
                            "SCOPE_EXIT has no matching SCOPE_ENTER",
                        )
                    })?;
                }
                InstructionKind::ReturnTop {
                    unwind_to_frame_base,
                } => {
                    if function.initializer {
                        return Err(failure(
                            Some(absolute_offset),
                            "module initializer cannot RETURN_TOP",
                        ));
                    }
                    if unwind_to_frame_base != state.scope_depth {
                        return Err(failure(
                            Some(absolute_offset),
                            "RETURN_TOP unwind count does not match active scope depth",
                        ));
                    }
                    if state.pending_captures {
                        return Err(failure(
                            Some(absolute_offset),
                            "RETURN_TOP exits with pending closure captures",
                        ));
                    }
                    if !state.stack.is_empty() {
                        state.pop_many(1, absolute_offset)?;
                    }
                    terminal = true;
                }
                InstructionKind::Trap => terminal = true,
                InstructionKind::Rethrow => {
                    if state.handler_range.is_none() {
                        return Err(failure(
                            Some(absolute_offset),
                            "RETHROW is not reachable from an exception handler",
                        ));
                    }
                    terminal = true;
                }
                InstructionKind::Halt => {
                    if !function.initializer {
                        return Err(failure(
                            Some(absolute_offset),
                            "HALT is only valid in a module initializer",
                        ));
                    }
                    if state.scope_depth != 0 {
                        return Err(failure(
                            Some(absolute_offset),
                            "HALT leaves lexical scopes active",
                        ));
                    }
                    if state.pending_captures {
                        return Err(failure(
                            Some(absolute_offset),
                            "HALT exits with pending closure captures",
                        ));
                    }
                    terminal = true;
                }
            }

            required_stack = required_stack.max(state.stack.len());
            if required_stack > function.max_stack as usize {
                return Err(failure(
                    Some(absolute_offset),
                    "function max_stack is below statically required stack depth",
                ));
            }
            if required_stack > policy.limits.max_stack_depth as usize {
                return Err(failure(
                    Some(absolute_offset),
                    "statically required stack depth exceeds host limit",
                ));
            }
            if terminal {
                continue;
            }
            if successors.is_empty() {
                let next_index = instruction_index + 1;
                if next_index >= decoded.instructions.len() {
                    return Err(failure(
                        Some(absolute_offset),
                        "reachable path falls through function end",
                    ));
                }
                successors.push((next_index, state));
            }
            for (successor_index, mut incoming) in successors {
                let successor = &decoded.instructions[successor_index];
                let successor_offset = successor.offset as u64;
                for exception in exceptions
                    .iter()
                    .filter(|exception| exception.function_index as usize == function_index)
                {
                    let source_inside = exception.protected_start <= instruction_offset
                        && instruction_offset < exception.protected_end;
                    let successor_inside = exception.protected_start <= successor_offset
                        && successor_offset < exception.protected_end;
                    if !source_inside
                        && successor_inside
                        && (incoming.opaque_prefix
                            || incoming.stack.len() != exception.stack_depth as usize)
                    {
                        return Err(failure(
                            Some(code_base + function.code_offset + successor.offset),
                            "control-flow entry into protected range has incompatible exception stack depth",
                        ));
                    }
                }
                if !incoming
                    .handler_range
                    .is_some_and(|(start, end)| start <= successor_offset && successor_offset < end)
                {
                    incoming.handler_range = None;
                }
                if let Some(current) = &mut states[successor_index] {
                    if merge_flow_state(
                        current,
                        &incoming,
                        function.code_offset + successor.offset,
                    )? {
                        worklist.push_back(successor_index);
                    }
                } else {
                    states[successor_index] = Some(incoming);
                    worklist.push_back(successor_index);
                }
            }
        }

        for exception in exceptions
            .iter()
            .filter(|exception| exception.function_index as usize == function_index)
        {
            let start_offset = usize::try_from(exception.protected_start)
                .map_err(|_| failure(None, "exception start offset overflow"))?;
            let Some(start_index) = decoded.offsets.get(&start_offset).copied() else {
                return Err(failure(
                    Some(code_base + function.code_offset),
                    "exception protected start is not an instruction",
                ));
            };
            if let Some(entry_state) = &states[start_index] {
                if entry_state.opaque_prefix
                    || entry_state.stack.len() != exception.stack_depth as usize
                {
                    return Err(failure(
                        Some(code_base + function.code_offset + start_offset),
                        "exception stack depth does not match protected-entry stack state",
                    ));
                }
            }
        }
        function_stack_depths.push(
            u32::try_from(required_stack)
                .map_err(|_| failure(None, "required stack depth exceeds u32"))?,
        );
    }
    Ok(function_stack_depths)
}

fn verify_exception_and_debug(
    sections: &HashMap<u32, Section>,
    functions: &[FunctionRecord],
    decoded_functions: &[DecodedFunction],
    exceptions: &[ExceptionRecord],
    debug: &DebugInfo,
) -> Result<(), VerificationError> {
    let code_section = sections[&4];
    let mut previous_exception_key = None;
    let mut intervals: HashMap<u32, Vec<(u64, u64, usize)>> = HashMap::new();
    for exception in exceptions {
        let function = &functions[exception.function_index as usize];
        let decoded = &decoded_functions[exception.function_index as usize];
        let function_absolute_offset = code_section.offset + function.code_offset;
        let starts = |offset: u64| decoded.offsets.contains_key(&(offset as usize));
        let function_end = function.code_length as u64;
        if !starts(exception.protected_start)
            || (exception.protected_end != function_end && !starts(exception.protected_end))
            || !starts(exception.handler_offset)
            || (exception.handler_end != function_end && !starts(exception.handler_end))
        {
            return Err(failure(
                Some(function_absolute_offset),
                "exception or handler range does not align to instruction boundaries",
            ));
        }
        let key = (
            exception.function_index,
            exception.protected_start,
            exception.protected_end,
        );
        if previous_exception_key.is_some_and(|previous| previous > key) {
            return Err(failure(
                Some(function_absolute_offset),
                "ExceptionTable is not ordered by function, start, and end",
            ));
        }
        previous_exception_key = Some(key);
        intervals
            .entry(exception.function_index)
            .or_default()
            .push((
                exception.protected_start,
                exception.protected_end,
                function_absolute_offset,
            ));
    }
    for records in intervals.values_mut() {
        records.sort_by_key(|(start, end, _)| (*start, std::cmp::Reverse(*end)));
        let mut nesting: Vec<(u64, u64)> = Vec::new();
        for &(start, end, base) in records.iter() {
            while nesting
                .last()
                .is_some_and(|(_, parent_end)| start >= *parent_end)
            {
                nesting.pop();
            }
            if nesting.last().is_some_and(|(parent_start, parent_end)| {
                (start == *parent_start && end == *parent_end) || end > *parent_end
            }) {
                return Err(failure(
                    Some(base),
                    "exception ranges overlap without proper nesting",
                ));
            }
            nesting.push((start, end));
        }
    }

    for (function_index, (function, decoded)) in functions.iter().zip(decoded_functions).enumerate()
    {
        for instruction in &decoded.instructions {
            if matches!(&instruction.kind, InstructionKind::Rethrow)
                && !exceptions.iter().any(|exception| {
                    exception.function_index as usize == function_index
                        && instruction.offset as u64 >= exception.handler_offset
                        && (instruction.offset as u64) < exception.handler_end
                })
            {
                return Err(failure(
                    Some(code_section.offset + function.code_offset + instruction.offset),
                    "RETHROW is outside every exception handler range",
                ));
            }
        }
    }

    for location in &debug.locations {
        let function = functions
            .get(location.function_index as usize)
            .ok_or_else(|| {
                failure(
                    Some(
                        sections
                            .get(&7)
                            .map_or(code_section.offset, |section| section.offset),
                    ),
                    "DebugInfo function index is out of range",
                )
            })?;
        if location.source_file_index >= debug.source_file_count {
            return Err(failure(
                Some(code_section.offset),
                "DebugInfo source file index is out of range",
            ));
        }
        let function_end = function.code_offset + function.code_length;
        let code_offset = usize::try_from(location.code_offset).map_err(|_| {
            failure(
                Some(code_section.offset),
                "DebugInfo code offset exceeds host range",
            )
        })?;
        if code_offset < function.code_offset || code_offset >= function_end {
            return Err(failure(
                Some(code_section.offset),
                "DebugInfo code offset is outside its function",
            ));
        }
        let local_offset = code_offset - function.code_offset;
        let decoded = &decoded_functions[location.function_index as usize];
        if !decoded.offsets.contains_key(&local_offset) {
            return Err(failure(
                Some(code_section.offset + code_offset),
                "DebugInfo code offset is not an instruction start",
            ));
        }
        if location.start_byte_offset > location.end_byte_offset {
            return Err(failure(
                Some(code_section.offset + code_offset),
                "DebugInfo source span is reversed",
            ));
        }
    }
    Ok(())
}
