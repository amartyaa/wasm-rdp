use bytes::BytesMut;
use futures_util::stream::SplitStream;
use futures_util::StreamExt;
use gloo_net::websocket::futures::WebSocket;
use gloo_net::websocket::Message;
use ironrdp::pdu::Action;

/// Async framed reader for WASM WebSocket transport.
///
/// Buffers raw bytes from the WebSocket and extracts complete RDP PDUs.
/// TCP doesn't guarantee message boundaries, and the proxy relays raw TCP
/// reads as individual WebSocket messages. A single PDU may span multiple
/// WS messages, or a single WS message may contain multiple PDUs.
/// This struct handles both cases via internal buffering.
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

    /// Read more bytes from the WebSocket into the internal buffer.
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

    /// Read exactly `n` bytes from the WebSocket, buffering as needed.
    /// Returns the bytes and leaves any excess in the internal buffer.
    pub async fn read_exact(&mut self, n: usize) -> anyhow::Result<Vec<u8>> {
        while self.buf.len() < n {
            self.fill_buf().await?;
        }
        Ok(self.buf.split_to(n).to_vec())
    }

    /// Read exactly one complete PDU using the connector's PduHint.
    ///
    /// The PduHint knows how to determine PDU size from the first few header
    /// bytes. We keep buffering WebSocket data until the hint can tell us
    /// the full size, then we extract exactly that many bytes.
    pub async fn read_by_hint(
        &mut self,
        hint: &dyn ironrdp::pdu::PduHint,
    ) -> anyhow::Result<Vec<u8>> {
        loop {
            // Try to determine the PDU size from what we have buffered
            match hint.find_size(&self.buf) {
                Ok(Some((_hint_matched, pdu_length))) => {
                    if pdu_length == 0 {
                        let hex_dump = self.buf.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" ");
                        anyhow::bail!("PduHint returned zero-length PDU. Buffer: {}", hex_dump);
                    }
                    // We know the size — ensure we have all the bytes
                    while self.buf.len() < pdu_length {
                        self.fill_buf().await?;
                    }
                    return Ok(self.buf.split_to(pdu_length).to_vec());
                }
                Ok(None) => {
                    // Not enough bytes to determine size yet — read more
                    self.fill_buf().await?;
                }
                Err(e) => {
                    anyhow::bail!("PduHint find_size error: {e}");
                }
            }
        }
    }

    /// Read the next complete RDP PDU from the WebSocket stream.
    /// Used during the active session phase (after connection).
    ///
    /// Determines Action (FastPath vs X224) from the first byte and
    /// reads the full PDU length from the wire format header.
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
                u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize
            }
            Action::FastPath => {
                // FastPath: byte 0 = header, byte 1 = length
                // If bit 7 of byte 1 is set, length is 2 bytes
                let len_byte = self.buf[1];
                if len_byte & 0x80 != 0 {
                    while self.buf.len() < 3 {
                        self.fill_buf().await?;
                    }
                    (((len_byte & 0x7F) as usize) << 8) | (self.buf[2] as usize)
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

        let payload = self.buf.split_to(pdu_length).to_vec();
        Ok((action, payload))
    }

    /// Read a text message from the WebSocket (used for proxy commands).
    /// Skips any binary messages that may arrive and buffers them.
    pub async fn read_text_message(&mut self) -> anyhow::Result<String> {
        loop {
            match self.ws_read.next().await {
                Some(Ok(Message::Text(text))) => {
                    return Ok(text);
                }
                Some(Ok(Message::Bytes(data))) => {
                    // Buffer binary data that arrives while we wait for text
                    self.buf.extend_from_slice(&data);
                }
                Some(Err(e)) => {
                    anyhow::bail!("WebSocket read error: {e}");
                }
                None => {
                    anyhow::bail!("WebSocket closed while waiting for text message");
                }
            }
        }
    }

    /// Read a CredSSP TSRequest response from the server.
    /// TSRequest is BER-encoded: starts with a SEQUENCE tag (0x30)
    /// followed by the length. We read the length then the full payload.
    pub async fn read_credssp_response(&mut self) -> anyhow::Result<Vec<u8>> {
        // Need at least 2 bytes to determine the TSRequest length
        while self.buf.len() < 2 {
            self.fill_buf().await?;
        }

        // BER SEQUENCE tag
        if self.buf[0] != 0x30 {
            anyhow::bail!(
                "Expected BER SEQUENCE tag (0x30), got 0x{:02x}",
                self.buf[0]
            );
        }

        // Parse BER length
        let (header_len, payload_len) = if self.buf[1] & 0x80 == 0 {
            // Short form: length < 128
            (2, self.buf[1] as usize)
        } else {
            // Long form: byte 1 indicates how many bytes encode the length
            let num_len_bytes = (self.buf[1] & 0x7F) as usize;
            while self.buf.len() < 2 + num_len_bytes {
                self.fill_buf().await?;
            }
            let mut len: usize = 0;
            for i in 0..num_len_bytes {
                len = (len << 8) | (self.buf[2 + i] as usize);
            }
            (2 + num_len_bytes, len)
        };

        let total_len = header_len + payload_len;
        while self.buf.len() < total_len {
            self.fill_buf().await?;
        }

        Ok(self.buf.split_to(total_len).to_vec())
    }
}

