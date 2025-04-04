mod server;
mod setup;
mod watch;

use setup::{upload, Session};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use watch::watch_for_changes;

use crate::commands::dev::{socket, Protocol, ServerConfig};
use crate::deploy::DeployTarget;
use crate::settings::global_user::GlobalUser;
use crate::settings::toml::Target;
use crate::terminal::message::{Message, StdOut};
use anyhow::Result;

use tokio::runtime::Runtime as TokioRuntime;

use std::sync::{
    mpsc::{self, Sender},
    Arc, Mutex,
};
use std::thread;

pub fn dev(
    target: Target,
    user: GlobalUser,
    server_config: ServerConfig,
    deploy_target: DeployTarget,
    local_protocol: Protocol,
    upstream_protocol: Protocol,
    verbose: bool,
) -> Result<()> {
    let runtime = TokioRuntime::new()?;
    loop {
        let target = target.clone();
        let user = user.clone();
        let server_config = server_config.clone();
        let deploy_target = deploy_target.clone();
        let (sender, receiver) = mpsc::channel();
        let (tx_init_shutdown, rx_init_shutdown) = oneshot::channel();
        let (tx_ack_shutdown, rx_ack_shutdown) = oneshot::channel();

        let tasks = dev_once(
            target,
            user,
            server_config,
            deploy_target,
            local_protocol,
            upstream_protocol,
            verbose,
            &runtime,
            sender,
            (rx_init_shutdown, tx_ack_shutdown),
        )?;

        while receiver.recv()?.is_none() {}
        tx_init_shutdown
            .send(())
            .expect("Could not initiate listener task shutdown");
        runtime.block_on(async {
            rx_ack_shutdown
                .await
                .expect("Could not receive shutdown acknowledgement");
        });
        for task in tasks {
            task.abort();
        }
        StdOut::info("Starting a new session because the existing token has expired");
    }
}

#[allow(clippy::too_many_arguments)]
fn dev_once(
    mut target: Target,
    user: GlobalUser,
    server_config: ServerConfig,
    deploy_target: DeployTarget,
    local_protocol: Protocol,
    upstream_protocol: Protocol,
    verbose: bool,
    runtime: &TokioRuntime,
    refresh_session_sender: Sender<Option<()>>,
    shutdown_channel: (oneshot::Receiver<()>, oneshot::Sender<()>),
) -> Result<Vec<JoinHandle<Result<()>>>> {
    let session = Session::new(&target, &user, &deploy_target)?;

    let preview_token = upload(
        &mut target,
        &deploy_target,
        &user,
        session.preview_token.clone(),
        verbose,
    )?;

    let preview_token = Arc::new(Mutex::new(preview_token));

    {
        let preview_token = preview_token.clone();
        let session_token = session.preview_token.clone();
        let refresh_session_sender = refresh_session_sender.clone();

        thread::spawn(move || {
            watch_for_changes(
                &target,
                &deploy_target,
                &user,
                Arc::clone(&preview_token),
                session_token,
                verbose,
                refresh_session_sender,
            )
        });
    }

    let devtools_listener = runtime.spawn(socket::listen(
        session.websocket_url,
        Some(refresh_session_sender),
    ));
    let server = match local_protocol {
        Protocol::Https => runtime.spawn(server::https(
            server_config,
            Arc::clone(&preview_token),
            session.host,
            shutdown_channel,
        )),
        Protocol::Http => runtime.spawn(server::http(
            server_config,
            Arc::clone(&preview_token),
            session.host,
            upstream_protocol,
            shutdown_channel,
        )),
    };
    Ok(vec![devtools_listener, server])
}
