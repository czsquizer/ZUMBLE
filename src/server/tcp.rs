use crate::client::Client;
use crate::handler::MessageHandler;
use crate::message::ClientMessage;
use crate::proto::mumble::Version;
use crate::proto::MessageKind;
use crate::sync::RwLock;
use crate::ServerState;
use actix_server::Server;
use actix_service::fn_service;
use anyhow::Context;
use std::sync::Arc;
use tokio::io;
use tokio::io::ReadHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::mpsc::Receiver;
use tokio_rustls::{server::TlsStream, TlsAcceptor};

pub fn create_tcp_server(
    tcp_listener: TcpListener,
    acceptor: TlsAcceptor,
    server_version: Version,
    state: Arc<RwLock<ServerState>>,
) -> Server {
    Server::build()
        .listen(
            "mumble-tcp",
            tcp_listener.into_std().expect("cannot create tcp listener"),
            move || {
                let acceptor = acceptor.clone();
                let server_version = server_version.clone();
                let state = state.clone();

                fn_service(move |stream: TcpStream| {
                    let acceptor = acceptor.clone();
                    let server_version = server_version.clone();
                    let state = state.clone();

                    async move {
                        match handle_new_client(acceptor, server_version, state, stream).await {
                            Ok(_) => (),
                            Err(e) => tracing::error!("handle client error: {:?}", e),
                        }

                        Ok::<(), anyhow::Error>(())
                    }
                })
            },
        )
        .expect("cannot create tcp server")
        .run()
}

async fn handle_new_client(acceptor: TlsAcceptor,
                     server_version: Version,
                     state: Arc<RwLock<ServerState>>, stream: TcpStream) -> Result<(), anyhow::Error> {
    stream.set_nodelay(true).context("set stream no delay")?;

    let mut stream = acceptor.accept(stream).await.context("accept tls")?;
    let (version, authenticate, crypt_state) = Client::init(&mut stream, server_version).await.context("init client")?;

    let (read, write) = io::split(stream);
    let (tx, rx) = mpsc::channel(1024 * 4);

    let username = authenticate.get_username().to_string();
    let client = {
        state.write_err().await.context("add client to server")?.add_client(
            version,
            authenticate,
            crypt_state,
            write,
            tx,
        )
    };

    crate::metrics::CLIENTS_TOTAL.inc();

    tracing::info!("new client {} connected", username);

    match client_run(read, rx, state.clone(), client.clone()).await {
        Ok(_) => (),
        Err(e) => tracing::error!("client {} error: {:?}", username, e),
    }

    tracing::info!("client {} disconnected", username);

    let (client_id, channel_id) = {
        state.write_err().await.context("wait state for disconnect user")?.disconnect(client).await.context("disconnect user")?
    };

    crate::metrics::CLIENTS_TOTAL.dec();

    {
        state
            .read_err()
            .await
            .context("wait state for remove client")?
            .remove_client(client_id, channel_id)
            .await.context("remove client")?;
    }

    Ok(())
}

pub async fn client_run(
    mut read: ReadHalf<TlsStream<TcpStream>>,
    mut receiver: Receiver<ClientMessage>,
    state: Arc<RwLock<ServerState>>,
    client: Arc<RwLock<Client>>,
) -> Result<(), anyhow::Error> {
    let codec_version = { state.read_err().await?.check_codec().await? };

    if let Some(codec_version) = codec_version {
        {
            client
                .read_err()
                .await?
                .send_message(MessageKind::CodecVersion, &codec_version)
                .await?;
        }
    }

    {
        let client_sync = client.read_err().await?;

        client_sync.sync_client_and_channels(&state).await.map_err(|e| {
            tracing::error!("init client error during channel sync: {:?}", e);

            e
        })?;
        client_sync.send_my_user_state().await?;
        client_sync.send_server_sync().await?;
        client_sync.send_server_config().await?;
    }

    let user_state = { client.read_err().await?.get_user_state() };

    {
        match state.read_err().await?.broadcast_message(MessageKind::UserState, &user_state).await {
            Ok(_) => (),
            Err(e) => tracing::error!("failed to send user state: {:?}", e),
        }
    }

    loop {
        match MessageHandler::handle(&mut read, &mut receiver, state.clone(), client.clone()).await {
            Ok(_) => (),
            Err(e) => {
                if e.is::<io::Error>() {
                    let ioerr = e.downcast::<io::Error>().unwrap();

                    // avoid error for client disconnect
                    if ioerr.kind() == io::ErrorKind::UnexpectedEof {
                        return Ok(());
                    }

                    return Err(ioerr.into());
                }

                return Err(e);
            }
        }
    }
}
