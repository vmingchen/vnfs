//! Owned standard-error causes. Payloads carry facts, never logical input indices.
use super::{RpcError, StatusCode, TransportKind, errno_for, status_kind};
use std::{
    borrow::Cow,
    error::Error,
    fmt, io,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug)]
struct NativeStatus(StatusCode);
impl fmt::Display for NativeStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)?;
        match self.0 {
            StatusCode::Nfs(10019) => write!(f, "; {}", super::NFS_REFERRAL_UNSUPPORTED),
            StatusCode::Smb(0xc000_0257) => f.write_str("; SMB DFS referrals are unsupported; connect directly to the destination server/share"),
            _ => Ok(()),
        }
    }
}
impl Error for NativeStatus {}

#[derive(Debug)]
struct Transport {
    kind: TransportKind,
    message: Cow<'static, str>,
}
impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} transport failure: {}", self.kind, self.message)
    }
}
impl Error for Transport {}

#[derive(Debug, Default)]
struct Context {
    native: Option<StatusCode>,
    transport: Option<TransportKind>,
    operation: Option<&'static str>,
    path: Option<PathBuf>,
    message: Option<Cow<'static, str>>,
    source: Option<io::Error>,
}
impl fmt::Display for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(operation) = self.operation {
            write!(f, "{operation}: ")?;
        }
        if let Some(path) = &self.path {
            write!(f, "{}: ", path.display())?;
        }
        if let Some(message) = &self.message {
            write!(f, "{message}: ")?;
        }
        if let Some(source) = &self.source {
            return source.fmt(f);
        }
        if let Some(status) = self.native {
            return NativeStatus(status).fmt(f);
        }
        if let Some(kind) = self.transport {
            return write!(f, "{kind:?} transport failure");
        }
        f.write_str("I/O failure")
    }
}
impl Error for Context {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_ref().map(|e| e as _)
    }
}

#[derive(Debug)]
struct SharedCause {
    source: Arc<io::Error>,
    native: Option<StatusCode>,
    transport: Option<TransportKind>,
}
impl fmt::Display for SharedCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(f)
    }
}
impl Error for SharedCause {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Inspect actual OS errno or a recognized protocol status without parsing text.
/// Context and group sharing preserve these facts. Foreign payloads are opaque.
pub fn native_status(error: &io::Error) -> Option<StatusCode> {
    if let Some(errno) = error.raw_os_error() {
        return Some(StatusCode::Errno(errno as u32));
    }
    let payload = error.get_ref()?;
    if let Some(native) = payload.downcast_ref::<NativeStatus>() {
        return Some(native.0);
    }
    if let Some(context) = payload.downcast_ref::<Context>() {
        return context.native;
    }
    payload
        .downcast_ref::<SharedCause>()
        .and_then(|shared| shared.native)
}

/// Transport provenance, independent of category and never replay permission.
pub fn transport_kind(error: &io::Error) -> Option<TransportKind> {
    let payload = error.get_ref()?;
    if let Some(transport) = payload.downcast_ref::<Transport>() {
        return Some(transport.kind);
    }
    if let Some(context) = payload.downcast_ref::<Context>() {
        return context.transport;
    }
    payload
        .downcast_ref::<SharedCause>()
        .and_then(|shared| shared.transport)
}

/// Construct a native failure without relabeling a protocol status as OS errno.
pub fn from_status(status: StatusCode) -> io::Error {
    match status {
        StatusCode::Errno(code) => io::Error::from_raw_os_error(code as i32),
        status => io::Error::new(status_kind(status), NativeStatus(status)),
    }
}

pub fn transport(kind: TransportKind, message: impl Into<Cow<'static, str>>) -> io::Error {
    let category = match kind {
        TransportKind::Timeout => io::ErrorKind::TimedOut,
        TransportKind::Auth => io::ErrorKind::PermissionDenied,
        TransportKind::InvalidReply => io::ErrorKind::InvalidData,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(
        category,
        Transport {
            kind,
            message: message.into(),
        },
    )
}

pub fn invalid_reply(message: impl Into<Cow<'static, str>>) -> io::Error {
    transport(TransportKind::InvalidReply, message)
}

fn update_context(mut error: io::Error, update: impl FnOnce(&mut Context)) -> io::Error {
    if let Some(context) = error.get_mut().and_then(|e| e.downcast_mut::<Context>()) {
        update(context);
        return error;
    }
    let kind = error.kind();
    let mut context = Context {
        native: native_status(&error),
        transport: transport_kind(&error),
        ..Context::default()
    };
    // Known small payloads become inline facts in the contextual allocation;
    // retain an opaque original intact instead of formatting/reconstructing it.
    let error = match error.downcast::<NativeStatus>() {
        Ok(_) => None,
        Err(error) => match error.downcast::<Transport>() {
            Ok(transport) => {
                context.message = Some(transport.message);
                None
            }
            Err(error) => Some(error),
        },
    };
    context.source = error;
    update(&mut context);
    io::Error::new(kind, context)
}

pub fn with_context(
    error: io::Error,
    operation: &'static str,
    path: impl Into<PathBuf>,
) -> io::Error {
    update_context(error, |context| {
        context.operation = Some(operation);
        context.path = Some(path.into());
    })
}

pub fn with_message(error: io::Error, message: impl Into<Cow<'static, str>>) -> io::Error {
    update_context(error, |context| context.message = Some(message.into()))
}

// Follow only our own wrappers, not arbitrary foreign Error::source chains.
// The bound also limits inspection of user-created chains assembled by repeatedly
// sharing and adding context. Native/provenance facts above are cached directly.
fn context_value<'a, T: ?Sized>(
    error: &'a io::Error,
    value: impl Fn(&'a Context) -> Option<&'a T>,
) -> Option<&'a T> {
    let mut current = error;
    for _ in 0..64 {
        let payload = current.get_ref()?;
        if let Some(context) = payload.downcast_ref::<Context>() {
            if let Some(value) = value(context) {
                return Some(value);
            }
            current = context.source.as_ref()?;
        } else {
            current = &payload.downcast_ref::<SharedCause>()?.source;
        }
    }
    None
}
pub fn operation(error: &io::Error) -> Option<&'static str> {
    context_value(error, |c| c.operation.as_ref()).copied()
}
pub fn path(error: &io::Error) -> Option<&Path> {
    context_value(error, |c| c.path.as_deref())
}
pub fn message(error: &io::Error) -> Option<&str> {
    let mut current = error;
    for _ in 0..64 {
        let payload = current.get_ref()?;
        if let Some(transport) = payload.downcast_ref::<Transport>() {
            return Some(&transport.message);
        }
        if let Some(context) = payload.downcast_ref::<Context>() {
            if let Some(message) = context.message.as_deref() {
                return Some(message);
            }
            current = context.source.as_ref()?;
        } else {
            current = &payload.downcast_ref::<SharedCause>()?.source;
        }
    }
    None
}

/// Lossy errno translation for existing adapters, not a Rust cause contract.
pub fn to_errno(error: &io::Error) -> i32 {
    errno_for(error.kind(), native_status(error))
}

pub fn from_rpc(error: RpcError) -> io::Error {
    if error.is_transport() {
        transport(
            error.transport_kind.unwrap_or(TransportKind::Other),
            error.message,
        )
    } else {
        let cause = from_status(StatusCode::Nfs(error.status));
        if error.message.is_empty() {
            cause
        } else {
            with_message(cause, error.message)
        }
    }
}

/// Give each affected slot an owned cause. Singleton errors move unchanged;
/// known native/category recipes need no opaque sharing. Other groups retain
/// their original source, with per-slot standard-error boxing.
pub fn fanout(error: io::Error, count: usize) -> impl ExactSizeIterator<Item = io::Error> {
    enum Recipe {
        Single(Option<io::Error>),
        Native(StatusCode),
        Kind(io::ErrorKind),
        Shared {
            source: Arc<io::Error>,
            native: Option<StatusCode>,
            transport: Option<TransportKind>,
        },
    }
    let mut recipe = if count <= 1 {
        Recipe::Single(Some(error))
    } else if let Some(native) = error
        .raw_os_error()
        .map(|code| StatusCode::Errno(code as u32))
        .or_else(|| {
            error
                .get_ref()
                .and_then(|e| e.downcast_ref::<NativeStatus>())
                .map(|n| n.0)
        })
    {
        Recipe::Native(native)
    } else if error.get_ref().is_none() {
        Recipe::Kind(error.kind())
    } else {
        let native = native_status(&error);
        let transport = transport_kind(&error);
        let source = match error
            .get_ref()
            .and_then(|e| e.downcast_ref::<SharedCause>())
        {
            Some(shared) => shared.source.clone(),
            None => Arc::new(error),
        };
        Recipe::Shared {
            source,
            native,
            transport,
        }
    };
    (0..count).map(move |_| match &mut recipe {
        Recipe::Single(error) => error.take().expect("one owned cause"),
        Recipe::Native(status) => from_status(*status),
        Recipe::Kind(kind) => (*kind).into(),
        Recipe::Shared {
            source,
            native,
            transport,
        } => io::Error::new(
            source.kind(),
            SharedCause {
                source: source.clone(),
                native: *native,
                transport: *transport,
            },
        ),
    })
}

/// Standard-I/O collectors retry Interrupted. Once progress is delivered or
/// completion is uncertain, retain the original cause without permitting replay.
pub fn suppress_interrupted(error: io::Error) -> io::Error {
    if error.kind() != io::ErrorKind::Interrupted {
        return error;
    }
    io::Error::other(Context {
        native: native_status(&error),
        transport: transport_kind(&error),
        source: Some(error),
        ..Context::default()
    })
}

/// Retain one original for an explicitly shared failure group.
pub fn shared_source(error: io::Error) -> Arc<io::Error> {
    match error
        .get_ref()
        .and_then(|e| e.downcast_ref::<SharedCause>())
    {
        Some(shared) => shared.source.clone(),
        None => Arc::new(error),
    }
}

/// One owned slot cause referring to an explicitly retained original.
pub fn from_shared(source: &Arc<io::Error>) -> io::Error {
    io::Error::new(
        source.kind(),
        SharedCause {
            native: native_status(source),
            transport: transport_kind(source),
            source: source.clone(),
        },
    )
}

/// Transfer an owned cause into a failure group while retaining this owner's
/// diagnostic. This is explicit source sharing, not lossy cloning of a borrow.
pub fn share_in_place(error: &mut io::Error) -> Arc<io::Error> {
    if let Some(shared) = error
        .get_ref()
        .and_then(|e| e.downcast_ref::<SharedCause>())
    {
        return shared.source.clone();
    }
    let original = std::mem::replace(error, io::ErrorKind::Other.into());
    let shared = shared_source(original);
    *error = from_shared(&shared);
    shared
}
