use bytes::BytesMut;
use futures_util::stream::SplitStream;
use futures_util::StreamExt;
use gloo_net::websocket::futures::WebSocket;
use gloo_net::websocket::Message;
use ironrdp::pdu::Action;

/// Async framed reader for WASM WebSocket transport.
///
/// Reads raw bytes from the WebSocket, buffers them, and extracts
/// complete RDP PDUs by examining the first byte for the Action type
/// and reading the appropriate length.
pub(crate) struct WasmFramed {
    ws_read: SplitStream<WebSocket>,
    buf: BytesMut,
}

impl WasmFramed {
    pub fn new(ws_read: SplitStream<WebSocket>) -> Self {
        Self {
            ws_read,
            buf: BytesMut::with_capacity(16384),
        }
    }

    /// Read raw bytes from the WebSocket, appending to internal buffer.
    async fn fill_buf(&mut self) -> anyhow::Result<()> {
        match self.ws_read.next().await {
            Some(Ok(Message::Bytes(data))) => {
                self.buf.extend_from_slice(&data);
                Ok(())
            }
            Some(Ok(Message::Text(_))) => {
                // Ignore text messages, try again
                Box::pin(self.fill_buf()).await
            }
            Some(Err(e)) => {
                anyhow::bail!("WebSocket read error: {e}");
            }
            None => {
                anyhow::bail!("WebSocket closed");
            }
        }
    }

    /// Read the next complete RDP PDU from the WebSocket stream.
    ///
    /// RDP frames start with a header byte that indicates whether it's
    /// FastPath (0x00/0x04/0x08/0x0C) or X224 (0x03).
    /// For FastPath: byte 1 is the length (1 or 2 bytes).
    /// For X224: bytes 2-3 are the 16-bit big-endian length.
    pub async fn read_pdu(&mut self) -> anyhow::Result<(Action, Vec<u8>)> {
        // Ensure we have at least 2 bytes for the header
        while self.buf.len() < 2 {
            self.fill_buf().await?;
        }

        // Determine action and PDU length
        let first_byte = self.buf[0];
        let action = Action::from_fp_output_header(first_byte)
            .map_err(|b| anyhow::anyhow!("Unknown action byte: 0x{b:02x}"))?;

        let pdu_length = match action {
            Action::X224 => {
                // TPKT header: [version(1), reserved(1), length(2 big-endian)]
                while self.buf.len() < 4 {
                    self.fill_buf().await?;
                }
                let len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
                len
            }
            Action::FastPath => {
                // FastPath: byte 0 = header, byte 1 = length
                // If bit 7 of byte 1 is set, length is 2 bytes (byte1[6:0] << 8 | byte2)
                let len_byte = self.buf[1];
                if len_byte & 0x80 != 0 {
                    while self.buf.len() < 3 {
                        self.fill_buf().await?;
                    }
                    let len = (((len_byte & 0x7F) as usize) << 8) | (self.buf[2] as usize);
                    len
                } else {
                    len_byte as usize
                }
            }
        };

        if pdu_length == 0 {
            anyhow::bail!("Invalid PDU length: 0");
        }

        // Read until we have the full PDU
        while self.buf.len() < pdu_length {
            self.fill_buf().await?;
        }

        // Extract the complete PDU
        let payload = self.buf.split_to(pdu_length).to_vec();
        Ok((action, payload))
    }

    /// Read raw bytes from the WebSocket (for connector phase).
    /// Returns whatever the WebSocket gives us in one message.
    pub async fn read_bytes(&mut self) -> anyhow::Result<Vec<u8>> {
        if !self.buf.is_empty() {
            return Ok(self.buf.split().to_vec());
        }

        match self.ws_read.next().await {
            Some(Ok(Message::Bytes(data))) => Ok(data),
            Some(Ok(Message::Text(_))) => Box::pin(self.read_bytes()).await,
            Some(Err(e)) => anyhow::bail!("WebSocket read error: {e}"),
            None => anyhow::bail!("WebSocket closed"),
        }
    }

    /// Push data back into the buffer.
    pub fn push_back(&mut self, data: &[u8]) {
        let mut new_buf = BytesMut::with_capacity(data.len() + self.buf.len());
        new_buf.extend_from_slice(data);
        new_buf.extend_from_slice(&self.buf);
        self.buf = new_buf;
    }
}
