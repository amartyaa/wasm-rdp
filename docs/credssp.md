# CredSSP / NLA implementation

When the RDP host requires NLA, the connection goes through a proxy-mediated TLS upgrade before CredSSP begins. The WASM client cannot do a TLS handshake directly (browsers don't expose raw TLS), so the relay handles the handshake and passes the server certificate back.

## Sequence

```mermaid
sequenceDiagram
    participant Browser as WASM Client
    participant Proxy as Relay Server
    participant RDP as RDP Host

    Browser->>Proxy: X.224 Connection Request (via WebSocket)
    Proxy->>RDP: X.224 Connection Request (via TCP)
    RDP->>Proxy: X.224 Connection Confirm (HYBRID)
    Proxy->>Browser: X.224 Connection Confirm

    Note over Browser: connector → EnhancedSecurityUpgrade

    Browser->>Proxy: {"cmd":"tls_upgrade"} (WS text)
    Proxy->>RDP: TLS handshake (TCP → TLS)
    RDP->>Proxy: TLS established
    Proxy->>Browser: {"cmd":"tls_ready","server_cert":"<hex>"} (WS text)

    Note over Browser: connector → CredSSP

    loop NTLM rounds (2–3)
        Browser->>Proxy: TSRequest (NTLM token, WS binary)
        Proxy->>RDP: TSRequest (via TLS)
        RDP->>Proxy: TSRequest (NTLM challenge, via TLS)
        Proxy->>Browser: TSRequest (WS binary)
    end

    Browser->>Proxy: TSRequest (final + auth_info, WS binary)
    Proxy->>RDP: TSRequest (via TLS)

    Note over Browser: connector → BasicSettingsExchange
    Note over Browser,RDP: Normal RDP session (all traffic via TLS)
```

If NLA is disabled on the target, the connector skips the TLS upgrade and CredSSP rounds entirely and goes straight to BasicSettingsExchange.

## Implementation notes

**Certificate extraction** — the relay extracts the DER-encoded server certificate from the TLS handshake and sends it as a hex string in the `tls_ready` message. The WASM client decodes it, extracts the raw SubjectPublicKeyInfo BIT STRING (matching FreeRDP's `i2d_PublicKey()` output), and uses it as the channel binding for CredSSP.

**NTLM only** — Kerberos is not supported. The `sspi::credssp::CredSspClient` is initialised in `ClientMode::Ntlm`. Kerberos network requests (`GeneratorState::Suspended`) are rejected.

**HYBRID_EX** — if HYBRID_EX was negotiated during X.224, the client reads the 4-byte `EarlyUserAuthResult` after the final TSRequest and requires it to be all-zero (access granted).

**SPN** — hardcoded to `TERMSRV/localhost`. The actual routing is handled by `--rdp-target` on the relay; the SPN does not need to match the real hostname because the server-side NLA validation uses the certificate binding, not the SPN, to verify the connection.
