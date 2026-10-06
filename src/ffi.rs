use crate::{ClientArgs, client_app};
use clap::Parser;
use method_name::method_name_unstable;
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    io::{Error, ErrorKind},
    net::SocketAddr,
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;

static CLIENT_TOKEN: Mutex<Option<CancellationToken>> = Mutex::new(None);

fn client_args(command_line: &str) -> std::io::Result<ClientArgs> {
    let arguments = shlex::split(command_line).ok_or_else(|| Error::new(ErrorKind::InvalidInput, "invalid command-line quoting"))?;
    ClientArgs::try_parse_from(arguments)
        .map_err(|error| Error::new(ErrorKind::InvalidInput, error.to_string()))?
        .resolve()
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
    use super::client_args;

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
}
