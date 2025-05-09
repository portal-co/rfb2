// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2022 Oxide Computer Company

use std::fmt::Debug;
use std::io;
use std::marker::{Send, Sync};
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::Shared;
use futures::FutureExt;
use log::{debug, error, info, trace};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::select;
use tokio::sync::{oneshot, Mutex};

use crate::rfb::{
    ClientInit, ClientMessage, FramebufferUpdate, KeyEvent, PixelFormat, ProtoVersion,
    ProtocolError, ReadMessage, SecurityResult, SecurityType, SecurityTypes, ServerInit,
    WriteMessage,
};

#[derive(Debug, Error)]
pub enum HandshakeError {
    #[error("incompatible protocol versions (client = {client:?}, server = {server:?})")]
    IncompatibleVersions {
        client: ProtoVersion,
        server: ProtoVersion,
    },

    #[error(
        "incompatible security types (client choice = {choice:?}, server offered = {offer:?})"
    )]
    IncompatibleSecurityTypes {
        choice: SecurityType,
        offer: SecurityTypes,
    },

    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

/// Immutable state
pub struct VncServerConfig {
    pub addr: SocketAddr,
    pub version: ProtoVersion,
    pub sec_types: SecurityTypes,
    pub name: String,
}

/// Mutable state
pub struct VncServerData {
    pub width: u16,
    pub height: u16,

    /// The pixel format of the framebuffer data passed in to the server via
    /// get_framebuffer_update.
    pub input_pixel_format: PixelFormat,
}

pub struct VncServer<S: Server<K>, K = SocketAddr> {
    /// VNC startup server configuration
    config: VncServerConfig,

    /// VNC runtime mutable state
    data: Mutex<VncServerData>,

    /// The underlying [`Server`] implementation
    pub server: Arc<dyn Fn(&K) -> Arc<S> + Send + Sync>,

    /// One-shot channel used to signal that the server should shut down.
    stop_ch: Mutex<Option<oneshot::Sender<()>>>,
}

#[async_trait]
pub trait Server<K = SocketAddr>: Sync + Send + 'static {
    async fn get_framebuffer_update(&self) -> FramebufferUpdate;
    async fn key_event(&self, _ke: KeyEvent) {}
    async fn stop(&self) {}
}

impl<S: Server<K>, K: Debug + Send + Sync + 'static> VncServer<S, K> {
    pub fn new(server: S, config: VncServerConfig, data: VncServerData) -> Arc<Self> {
        let server = Arc::new(server);
        Self::new_base(Arc::new(move |_| server.clone()), config, data)
    }
    pub fn new_base(
        server: Arc<dyn Fn(&K) -> Arc<S> + Send + Sync>,
        config: VncServerConfig,
        data: VncServerData,
    ) -> Arc<Self> {
        assert!(
            config.sec_types.0.len() > 0,
            "at least one security type must be defined"
        );
        Arc::new(Self {
            config: config,
            data: Mutex::new(data),
            server: server,
            stop_ch: Mutex::new(None),
        })
    }

    pub async fn set_pixel_format(&self, pixel_format: PixelFormat) {
        let mut locked = self.data.lock().await;
        locked.input_pixel_format = pixel_format;
    }

    pub async fn set_resolution(&self, width: u16, height: u16) {
        let mut locked = self.data.lock().await;
        locked.width = width;
        locked.height = height;
    }

    async fn rfb_handshake(
        &self,
        s: &mut (impl AsyncRead + AsyncWrite + Unpin + Send + Sync),
        addr: &K,
    ) -> Result<(), HandshakeError> {
        // ProtocolVersion handshake
        info!("Tx [{:?}]: ProtoVersion={:?}", addr, self.config.version);
        self.config.version.write_to(s).await?;
        let client_version = ProtoVersion::read_from(s).await?;
        info!("Rx [{:?}]: ClientVersion={:?}", addr, client_version);

        if client_version < self.config.version {
            let err_str = format!(
                "[{:?}] unsupported client version={:?} (server version: {:?})",
                addr, client_version, self.config.version
            );
            error!("{}", err_str);
            return Err(HandshakeError::IncompatibleVersions {
                client: client_version,
                server: self.config.version,
            });
        }

        // Security Handshake
        let supported_types = self.config.sec_types.clone();
        info!("Tx [{:?}]: SecurityTypes={:?}", addr, supported_types);
        supported_types.write_to(s).await?;
        let client_choice = SecurityType::read_from(s).await?;
        info!("Rx [{:?}]: SecurityType Choice={:?}", addr, client_choice);
        if !self.config.sec_types.0.contains(&client_choice) {
            info!("Tx [{:?}]: SecurityResult=Failure", addr);
            let failure = SecurityResult::Failure("unsupported security type".to_string());
            failure.write_to(s).await?;
            let err_str = format!("invalid security choice={:?}", client_choice);
            error!("{}", err_str);
            return Err(HandshakeError::IncompatibleSecurityTypes {
                choice: client_choice,
                offer: self.config.sec_types.clone(),
            });
        }

        let res = SecurityResult::Success;
        info!("Tx: SecurityResult=Success");
        res.write_to(s).await?;

        Ok(())
    }

    async fn rfb_initialization(
        &self,
        s: &mut (impl AsyncRead + AsyncWrite + Unpin + Send + Sync),
        addr: &K,
    ) -> Result<(), ProtocolError> {
        let client_init = ClientInit::read_from(s).await?;
        info!("Rx [{:?}]: ClientInit={:?}", addr, client_init);
        // TODO: decide what to do in exclusive case
        match client_init.shared {
            true => {}
            false => {}
        }

        let data = self.data.lock().await;
        let server_init = ServerInit::new(
            data.width,
            data.height,
            self.config.name.clone(),
            data.input_pixel_format.clone(),
        );
        info!("Tx [{:?}]: ServerInit={:#?}", addr, server_init);
        server_init.write_to(s).await?;

        Ok(())
    }

    pub async fn handle_conn(
        &self,
        s: &mut (impl AsyncRead + AsyncWrite + Unpin + Send + Sync),
        addr: K,
        mut close_ch: Shared<oneshot::Receiver<()>>,
    ) {
        info!("[{:?}] new connection", addr);

        if let Err(e) = self.rfb_handshake(s, &addr).await {
            error!("[{:?}] could not complete handshake: {:?}", addr, e);
            return;
        }

        if let Err(e) = self.rfb_initialization(s, &addr).await {
            error!("[{:?}] could not complete handshake: {:?}", addr, e);
            return;
        }

        let data = self.data.lock().await;
        let mut output_pixel_format = data.input_pixel_format.clone();
        drop(data);

        let server = (self.server)(&addr);

        loop {
            let req = select! {
                // Poll in the order written so we check for close first
                biased;

                _ = &mut close_ch => {
                    info!("[{:?}] server stopping, closing connection with peer", addr);
                    let _ = s.shutdown().await;
                    return;
                }

                req = ClientMessage::read_from(s) => req,
            };

            match req {
                Ok(client_msg) => match client_msg {
                    ClientMessage::SetPixelFormat(pf) => {
                        debug!("Rx [{:?}]: SetPixelFormat={:#?}", addr, pf);

                        // TODO: invalid pixel formats?
                        output_pixel_format = pf;
                    }
                    ClientMessage::SetEncodings(e) => {
                        debug!("Rx [{:?}]: SetEncodings={:?}", addr, e);
                    }
                    ClientMessage::FramebufferUpdateRequest(f) => {
                        debug!("Rx [{:?}]: FramebufferUpdateRequest={:?}", addr, f);

                        let mut fbu = server.get_framebuffer_update().await;

                        let data = self.data.lock().await;

                        // We only need to change pixel formats if the client requested a different
                        // one than what's specified in the input.
                        //
                        // For now, we only support transformations between 4-byte RGB formats, so
                        // if the requested format isn't one of those, we'll just leave the pixels
                        // as is.
                        if data.input_pixel_format != output_pixel_format
                            && data.input_pixel_format.is_rgb_888()
                            && output_pixel_format.is_rgb_888()
                        {
                            debug!(
                                "transforming: input={:#?}, output={:#?}",
                                data.input_pixel_format, output_pixel_format
                            );
                            fbu = fbu.transform(&data.input_pixel_format, &output_pixel_format);
                        } else if !(data.input_pixel_format.is_rgb_888()
                            && output_pixel_format.is_rgb_888())
                        {
                            debug!("cannot transform between pixel formats (not rgb888): input.is_rgb_888()={}, output.is_rgb_888()={}", data.input_pixel_format.is_rgb_888(), output_pixel_format.is_rgb_888());
                        } else {
                            debug!("no input transformation needed");
                        }

                        if let Err(e) = fbu.write_to(s).await {
                            error!(
                                "[{:?}] could not write FramebufferUpdateRequest: {:?}",
                                addr, e
                            );
                            return;
                        }
                        debug!("Tx [{:?}]: FramebufferUpdate", addr);
                    }
                    ClientMessage::KeyEvent(ke) => {
                        trace!("Rx [{:?}]: KeyEvent={:?}", addr, ke);
                        server.key_event(ke).await;
                    }
                    ClientMessage::PointerEvent(pe) => {
                        trace!("Rx [{:?}: PointerEvent={:?}", addr, pe);
                    }
                    ClientMessage::ClientCutText(t) => {
                        trace!("Rx [{:?}: ClientCutText={:?}", addr, t);
                    }
                },
                Err(e) => {
                    error!("[{:?}] error reading client message: {}", addr, e);
                    return;
                }
            }
        }
    }

    /// Start listening for incoming connections.
    pub async fn start(self: &Arc<Self>) -> io::Result<()>  where K: From<SocketAddr>{
        let listener = TcpListener::bind(self.config.addr).await?;

        // Create a channel to signal the server to stop.
        let (close_tx, close_rx) = oneshot::channel();
        assert!(
            self.stop_ch.lock().await.replace(close_tx).is_none(),
            "server already started"
        );
        let mut close_rx = close_rx.shared();

        loop {
            let (mut client_sock, client_addr) = select! {
                // Poll in the order written so we check for close first
                biased;

                _ = &mut close_rx => {
                    info!("server stopping");
                    // self.server.stop().await;
                    return Ok(());
                }

                conn = listener.accept() => conn?,
            };

            let close_rx = close_rx.clone();
            let server = self.clone();
            tokio::spawn(async move {
                server
                    .handle_conn(&mut client_sock, client_addr.into(), close_rx)
                    .await;
            });
        }
    }

    /// Stop the server (and disconnect any client) if it's running.
    pub async fn stop(self: &Arc<Self>) {
        if let Some(close_tx) = self.stop_ch.lock().await.take() {
            let _ = close_tx.send(());
        }
    }
}
