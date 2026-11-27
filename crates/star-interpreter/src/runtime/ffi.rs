//! Capability-gated C ABI calls with checked scalar and managed-buffer values.

use std::cell::{RefCell, RefMut};
use std::collections::BTreeSet;
use std::ffi::{c_char, c_void, CString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use libffi::middle::{Arg, Cif, CodePtr, Type};
use libloading::Library;
use num_bigint::BigInt;
use num_traits::ToPrimitive;
use star_compiler::Capability;

/// C ABI argument or result type supported by the safe FFI bridge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeType {
    /// No return value; valid only as a result type.
    Void,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    Isize,
    Usize,
    F32,
    F64,
    /// C `_Bool`, represented as one byte.
    Bool,
    /// NUL-terminated UTF-8 string copied for the duration of the call.
    CString,
    /// Mutable pointer to a managed byte buffer, borrowed for the call.
    Buffer,
}

impl NativeType {
    fn ffi_type(self) -> Type {
        match self {
            Self::Void => Type::void(),
            Self::I8 => Type::i8(),
            Self::U8 | Self::Bool => Type::u8(),
            Self::I16 => Type::i16(),
            Self::U16 => Type::u16(),
            Self::I32 => Type::i32(),
            Self::U32 => Type::u32(),
            Self::I64 => Type::i64(),
            Self::U64 => Type::u64(),
            Self::Isize => Type::isize(),
            Self::Usize => Type::usize(),
            Self::F32 => Type::f32(),
            Self::F64 => Type::f64(),
            Self::CString | Self::Buffer => Type::pointer(),
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "void" => Self::Void,
            "i8" => Self::I8,
            "u8" => Self::U8,
            "i16" => Self::I16,
            "u16" => Self::U16,
            "i32" => Self::I32,
            "u32" => Self::U32,
            "i64" => Self::I64,
            "u64" => Self::U64,
            "isize" => Self::Isize,
            "usize" => Self::Usize,
            "f32" => Self::F32,
            "f64" => Self::F64,
            "bool" => Self::Bool,
            "cstring" => Self::CString,
            "buffer" => Self::Buffer,
            _ => return None,
        })
    }
}

/// A parsed, explicitly declared C ABI function signature.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeSignature {
    arguments: Vec<NativeType>,
    result: NativeType,
}

impl NativeSignature {
    /// Parses `cdecl(arg, ...)->result`; supported pointer arguments are
    /// `cstring` and `buffer`, and pointer results are rejected.
    pub fn parse(source: &str) -> Result<Self, FfiError> {
        let source = source.trim();
        let body = source
            .strip_prefix("cdecl(")
            .ok_or(FfiError::InvalidSignature)?;
        let (arguments, result) = body.split_once(")->").ok_or(FfiError::InvalidSignature)?;
        let result = NativeType::parse(result.trim()).ok_or(FfiError::InvalidSignature)?;
        if matches!(result, NativeType::CString | NativeType::Buffer) {
            return Err(FfiError::InvalidSignature);
        }
        let arguments = if arguments.trim().is_empty() {
            Vec::new()
        } else {
            arguments
                .split(',')
                .map(|argument| {
                    let argument =
                        NativeType::parse(argument.trim()).ok_or(FfiError::InvalidSignature)?;
                    if argument == NativeType::Void {
                        return Err(FfiError::InvalidSignature);
                    }
                    Ok(argument)
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(Self { arguments, result })
    }

    /// Returns the declared argument types.
    pub fn arguments(&self) -> &[NativeType] {
        &self.arguments
    }

    /// Returns the declared result type.
    pub fn result(&self) -> NativeType {
        self.result
    }

    fn cif(&self) -> Result<Cif, FfiError> {
        Cif::try_new(
            self.arguments.iter().copied().map(NativeType::ffi_type),
            self.result.ffi_type(),
        )
        .map_err(|_| FfiError::InvalidSignature)
    }
}

/// Values accepted by a declared native call.
#[derive(Clone, Debug)]
pub enum NativeValue {
    Void,
    Int(BigInt),
    Float(f64),
    Bool(bool),
    String(String),
    Buffer(NativeBuffer),
}

/// A managed zero-initialized native byte buffer.
#[derive(Clone, Debug)]
pub struct NativeBuffer(Rc<RefCell<Vec<u8>>>);

impl NativeBuffer {
    /// Allocates a zero-filled buffer no larger than `max_bytes`.
    pub fn new(length: usize, max_bytes: usize) -> Result<Self, FfiError> {
        if length > max_bytes {
            return Err(FfiError::BufferTooLarge { max_bytes });
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| FfiError::AllocationFailed)?;
        bytes.resize(length, 0);
        Ok(Self(Rc::new(RefCell::new(bytes))))
    }

    /// Copies the current buffer bytes.
    pub fn read(&self) -> Result<Vec<u8>, FfiError> {
        self.0
            .try_borrow()
            .map(|bytes| bytes.clone())
            .map_err(|_| FfiError::BufferBorrowed)
    }

    /// Writes bytes at `offset`, failing without a partial write when out of range.
    pub fn write(&self, offset: usize, input: &[u8]) -> Result<(), FfiError> {
        let mut bytes = self
            .0
            .try_borrow_mut()
            .map_err(|_| FfiError::BufferBorrowed)?;
        let end = offset
            .checked_add(input.len())
            .ok_or(FfiError::OutOfRange)?;
        let target = bytes.get_mut(offset..end).ok_or(FfiError::OutOfRange)?;
        target.copy_from_slice(input);
        Ok(())
    }

    /// Returns the fixed buffer length.
    pub fn len(&self) -> Result<usize, FfiError> {
        self.0
            .try_borrow()
            .map(|bytes| bytes.len())
            .map_err(|_| FfiError::BufferBorrowed)
    }

    /// Returns whether the buffer has zero length.
    pub fn is_empty(&self) -> Result<bool, FfiError> {
        self.len().map(|length| length == 0)
    }
}

/// A managed handle to a loaded library, bound function, or buffer.
#[derive(Clone, Debug)]
pub enum NativeHandle {
    /// Dynamic library handle.
    Library(NativeLibraryHandle),
    /// Symbol and its declared C ABI signature.
    Function(NativeFunctionHandle),
    /// Managed native byte buffer.
    Buffer(NativeBuffer),
}

impl NativeHandle {
    pub(crate) fn kind_name(&self) -> &'static str {
        match self {
            Self::Library(_) => "Library",
            Self::Function(_) => "Function",
            Self::Buffer(_) => "Buffer",
        }
    }

    pub(crate) fn same_handle(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Library(left), Self::Library(right)) => Rc::ptr_eq(&left.0, &right.0),
            (Self::Function(left), Self::Function(right)) => Rc::ptr_eq(&left.0, &right.0),
            (Self::Buffer(left), Self::Buffer(right)) => Rc::ptr_eq(&left.0, &right.0),
            _ => false,
        }
    }

    pub(crate) fn estimated_bytes(&self) -> usize {
        match self {
            Self::Buffer(buffer) => buffer.0.try_borrow().map_or(0, |bytes| bytes.capacity()),
            Self::Library(_) | Self::Function(_) => 0,
        }
    }
}

/// A loaded dynamic library. Closing it invalidates its symbol handles.
#[derive(Clone, Debug)]
pub struct NativeLibraryHandle(Rc<NativeLibraryInner>);

#[derive(Debug)]
struct NativeLibraryInner {
    path: PathBuf,
    library: RefCell<Option<Library>>,
}

impl NativeLibraryHandle {
    /// Loads a library only when the host granted the `ffi` capability.
    /// Loading runs the library's platform initialization code.
    pub fn load(
        path: impl AsRef<Path>,
        capabilities: &BTreeSet<Capability>,
    ) -> Result<Self, FfiError> {
        require_ffi(capabilities)?;
        let path = path.as_ref();
        let library =
            unsafe { Library::new(path) }.map_err(|error| FfiError::Load(error.to_string()))?;
        Ok(Self(Rc::new(NativeLibraryInner {
            path: path.to_path_buf(),
            library: RefCell::new(Some(library)),
        })))
    }

    /// Returns the host library path recorded at load time.
    pub fn path(&self) -> &Path {
        &self.0.path
    }

    /// Resolves `name` with an explicit signature under the `ffi` capability.
    pub fn symbol(
        &self,
        name: &str,
        signature: NativeSignature,
        capabilities: &BTreeSet<Capability>,
    ) -> Result<NativeFunctionHandle, FfiError> {
        require_ffi(capabilities)?;
        if name.as_bytes().contains(&0) {
            return Err(FfiError::InvalidSymbolName);
        }
        let library = self
            .0
            .library
            .try_borrow()
            .map_err(|_| FfiError::LibraryBorrowed)?;
        let library = library.as_ref().ok_or(FfiError::LibraryClosed)?;
        let symbol = unsafe { library.get::<unsafe extern "C" fn()>(name.as_bytes()) }
            .map_err(|error| FfiError::Symbol(error.to_string()))?;
        let address = *symbol as *const () as *mut c_void;
        let cif = signature.cif()?;
        Ok(NativeFunctionHandle(Rc::new(NativeFunctionInner {
            library: Rc::downgrade(&self.0),
            address,
            signature,
            cif,
        })))
    }

    /// Unloads the library and invalidates all symbol handles.
    pub fn close(&self, capabilities: &BTreeSet<Capability>) -> Result<(), FfiError> {
        require_ffi(capabilities)?;
        let mut library = self
            .0
            .library
            .try_borrow_mut()
            .map_err(|_| FfiError::LibraryBorrowed)?;
        if library.take().is_none() {
            return Err(FfiError::LibraryClosed);
        }
        Ok(())
    }
}

/// A native function pointer bound to a checked ABI signature.
#[derive(Clone, Debug)]
pub struct NativeFunctionHandle(Rc<NativeFunctionInner>);

#[derive(Debug)]
struct NativeFunctionInner {
    library: Weak<NativeLibraryInner>,
    address: *mut c_void,
    signature: NativeSignature,
    cif: Cif,
}

enum ArgumentValue<'a> {
    I8(i8),
    U8(u8),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    Isize(isize),
    Usize(usize),
    F32(f32),
    F64(f64),
    Bool(u8),
    CString {
        _value: CString,
        pointer: *const c_char,
    },
    Buffer {
        _value: RefMut<'a, Vec<u8>>,
        pointer: *mut c_void,
    },
}

impl ArgumentValue<'_> {
    fn arg(&self) -> Arg<'_> {
        match self {
            Self::I8(value) => Arg::new(value),
            Self::U8(value) => Arg::new(value),
            Self::I16(value) => Arg::new(value),
            Self::U16(value) => Arg::new(value),
            Self::I32(value) => Arg::new(value),
            Self::U32(value) => Arg::new(value),
            Self::I64(value) => Arg::new(value),
            Self::U64(value) => Arg::new(value),
            Self::Isize(value) => Arg::new(value),
            Self::Usize(value) => Arg::new(value),
            Self::F32(value) => Arg::new(value),
            Self::F64(value) => Arg::new(value),
            Self::Bool(value) => Arg::new(value),
            Self::CString { pointer, .. } => Arg::new(pointer),
            Self::Buffer { pointer, .. } => Arg::new(pointer),
        }
    }
}

impl NativeFunctionHandle {
    /// Returns the declared signature for this symbol.
    pub fn signature(&self) -> &NativeSignature {
        &self.0.signature
    }

    /// Calls the symbol after checking capabilities, value kinds, and integer ranges.
    pub fn call(
        &self,
        arguments: &[NativeValue],
        capabilities: &BTreeSet<Capability>,
    ) -> Result<NativeValue, FfiError> {
        require_ffi(capabilities)?;
        if arguments.len() != self.0.signature.arguments.len() {
            return Err(FfiError::ArgumentCount {
                expected: self.0.signature.arguments.len(),
                found: arguments.len(),
            });
        }
        let library = self.0.library.upgrade().ok_or(FfiError::LibraryClosed)?;
        let library_guard = library
            .library
            .try_borrow()
            .map_err(|_| FfiError::LibraryBorrowed)?;
        if library_guard.is_none() {
            return Err(FfiError::LibraryClosed);
        }
        let mut stored = Vec::with_capacity(arguments.len());
        for (index, (argument, expected)) in arguments
            .iter()
            .zip(&self.0.signature.arguments)
            .enumerate()
        {
            stored.push(prepare_argument(argument, *expected, index)?);
        }
        let ffi_arguments = stored.iter().map(ArgumentValue::arg).collect::<Vec<_>>();
        let code = CodePtr(self.0.address);
        let result = call_result(&self.0.cif, code, &ffi_arguments, self.0.signature.result)?;
        drop(library_guard);
        Ok(result)
    }
}

fn prepare_argument<'a>(
    value: &'a NativeValue,
    expected: NativeType,
    index: usize,
) -> Result<ArgumentValue<'a>, FfiError> {
    let type_error = || FfiError::ArgumentType {
        index,
        expected,
        found: value.name(),
    };
    let integer = || match value {
        NativeValue::Int(value) => Ok(value),
        _ => Err(type_error()),
    };
    Ok(match expected {
        NativeType::I8 => ArgumentValue::I8(
            integer()?
                .to_i8()
                .ok_or(FfiError::IntegerOutOfRange { index })?,
        ),
        NativeType::U8 => ArgumentValue::U8(
            integer()?
                .to_u8()
                .ok_or(FfiError::IntegerOutOfRange { index })?,
        ),
        NativeType::I16 => ArgumentValue::I16(
            integer()?
                .to_i16()
                .ok_or(FfiError::IntegerOutOfRange { index })?,
        ),
        NativeType::U16 => ArgumentValue::U16(
            integer()?
                .to_u16()
                .ok_or(FfiError::IntegerOutOfRange { index })?,
        ),
        NativeType::I32 => ArgumentValue::I32(
            integer()?
                .to_i32()
                .ok_or(FfiError::IntegerOutOfRange { index })?,
        ),
        NativeType::U32 => ArgumentValue::U32(
            integer()?
                .to_u32()
                .ok_or(FfiError::IntegerOutOfRange { index })?,
        ),
        NativeType::I64 => ArgumentValue::I64(
            integer()?
                .to_i64()
                .ok_or(FfiError::IntegerOutOfRange { index })?,
        ),
        NativeType::U64 => ArgumentValue::U64(
            integer()?
                .to_u64()
                .ok_or(FfiError::IntegerOutOfRange { index })?,
        ),
        NativeType::Isize => ArgumentValue::Isize(
            integer()?
                .to_isize()
                .ok_or(FfiError::IntegerOutOfRange { index })?,
        ),
        NativeType::Usize => ArgumentValue::Usize(
            integer()?
                .to_usize()
                .ok_or(FfiError::IntegerOutOfRange { index })?,
        ),
        NativeType::F32 => match value {
            NativeValue::Float(value) => {
                let converted = *value as f32;
                if value.is_finite() && !converted.is_finite() {
                    return Err(FfiError::FloatOutOfRange { index });
                }
                ArgumentValue::F32(converted)
            }
            _ => return Err(type_error()),
        },
        NativeType::F64 => match value {
            NativeValue::Float(value) => ArgumentValue::F64(*value),
            _ => return Err(type_error()),
        },
        NativeType::Bool => match value {
            NativeValue::Bool(value) => ArgumentValue::Bool(u8::from(*value)),
            _ => return Err(type_error()),
        },
        NativeType::CString => match value {
            NativeValue::String(value) => {
                let owned =
                    CString::new(value.as_str()).map_err(|_| FfiError::InteriorNul { index })?;
                let pointer = owned.as_ptr();
                ArgumentValue::CString {
                    _value: owned,
                    pointer,
                }
            }
            _ => return Err(type_error()),
        },
        NativeType::Buffer => match value {
            NativeValue::Buffer(buffer) => {
                let guard = buffer
                    .0
                    .try_borrow_mut()
                    .map_err(|_| FfiError::BufferBorrowed)?;
                let pointer = guard.as_ptr() as *mut c_void;
                ArgumentValue::Buffer {
                    _value: guard,
                    pointer,
                }
            }
            _ => return Err(type_error()),
        },
        NativeType::Void => return Err(FfiError::InvalidSignature),
    })
}

fn call_result(
    cif: &Cif,
    function: CodePtr,
    arguments: &[Arg<'_>],
    result: NativeType,
) -> Result<NativeValue, FfiError> {
    macro_rules! integer_call {
        ($type:ty) => {{
            let value: $type = unsafe { cif.call(function, arguments) };
            NativeValue::Int(BigInt::from(value))
        }};
    }
    Ok(match result {
        NativeType::Void => {
            let _: () = unsafe { cif.call(function, arguments) };
            NativeValue::Void
        }
        NativeType::I8 => integer_call!(i8),
        NativeType::U8 => integer_call!(u8),
        NativeType::I16 => integer_call!(i16),
        NativeType::U16 => integer_call!(u16),
        NativeType::I32 => integer_call!(i32),
        NativeType::U32 => integer_call!(u32),
        NativeType::I64 => integer_call!(i64),
        NativeType::U64 => integer_call!(u64),
        NativeType::Isize => integer_call!(isize),
        NativeType::Usize => integer_call!(usize),
        NativeType::Bool => NativeValue::Bool(unsafe { cif.call::<u8>(function, arguments) } != 0),
        NativeType::F32 => {
            NativeValue::Float(unsafe { cif.call::<f32>(function, arguments) } as f64)
        }
        NativeType::F64 => NativeValue::Float(unsafe { cif.call::<f64>(function, arguments) }),
        NativeType::CString | NativeType::Buffer => return Err(FfiError::InvalidSignature),
    })
}

impl NativeValue {
    fn name(&self) -> &'static str {
        match self {
            Self::Void => "Void",
            Self::Int(_) => "Int",
            Self::Float(_) => "Float",
            Self::Bool(_) => "Bool",
            Self::String(_) => "String",
            Self::Buffer(_) => "Buffer",
        }
    }
}

fn require_ffi(capabilities: &BTreeSet<Capability>) -> Result<(), FfiError> {
    if capabilities.contains(&Capability::Ffi) {
        Ok(())
    } else {
        Err(FfiError::PermissionDenied)
    }
}

/// Errors raised while parsing a C signature, managing a handle, or calling native code.
#[derive(Debug)]
pub enum FfiError {
    PermissionDenied,
    InvalidSignature,
    InvalidSymbolName,
    Load(String),
    Symbol(String),
    LibraryClosed,
    LibraryBorrowed,
    ArgumentCount {
        expected: usize,
        found: usize,
    },
    ArgumentType {
        index: usize,
        expected: NativeType,
        found: &'static str,
    },
    IntegerOutOfRange {
        index: usize,
    },
    FloatOutOfRange {
        index: usize,
    },
    InteriorNul {
        index: usize,
    },
    BufferTooLarge {
        max_bytes: usize,
    },
    AllocationFailed,
    BufferBorrowed,
    OutOfRange,
}

impl fmt::Display for FfiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PermissionDenied => formatter.write_str("missing ffi capability"),
            Self::InvalidSignature => formatter.write_str("invalid or unsupported C ABI signature"),
            Self::InvalidSymbolName => formatter.write_str("native symbol name contains NUL"),
            Self::Load(error) => write!(formatter, "native library load failed: {error}"),
            Self::Symbol(error) => write!(formatter, "native symbol lookup failed: {error}"),
            Self::LibraryClosed => formatter.write_str("native library is closed"),
            Self::LibraryBorrowed => formatter.write_str("native library is in use"),
            Self::ArgumentCount { expected, found } => write!(
                formatter,
                "native call expects {expected} arguments, got {found}"
            ),
            Self::ArgumentType {
                index,
                expected,
                found,
            } => write!(
                formatter,
                "native argument {index} expects {expected:?}, got {found}"
            ),
            Self::IntegerOutOfRange { index } => {
                write!(formatter, "native integer argument {index} is out of range")
            }
            Self::FloatOutOfRange { index } => {
                write!(formatter, "native float argument {index} is out of range")
            }
            Self::InteriorNul { index } => {
                write!(formatter, "native C string argument {index} contains NUL")
            }
            Self::BufferTooLarge { max_bytes } => {
                write!(formatter, "native buffer exceeds {max_bytes} bytes")
            }
            Self::AllocationFailed => formatter.write_str("native buffer allocation failed"),
            Self::BufferBorrowed => formatter.write_str("native buffer is already borrowed"),
            Self::OutOfRange => formatter.write_str("native buffer range is out of bounds"),
        }
    }
}

impl std::error::Error for FfiError {}
