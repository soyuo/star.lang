//! Capability-gated C ABI calls with checked scalar and managed-buffer values.

use std::cell::{RefCell, RefMut};
use std::collections::BTreeSet;
use std::ffi::{c_char, c_void, CString};
use std::fmt;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::ptr;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Mutex;
use std::thread::ThreadId;

use libffi::middle::{Arg, Cif, Closure, CodePtr, Type};
use libloading::Library;
use num_bigint::BigInt;
use num_traits::ToPrimitive;
use star_compiler::Capability;
use star_parser::Span;

use crate::value::Value;

use super::RuntimeError;

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
    /// Synchronous scalar-only callback pointer.
    Callback,
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
            Self::CString | Self::Buffer | Self::Callback => Type::pointer(),
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
            "callback" => Self::Callback,
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
        if matches!(
            result,
            NativeType::CString | NativeType::Buffer | NativeType::Callback
        ) {
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
    Callback(NativeCallbackHandle),
}

/// Re-entry hooks available only while one native function call is active.
pub(crate) struct NativeCallbackContext {
    data: *mut c_void,
    invoke: unsafe fn(*mut c_void, &Value, &[Value], Span) -> Result<Value, RuntimeError>,
    preflight: unsafe fn(*mut c_void, usize, Span) -> Result<(), RuntimeError>,
    span: Span,
    failure: Option<CallbackFailure>,
}

impl NativeCallbackContext {
    pub(crate) fn new(
        data: *mut c_void,
        invoke: unsafe fn(*mut c_void, &Value, &[Value], Span) -> Result<Value, RuntimeError>,
        preflight: unsafe fn(*mut c_void, usize, Span) -> Result<(), RuntimeError>,
        span: Span,
    ) -> Self {
        Self {
            data,
            invoke,
            preflight,
            span,
            failure: None,
        }
    }

    fn take_failure(&mut self) -> Option<CallbackFailure> {
        self.failure.take()
    }
}

enum CallbackFailure {
    Runtime(RuntimeError),
    Ffi(FfiError),
}

pub(crate) enum NativeCallError {
    Runtime(RuntimeError),
    Ffi(FfiError),
}

impl NativeCallError {
    pub(crate) fn into_ffi(self) -> FfiError {
        match self {
            Self::Runtime(_) => FfiError::CallbackFailed,
            Self::Ffi(error) => error,
        }
    }
}

impl From<FfiError> for NativeCallError {
    fn from(error: FfiError) -> Self {
        Self::Ffi(error)
    }
}

impl From<CallbackFailure> for NativeCallError {
    fn from(failure: CallbackFailure) -> Self {
        match failure {
            CallbackFailure::Runtime(error) => Self::Runtime(error),
            CallbackFailure::Ffi(error) => Self::Ffi(error),
        }
    }
}

#[derive(Debug)]
struct NativeCallbackState {
    callable: Value,
    signature: NativeSignature,
    context: AtomicPtr<NativeCallbackContext>,
    active_thread: Mutex<Option<ThreadId>>,
    thread_violation: std::sync::atomic::AtomicBool,
}

#[derive(Debug)]
struct NativeCallbackInner {
    // The closure is dropped before its boxed userdata state.
    closure: Closure<'static>,
    state: Box<NativeCallbackState>,
}

/// Synchronous native callback with a scalar-only C ABI signature.
#[derive(Clone, Debug)]
pub struct NativeCallbackHandle(Rc<NativeCallbackInner>);

struct CallbackContextGuard {
    callback: NativeCallbackHandle,
    previous: *mut NativeCallbackContext,
    previous_thread: Option<ThreadId>,
    previous_violation: bool,
}

impl Drop for CallbackContextGuard {
    fn drop(&mut self) {
        self.callback
            .0
            .state
            .context
            .store(self.previous, Ordering::Release);
        if let Ok(mut active_thread) = self.callback.0.state.active_thread.lock() {
            *active_thread = self.previous_thread;
        }
        self.callback
            .0
            .state
            .thread_violation
            .store(self.previous_violation, Ordering::Release);
    }
}

impl NativeCallbackHandle {
    /// Creates a callback that can only run synchronously during an active
    /// native call. Native code must not retain its code pointer afterward.
    pub fn new(
        callable: Value,
        signature: NativeSignature,
        capabilities: &BTreeSet<Capability>,
    ) -> Result<Self, FfiError> {
        require_ffi(capabilities)?;
        if !matches!(callable, Value::Function(_) | Value::Closure(_))
            || signature.arguments.iter().any(|ty| {
                matches!(
                    ty,
                    NativeType::Void
                        | NativeType::CString
                        | NativeType::Buffer
                        | NativeType::Callback
                )
            })
            || matches!(
                signature.result,
                NativeType::CString | NativeType::Buffer | NativeType::Callback
            )
        {
            return Err(FfiError::InvalidSignature);
        }
        let cif = signature.cif()?;
        let state = Box::new(NativeCallbackState {
            callable,
            signature,
            context: AtomicPtr::new(ptr::null_mut()),
            active_thread: Mutex::new(None),
            thread_violation: std::sync::atomic::AtomicBool::new(false),
        });
        let state_ptr: *const NativeCallbackState = &*state;
        // SAFETY: NativeCallbackInner stores the Box beside the Closure and
        // drops the Closure first, keeping this userdata address stable.
        let state_ref: &'static NativeCallbackState = unsafe { &*state_ptr };
        let closure = match state_ref.signature.result {
            NativeType::Void => Closure::try_new(cif, callback_void, state_ref),
            NativeType::I8 => Closure::try_new(cif, callback_i8, state_ref),
            NativeType::U8 => Closure::try_new(cif, callback_u8, state_ref),
            NativeType::I16 => Closure::try_new(cif, callback_i16, state_ref),
            NativeType::U16 => Closure::try_new(cif, callback_u16, state_ref),
            NativeType::I32 => Closure::try_new(cif, callback_i32, state_ref),
            NativeType::U32 => Closure::try_new(cif, callback_u32, state_ref),
            NativeType::I64 => Closure::try_new(cif, callback_i64, state_ref),
            NativeType::U64 => Closure::try_new(cif, callback_u64, state_ref),
            NativeType::Isize => Closure::try_new(cif, callback_isize, state_ref),
            NativeType::Usize => Closure::try_new(cif, callback_usize, state_ref),
            NativeType::F32 => Closure::try_new(cif, callback_f32, state_ref),
            NativeType::F64 => Closure::try_new(cif, callback_f64, state_ref),
            NativeType::Bool => Closure::try_new(cif, callback_bool, state_ref),
            NativeType::CString | NativeType::Buffer | NativeType::Callback => {
                return Err(FfiError::InvalidSignature)
            }
        }
        .map_err(|_| FfiError::AllocationFailed)?;
        Ok(Self(Rc::new(NativeCallbackInner { closure, state })))
    }

    fn bind(&self, context: &mut NativeCallbackContext) -> CallbackContextGuard {
        let previous_violation = self.0.state.thread_violation.swap(false, Ordering::AcqRel);
        let previous_thread = self
            .0
            .state
            .active_thread
            .lock()
            .map(|mut active_thread| {
                let previous = *active_thread;
                *active_thread = Some(std::thread::current().id());
                previous
            })
            .unwrap_or(None);
        let previous = self
            .0
            .state
            .context
            .swap(context as *mut _, Ordering::AcqRel);
        CallbackContextGuard {
            callback: self.clone(),
            previous,
            previous_thread,
            previous_violation,
        }
    }

    fn code_pointer(&self) -> *mut c_void {
        *self.0.closure.code_ptr() as *const () as *mut c_void
    }

    pub(crate) fn callable(&self) -> &Value {
        &self.0.state.callable
    }

    fn had_thread_violation(&self) -> bool {
        self.0.state.thread_violation.load(Ordering::Acquire)
    }

    pub(crate) fn estimated_bytes(&self) -> usize {
        size_of::<NativeCallbackInner>()
            .saturating_add(size_of::<NativeCallbackState>())
            .saturating_add(match &self.0.state.callable {
                Value::Function(function) => super::estimated_bigint_bytes(&function.id),
                Value::Closure(closure) => super::estimated_bigint_bytes(&closure.function.id),
                _ => 0,
            })
            .saturating_add(
                self.0
                    .state
                    .signature
                    .arguments
                    .capacity()
                    .saturating_mul(size_of::<NativeType>()),
            )
    }
}

fn dispatch_callback(
    state: &NativeCallbackState,
    raw_arguments: *const *const c_void,
) -> Option<NativeValue> {
    let dispatched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        dispatch_callback_inner(state, raw_arguments)
    }));
    match dispatched {
        Ok(value) => value,
        Err(_) => {
            if state
                .active_thread
                .lock()
                .map(|active_thread| *active_thread == Some(std::thread::current().id()))
                .unwrap_or(false)
            {
                let context_ptr = state.context.load(Ordering::Acquire);
                if !context_ptr.is_null() {
                    // SAFETY: this is the active synchronous callback thread.
                    unsafe { &mut *context_ptr }.failure =
                        Some(CallbackFailure::Ffi(FfiError::CallbackPanicked));
                }
            }
            None
        }
    }
}

fn dispatch_callback_inner(
    state: &NativeCallbackState,
    raw_arguments: *const *const c_void,
) -> Option<NativeValue> {
    if state
        .active_thread
        .lock()
        .map(|active_thread| *active_thread != Some(std::thread::current().id()))
        .unwrap_or(true)
    {
        state.thread_violation.store(true, Ordering::Release);
        return None;
    }
    let context_ptr = state.context.load(Ordering::Acquire);
    if context_ptr.is_null() {
        return None;
    }
    // SAFETY: bind() installs this pointer only for the duration of the
    // synchronous native call, and the call guard clears/restores it afterward.
    let context = unsafe { &mut *context_ptr };
    if context.failure.is_some() {
        return None;
    }
    let guest_bytes = state.signature.arguments.len().saturating_mul(
        size_of::<Value>()
            .saturating_add(size_of::<NativeValue>())
            .saturating_add(32),
    );
    if let Err(error) = unsafe { (context.preflight)(context.data, guest_bytes, context.span) } {
        context.failure = Some(CallbackFailure::Runtime(error));
        return None;
    }
    let mut guest_arguments = Vec::new();
    if guest_arguments
        .try_reserve_exact(state.signature.arguments.len())
        .is_err()
    {
        context.failure = Some(CallbackFailure::Ffi(FfiError::AllocationFailed));
        return None;
    }
    for (index, ty) in state.signature.arguments.iter().enumerate() {
        let pointer = unsafe { raw_arguments.add(index).read() };
        if pointer.is_null() {
            context.failure = Some(CallbackFailure::Ffi(FfiError::InvalidSignature));
            return None;
        }
        let argument = unsafe { decode_callback_argument(*ty, pointer) };
        let Some(argument) = argument else {
            context.failure = Some(CallbackFailure::Ffi(FfiError::InvalidSignature));
            return None;
        };
        guest_arguments.push(argument);
    }
    let invoked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        (context.invoke)(
            context.data,
            &state.callable,
            &guest_arguments,
            context.span,
        )
    }));
    let guest_result = match invoked {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            context.failure = Some(CallbackFailure::Runtime(error));
            return None;
        }
        Err(_) => {
            context.failure = Some(CallbackFailure::Ffi(FfiError::CallbackPanicked));
            return None;
        }
    };
    match convert_callback_result(guest_result, state.signature.result) {
        Ok(value) => Some(value),
        Err(error) => {
            context.failure = Some(CallbackFailure::Ffi(error));
            None
        }
    }
}

unsafe fn decode_callback_argument(ty: NativeType, pointer: *const c_void) -> Option<Value> {
    macro_rules! read_int {
        ($type:ty) => {
            Value::Int(BigInt::from(unsafe {
                ptr::read_unaligned(pointer.cast::<$type>())
            }))
        };
    }
    Some(match ty {
        NativeType::I8 => read_int!(i8),
        NativeType::U8 => read_int!(u8),
        NativeType::I16 => read_int!(i16),
        NativeType::U16 => read_int!(u16),
        NativeType::I32 => read_int!(i32),
        NativeType::U32 => read_int!(u32),
        NativeType::I64 => read_int!(i64),
        NativeType::U64 => read_int!(u64),
        NativeType::Isize => read_int!(isize),
        NativeType::Usize => read_int!(usize),
        NativeType::F32 => {
            Value::Float(unsafe { ptr::read_unaligned(pointer.cast::<f32>()) } as f64)
        }
        NativeType::F64 => Value::Float(unsafe { ptr::read_unaligned(pointer.cast::<f64>()) }),
        NativeType::Bool => Value::Bool(unsafe { ptr::read_unaligned(pointer.cast::<u8>()) } != 0),
        NativeType::Void | NativeType::CString | NativeType::Buffer | NativeType::Callback => {
            return None
        }
    })
}

fn convert_callback_result(value: Value, ty: NativeType) -> Result<NativeValue, FfiError> {
    match (ty, value) {
        (NativeType::Void, _) => Ok(NativeValue::Void),
        (NativeType::Bool, Value::Bool(value)) => Ok(NativeValue::Bool(value)),
        (NativeType::F32, Value::Float(value)) if (value as f32).is_finite() => {
            Ok(NativeValue::Float((value as f32) as f64))
        }
        (NativeType::F64, Value::Float(value)) => Ok(NativeValue::Float(value)),
        (ty, Value::Int(value)) if is_integer_type(ty) => {
            let in_range = match ty {
                NativeType::I8 => value.to_i8().is_some(),
                NativeType::U8 => value.to_u8().is_some(),
                NativeType::I16 => value.to_i16().is_some(),
                NativeType::U16 => value.to_u16().is_some(),
                NativeType::I32 => value.to_i32().is_some(),
                NativeType::U32 => value.to_u32().is_some(),
                NativeType::I64 => value.to_i64().is_some(),
                NativeType::U64 => value.to_u64().is_some(),
                NativeType::Isize => value.to_isize().is_some(),
                NativeType::Usize => value.to_usize().is_some(),
                _ => false,
            };
            if in_range {
                Ok(NativeValue::Int(value))
            } else {
                Err(FfiError::CallbackResultOutOfRange)
            }
        }
        _ => Err(FfiError::CallbackResultType),
    }
}

fn is_integer_type(ty: NativeType) -> bool {
    matches!(
        ty,
        NativeType::I8
            | NativeType::U8
            | NativeType::I16
            | NativeType::U16
            | NativeType::I32
            | NativeType::U32
            | NativeType::I64
            | NativeType::U64
            | NativeType::Isize
            | NativeType::Usize
    )
}

unsafe extern "C" fn callback_void(
    _cif: &libffi::low::ffi_cif,
    _result: &mut (),
    arguments: *const *const c_void,
    state: &NativeCallbackState,
) {
    let _ = dispatch_callback(state, arguments);
}

macro_rules! integer_callback {
    ($name:ident, $type:ty, $conversion:ident) => {
        unsafe extern "C" fn $name(
            _cif: &libffi::low::ffi_cif,
            result: &mut $type,
            arguments: *const *const c_void,
            state: &NativeCallbackState,
        ) {
            *result = match dispatch_callback(state, arguments) {
                Some(NativeValue::Int(value)) => value.$conversion().unwrap_or_default(),
                _ => 0,
            };
        }
    };
}

integer_callback!(callback_i8, i8, to_i8);
integer_callback!(callback_u8, u8, to_u8);
integer_callback!(callback_i16, i16, to_i16);
integer_callback!(callback_u16, u16, to_u16);
integer_callback!(callback_i32, i32, to_i32);
integer_callback!(callback_u32, u32, to_u32);
integer_callback!(callback_i64, i64, to_i64);
integer_callback!(callback_u64, u64, to_u64);
integer_callback!(callback_isize, isize, to_isize);
integer_callback!(callback_usize, usize, to_usize);

unsafe extern "C" fn callback_f32(
    _cif: &libffi::low::ffi_cif,
    result: &mut f32,
    arguments: *const *const c_void,
    state: &NativeCallbackState,
) {
    *result = match dispatch_callback(state, arguments) {
        Some(NativeValue::Float(value)) => value as f32,
        _ => 0.0,
    };
}

unsafe extern "C" fn callback_f64(
    _cif: &libffi::low::ffi_cif,
    result: &mut f64,
    arguments: *const *const c_void,
    state: &NativeCallbackState,
) {
    *result = match dispatch_callback(state, arguments) {
        Some(NativeValue::Float(value)) => value,
        _ => 0.0,
    };
}

unsafe extern "C" fn callback_bool(
    _cif: &libffi::low::ffi_cif,
    result: &mut u8,
    arguments: *const *const c_void,
    state: &NativeCallbackState,
) {
    *result = match dispatch_callback(state, arguments) {
        Some(NativeValue::Bool(value)) => u8::from(value),
        _ => 0,
    };
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
    /// Native-callable Star function callback.
    Callback(NativeCallbackHandle),
}

impl NativeHandle {
    pub(crate) fn kind_name(&self) -> &'static str {
        match self {
            Self::Library(_) => "Library",
            Self::Function(_) => "Function",
            Self::Buffer(_) => "Buffer",
            Self::Callback(_) => "Callback",
        }
    }

    pub(crate) fn same_handle(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Library(left), Self::Library(right)) => Rc::ptr_eq(&left.0, &right.0),
            (Self::Function(left), Self::Function(right)) => Rc::ptr_eq(&left.0, &right.0),
            (Self::Buffer(left), Self::Buffer(right)) => Rc::ptr_eq(&left.0, &right.0),
            (Self::Callback(left), Self::Callback(right)) => Rc::ptr_eq(&left.0, &right.0),
            _ => false,
        }
    }

    pub(crate) fn estimated_bytes(&self) -> usize {
        match self {
            Self::Buffer(buffer) => buffer.0.try_borrow().map_or(0, |bytes| bytes.capacity()),
            Self::Callback(callback) => callback.estimated_bytes(),
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
    Callback {
        _value: NativeCallbackHandle,
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
            Self::Callback { pointer, .. } => Arg::new(pointer),
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
        self.call_scoped(arguments, capabilities, None)
            .map_err(NativeCallError::into_ffi)
    }

    pub(crate) fn call_scoped(
        &self,
        arguments: &[NativeValue],
        capabilities: &BTreeSet<Capability>,
        mut callback_context: Option<&mut NativeCallbackContext>,
    ) -> Result<NativeValue, NativeCallError> {
        require_ffi(capabilities)?;
        if arguments.len() != self.0.signature.arguments.len() {
            return Err(FfiError::ArgumentCount {
                expected: self.0.signature.arguments.len(),
                found: arguments.len(),
            }
            .into());
        }
        let library = self.0.library.upgrade().ok_or(FfiError::LibraryClosed)?;
        let library_guard = library
            .library
            .try_borrow()
            .map_err(|_| FfiError::LibraryBorrowed)?;
        if library_guard.is_none() {
            return Err(FfiError::LibraryClosed.into());
        }
        let mut stored = Vec::with_capacity(arguments.len());
        let mut callback_guards = Vec::new();
        for (index, (argument, expected)) in arguments
            .iter()
            .zip(&self.0.signature.arguments)
            .enumerate()
        {
            let prepared = if *expected == NativeType::Callback {
                let NativeValue::Callback(callback) = argument else {
                    return Err(FfiError::ArgumentType {
                        index,
                        expected: *expected,
                        found: argument.name(),
                    }
                    .into());
                };
                let context = callback_context
                    .as_deref_mut()
                    .ok_or(FfiError::CallbackContextRequired)?;
                callback_guards.push(callback.bind(context));
                ArgumentValue::Callback {
                    pointer: callback.code_pointer(),
                    _value: callback.clone(),
                }
            } else {
                prepare_argument(argument, *expected, index)?
            };
            stored.push(prepared);
        }
        let ffi_arguments = stored.iter().map(ArgumentValue::arg).collect::<Vec<_>>();
        let code = CodePtr(self.0.address);
        let result = call_result(&self.0.cif, code, &ffi_arguments, self.0.signature.result);
        let callback_thread_violation = arguments.iter().any(|argument| match argument {
            NativeValue::Callback(callback) => callback.had_thread_violation(),
            _ => false,
        });
        drop(ffi_arguments);
        drop(stored);
        drop(callback_guards);
        drop(library_guard);
        if callback_thread_violation {
            return Err(FfiError::CallbackWrongThread.into());
        }
        if let Some(context) = callback_context {
            if let Some(failure) = context.take_failure() {
                return Err(failure.into());
            }
        }
        result.map_err(Into::into)
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
        NativeType::Callback => return Err(FfiError::CallbackContextRequired),
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
        NativeType::CString | NativeType::Buffer | NativeType::Callback => {
            return Err(FfiError::InvalidSignature)
        }
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
            Self::Callback(_) => "Callback",
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
    CallbackContextRequired,
    CallbackFailed,
    CallbackPanicked,
    CallbackWrongThread,
    CallbackResultOutOfRange,
    CallbackResultType,
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
            Self::CallbackContextRequired => {
                formatter.write_str("native callback requires an active call context")
            }
            Self::CallbackFailed => formatter.write_str("native callback failed"),
            Self::CallbackPanicked => formatter.write_str("native callback panicked"),
            Self::CallbackWrongThread => {
                formatter.write_str("native callback invoked from another thread")
            }
            Self::CallbackResultOutOfRange => {
                formatter.write_str("native callback result is out of range")
            }
            Self::CallbackResultType => {
                formatter.write_str("native callback returned an incompatible value")
            }
        }
    }
}

impl std::error::Error for FfiError {}
