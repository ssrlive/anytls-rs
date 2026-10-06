use crate::{ClientArgs, TrafficStatus, client_app, traffic_status};
use clap::Parser;
use method_name::method_name_unstable;
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    io::{Error, ErrorKind},
    net::SocketAddr,
    ptr,
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;

static CLIENT_TOKEN: Mutex<Option<CancellationToken>> = Mutex::new(None);

/// Register a callback for cumulative client traffic totals.
///
/// The callback runs on a client runtime worker thread after traffic changes
/// and the configured interval has elapsed. Calls may be concurrent. Passing
/// a null callback disables reporting. A zero interval keeps the current
/// interval; the initial interval is one second.
/// The status pointer is valid only for the duration of the callback.
///
/// # Safety
///
/// The callback and context must remain valid while registered and until any
/// in-flight callback returns. The callback must be safe to invoke concurrently
/// from client runtime threads.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn anytls_set_traffic_status_callback(
    send_interval_secs: u32,
    callback: Option<unsafe extern "C" fn(*const TrafficStatus, *mut c_void)>,
    ctx: *mut c_void,
) {
    traffic_status::set_callback(send_interval_secs, callback, ctx);
}

fn client_args(command_line: &str) -> std::io::Result<ClientArgs> {
    let arguments = shlex::split(command_line).ok_or_else(|| Error::new(ErrorKind::InvalidInput, "invalid command-line quoting"))?;
    ClientArgs::try_parse_from(arguments)
        .map_err(|error| Error::new(ErrorKind::InvalidInput, error.to_string()))?
        .resolve()
}

fn generate_url(command_line: &str) -> std::io::Result<String> {
    client_args(command_line)?.format_url()
}

/// Run the client using a shell-style, complete client command line.
///
/// The command line must include the program name, for example
/// `anytls-client --url 'anytls://password@example.com' --listen mixed://127.0.0.1:0`.
/// The callback is invoked exactly once after the configured listener is bound.
/// This function blocks until `anytls_client_stop` is called or the client exits.
///
/// # Safety
///
/// `command_line` must point to a valid NUL-terminated UTF-8 string. If provided,
/// `callback` and `ctx` must remain valid for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn anytls_client_run(
    command_line: *const c_char,
    callback: Option<unsafe extern "C" fn(c_int, *mut c_void)>,
    ctx: *mut c_void,
) -> c_int {
    let result = (|| -> std::io::Result<()> {
        if command_line.is_null() {
            return Err(Error::new(ErrorKind::InvalidInput, "command_line is null"));
        }
        let command_line = unsafe { CStr::from_ptr(command_line) }
            .to_str()
            .map_err(|error| Error::new(ErrorKind::InvalidInput, error))?;
        let args = client_args(command_line)?;
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        let token = CancellationToken::new();
        {
            let mut running = CLIENT_TOKEN.lock().unwrap_or_else(|error| error.into_inner());
            if running.is_some() {
                return Err(Error::new(ErrorKind::AlreadyExists, "client is already running"));
            }
            *running = Some(token.clone());
        }

        let on_listening = callback.map(|callback| {
            Box::new(move |addr: SocketAddr| unsafe { callback(c_int::from(addr.port()), ctx) }) as Box<dyn FnOnce(SocketAddr)>
        });
        let result = runtime.block_on(client_app::run_client_with_args(token, args, on_listening));
        let mut running = CLIENT_TOKEN.lock().unwrap_or_else(|error| error.into_inner());
        *running = None;
        result
    })();

    match result {
        Ok(()) => 0,
        Err(error) => {
            log::error!("{} failed: {error}", method_name_unstable!());
            -1
        }
    }
}

/// Generate an AnyTLS URL from the complete client command line.
///
/// Returns the required buffer size in bytes, including the trailing NUL. If
/// `buf` is null or `size` is too small, the buffer is not modified. Returns 0
/// if the command line is invalid or does not describe a valid client.
///
/// # Safety
///
/// `command_line` must point to a valid NUL-terminated UTF-8 string. If `buf`
/// is non-null and `size` is large enough, it must point to writable memory of
/// at least `size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn anytls_generate_url(command_line: *const c_char, buf: *mut c_char, size: usize) -> usize {
    let url = match (|| -> std::io::Result<String> {
        if command_line.is_null() {
            return Err(Error::new(ErrorKind::InvalidInput, "command_line is null"));
        }
        let command_line = unsafe { CStr::from_ptr(command_line) }
            .to_str()
            .map_err(|error| Error::new(ErrorKind::InvalidInput, error))?;
        generate_url(command_line)
    })() {
        Ok(url) => url,
        Err(error) => {
            log::error!("{} failed: {error}", method_name_unstable!());
            return 0;
        }
    };

    let required = match url.len().checked_add(1) {
        Some(required) => required,
        None => return 0,
    };
    if buf.is_null() || size < required {
        return required;
    }

    unsafe {
        ptr::copy_nonoverlapping(url.as_ptr(), buf.cast::<u8>(), url.len());
        *buf.add(url.len()) = 0;
    }
    required
}

/// Request cancellation of a client started by `anytls_client_run`.
#[unsafe(no_mangle)]
pub extern "C" fn anytls_client_stop() -> c_int {
    let running = CLIENT_TOKEN.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(token) = running.as_ref() {
        token.cancel();
    }
    0
}

#[cfg(test)]
mod tests {
    use super::{client_args, generate_url};
    use std::ffi::{CStr, CString};

    #[test]
    fn ffi_client_parses_complete_cli_command_line() {
        let args = client_args("anytls-client --url 'anytls://secret@example.com' --listen mixed://127.0.0.1:0").unwrap();
        let listen_addr = args.listen.addr.unwrap();
        assert_eq!(listen_addr.to_string(), "127.0.0.1:0");
        assert_eq!(args.password.as_deref(), Some("secret"));
    }

    #[test]
    fn ffi_client_rejects_unclosed_cli_quotes() {
        assert!(client_args("anytls-client --url 'anytls://secret@example.com").is_err());
    }

    #[test]
    fn ffi_generate_url_uses_caller_buffer_and_reports_required_size() {
        let command_line = CString::new("anytls-client --url anytls://secret@example.com").unwrap();
        let expected = generate_url(command_line.to_str().unwrap()).unwrap();
        let required = unsafe { super::anytls_generate_url(command_line.as_ptr(), std::ptr::null_mut(), 0) };
        assert_eq!(required, expected.len() + 1);

        let mut small_buffer = vec![b'x' as std::ffi::c_char; required - 1];
        let reported = unsafe { super::anytls_generate_url(command_line.as_ptr(), small_buffer.as_mut_ptr(), small_buffer.len()) };
        assert_eq!(reported, required);
        assert!(small_buffer.iter().all(|byte| *byte == b'x' as std::ffi::c_char));

        let mut buffer = vec![0; required];
        let written = unsafe { super::anytls_generate_url(command_line.as_ptr(), buffer.as_mut_ptr(), buffer.len()) };
        assert_eq!(written, required);
        assert_eq!(unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_str().unwrap(), expected);
    }
}
