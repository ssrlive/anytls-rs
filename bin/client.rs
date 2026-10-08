use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let cancel_token = CancellationToken::new();
    let cancel_token_on_ctrlc = cancel_token.clone();
    let ctrlc = ctrlc2::AsyncCtrlC::new(move || {
        cancel_token_on_ctrlc.cancel();
        true
    })?;

    let args = anytls::ClientArgs::from_cli()?;
    let client = anytls::run_client(cancel_token, args, None);
    tokio::pin!(client);
    let res = tokio::select! {
        result = &mut client => result,
        result = ctrlc => {
            result?;
            client.await
        }
    };
    log::info!("{} -- Client exited with result: {:?}", method_name::method_name_unstable!(), res);
    res
}
