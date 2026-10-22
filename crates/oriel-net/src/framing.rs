use anyhow::{Context, Result, bail};
use oriel_proto::MAX_MESSAGE_LEN;
use serde::{Serialize, de::DeserializeOwned};

pub async fn write_msg<T: Serialize>(stream: &mut quinn::SendStream, msg: &T) -> Result<()> {
    let bytes = oriel_proto::encode_framed(msg)?;
    stream.write_all(&bytes).await.context("write message")?;
    Ok(())
}

/// Read one length-prefixed message; `None` on a clean end of stream.
pub async fn read_msg<T: DeserializeOwned>(stream: &mut quinn::RecvStream) -> Result<Option<T>> {
    let mut len = [0u8; 4];
    match stream.read_exact(&mut len).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e).context("read message length"),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MESSAGE_LEN {
        bail!("message too large ({len} bytes)");
    }
    let mut buf = vec![0u8; len];
    stream
        .read_exact(&mut buf)
        .await
        .context("read message body")?;
    Ok(Some(oriel_proto::decode(&buf)?))
}

/// The bidirectional control stream.
pub struct ControlStream {
    pub send: quinn::SendStream,
    pub recv: quinn::RecvStream,
}

impl ControlStream {
    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<()> {
        write_msg(&mut self.send, msg).await
    }

    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<T> {
        read_msg(&mut self.recv)
            .await?
            .context("control stream closed by peer")
    }
}
