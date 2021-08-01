use std::io;

use std::pin::Pin;
use std::task::{Poll, Context};
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicUsize, Ordering};
use futures::StreamExt;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use quinn::crypto::rustls::TlsSession;
use quinn::generic::{SendStream, RecvStream, Connection};
use quinn::{Endpoint, NewConnection, Incoming, IncomingBiStreams};

use super::{AsyncConnect, AsyncAccept, IOStream, Transport};
use crate::dns;
use crate::utils::{self, CommonAddr};

pub struct QuicStream {
    send: SendStream<TlsSession>,
    recv: RecvStream<TlsSession>,
}

impl QuicStream {
    #[inline]
    pub fn new(
        send: SendStream<TlsSession>,
        recv: RecvStream<TlsSession>,
    ) -> Self {
        QuicStream { send, recv }
    }
}

impl IOStream for QuicStream {}

impl AsyncRead for QuicStream {
    #[inline]
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}

impl AsyncWrite for QuicStream {
    #[inline]
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.send).poll_write(cx, buf)
    }

    #[inline]
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.send).poll_flush(cx)
    }

    #[inline]
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.send).poll_shutdown(cx)
    }
}

// Connector
pub struct Connector {
    cc: Endpoint,
    addr: CommonAddr,
    sni: String,
    max_concurrent: usize,
    count: AtomicUsize,
    channel: RwLock<Option<Connection<TlsSession>>>,
}

impl Connector {
    pub fn new(
        cc: Endpoint,
        addr: CommonAddr,
        sni: String,
        max_concurrent: usize,
    ) -> Self {
        let max_concurrent = if max_concurrent == 0 {
            1000
        } else {
            max_concurrent
        };
        Connector {
            cc,
            addr,
            sni,
            max_concurrent,
            count: AtomicUsize::new(1),
            channel: RwLock::new(None),
        }
    }
}

#[async_trait]
impl AsyncConnect for Connector {
    const TRANS: Transport = Transport::QUIC;

    const SCHEME: &'static str = "quic";

    type IO = QuicStream;

    #[inline]
    fn addr(&self) -> &CommonAddr { &self.addr }

    async fn connect(&self) -> io::Result<Self::IO> {
        let client = new_client(self).await?;
        let (send, recv) = client.open_bi().await?;
        Ok(QuicStream::new(send, recv))
    }
}

async fn new_client(cc: &Connector) -> io::Result<Connection<TlsSession>> {
    // reuse existed connection
    let channel = (*cc.channel.read().unwrap()).clone();
    if let Some(client) = channel {
        if cc.count.load(Ordering::Relaxed) < cc.max_concurrent {
            return Ok(client);
        };
    };

    // establish a new connection
    let connect_addr = match &cc.addr {
        CommonAddr::SocketAddr(sockaddr) => *sockaddr,
        CommonAddr::DomainName(addr, port) => {
            let ip = dns::resolve_async(addr).await?;
            SocketAddr::new(ip, *port)
        }
        #[cfg(unix)]
        CommonAddr::UnixSocketPath(_) => unreachable!(),
    };

    let connecting = cc
        .cc
        .connect(&connect_addr, &cc.sni)
        .map_err(|e| utils::new_io_err(&e.to_string()))?;

    // early data
    let new_conn = match connecting.into_0rtt() {
        Ok((new_conn, zero_rtt)) => {
            zero_rtt.await;
            new_conn
        }
        Err(connecting) => connecting.await?,
    };

    let NewConnection {
        connection: client, ..
    } = new_conn;

    // store connection
    // may have conflicts
    cc.count.store(1, Ordering::Relaxed);
    *cc.channel.write().unwrap() = Some(client.clone());
    Ok(client)
}

// Acceptor
pub struct Acceptor<C: AsyncConnect> {
    cc: Arc<C>,
    lis: Incoming,
    addr: CommonAddr,
}

impl<C: AsyncConnect> Acceptor<C> {
    pub fn new(cc: Arc<C>, lis: Incoming, addr: CommonAddr) -> Self {
        Acceptor { cc, lis, addr }
    }
}

#[async_trait]
impl<C> AsyncAccept for Acceptor<C>
where
    C: AsyncConnect + 'static,
{
    const TRANS: Transport = Transport::QUIC;

    const SCHEME: &'static str = "quic";

    type IO = QuicStream;

    type Base = QuicStream;

    fn addr(&self) -> &CommonAddr { &self.addr }

    async fn accept_base(&self) -> io::Result<(Self::Base, SocketAddr)> {
        // new connection
        let lis = unsafe { utils::const_cast(&self.lis) };
        let connecting = lis
            .next()
            .await
            .ok_or(utils::new_io_err("no more connections"))?;

        // early data
        let new_conn = match connecting.into_0rtt() {
            Ok((new_conn, _)) => new_conn,
            Err(connecting) => connecting.await?,
        };

        let NewConnection {
            connection: x,
            mut bi_streams,
            ..
        } = new_conn;

        let (send, recv) = bi_streams
            .next()
            .await
            .ok_or(utils::new_io_err("connection closed"))??;

        tokio::spawn(handle_mux_conn(self.cc.clone(), bi_streams));
        Ok((QuicStream::new(send, recv), x.remote_address()))
    }

    async fn accept(&self, base: Self::Base) -> io::Result<Self::IO> {
        Ok(base)
    }
}

async fn handle_mux_conn<C>(cc: Arc<C>, mut bi_streams: IncomingBiStreams)
where
    C: AsyncConnect + 'static,
{
    use crate::io::bidi_copy_with_stream;
    while let Some(Ok((send, recv))) = bi_streams.next().await {
        tokio::spawn(bidi_copy_with_stream(
            cc.clone(),
            QuicStream::new(send, recv),
        ));
    }
}