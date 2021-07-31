use std::io;
use std::sync::Arc;
use futures::try_join;

use tokio::io::{AsyncRead, AsyncWrite};
use crate::transport::{AsyncConnect, AsyncAccept};

mod copy;
pub use copy::copy;

pub async fn bidi_copy<L, C>(
    base: L::Base,
    lis: Arc<L>,
    conn: Arc<C>,
) -> io::Result<()>
where
    L: AsyncAccept,
    C: AsyncConnect,
{
    let (sin, sout) = try_join!(lis.accept(base), conn.connect())?;
    let (ri, wi) = tokio::io::split(sin);
    let (ro, wo) = tokio::io::split(sout);
    let _ = try_join!(copy(ri, wo), copy(ro, wi));
    Ok(())
}

// this is only used by protocols that impl multiplex
pub async fn bidi_copy_with_stream<C, S>(cc: Arc<C>, sin: S) -> io::Result<()>
where
    C: AsyncConnect + 'static,
    S: AsyncRead + AsyncWrite,
{
    let sout = cc.connect().await?;
    let (ri, wi) = tokio::io::split(sin);
    let (ro, wo) = tokio::io::split(sout);
    let _ = try_join!(copy(ri, wo), copy(ro, wi));
    Ok(())
}

pub async fn proxy<L, C>(lis: Arc<L>, conn: Arc<C>) -> io::Result<()>
where
    L: AsyncAccept + 'static,
    C: AsyncConnect + 'static,
{
    loop {
        if let Ok((base, _)) = lis.accept_base().await {
            tokio::spawn(bidi_copy(base, lis.clone(), conn.clone()));
        }
    }
}

// zero copy
#[cfg(target_os = "linux")]
mod zero_copy;

#[cfg(target_os = "linux")]
pub mod linux_ext {
    use super::*;
    use zero_copy::zero_copy;
    use crate::transport::plain;
    pub async fn bidi_zero_copy(
        mut sin: plain::PlainStream,
        conn: plain::Connector,
    ) -> io::Result<()> {
        let mut sout = conn.connect().await?;
        let (ri, wi) = plain::linux_ext::split(&mut sin);
        let (ro, wo) = plain::linux_ext::split(&mut sout);

        let _ = try_join!(zero_copy(ri, wo), zero_copy(ro, wi));

        Ok(())
    }

    pub async fn splice(
        lis: plain::Acceptor,
        conn: plain::Connector,
    ) -> io::Result<()> {
        let plain_lis = lis.inner();
        loop {
            if let Ok((sin, _)) = plain_lis.accept_plain().await {
                tokio::spawn(bidi_zero_copy(sin, conn.clone()));
            }
        }
    }
}
