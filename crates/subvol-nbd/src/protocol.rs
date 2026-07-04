use std::fmt;

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::NbdResult;

// ─── NBD Magic Constants ───

/// NBDMAGIC — 握手起始标记
pub const NBD_MAGIC: u64 = 0x4e42444d41474943;
/// IHAVEOPT — 新版握手标记
pub const IHAVEOPT: u64 = 0x49484156454f5054;
/// OPT_MAGIC — 选项协商请求标记
pub const NBD_OPT_MAGIC: u64 = 0x3e8890455656;
/// REP_MAGIC — 选项协商回复标记
pub const NBD_REP_MAGIC: u64 = 0x3e8890455656;
/// 数据传输请求标记
pub const NBD_REQUEST_MAGIC: u32 = 0x25609513;
/// 数据传输回复标记
pub const NBD_RESPONSE_MAGIC: u32 = 0x67446698;

// ─── Option Codes ───

pub const NBD_OPT_EXPORT_NAME: u32 = 1;
pub const NBD_OPT_ABORT: u32 = 2;
pub const NBD_OPT_LIST: u32 = 3;
pub const NBD_OPT_INFO: u32 = 30;
pub const NBD_OPT_GO: u32 = 31;

// ─── Option Reply Types ───

pub const NBD_REP_ACK: u32 = 1;
pub const NBD_REP_SERVER: u32 = 2;
pub const NBD_REP_INFO: u32 = 3;
pub const NBD_REP_ERR_UNSUP: u32 = 0x8000_0001;
pub const NBD_REP_ERR_POLICY: u32 = 0x8000_0002;
pub const NBD_REP_ERR_INVALID: u32 = 0x8000_0003;
pub const NBD_REP_ERR_UNKNOWN: u32 = 0x8000_0006;

// ─── Info Types (for NBD_OPT_INFO / NBD_OPT_GO) ───

pub const NBD_INFO_EXPORT: u16 = 0;

// ─── NBD Error Codes ───

pub const NBD_EIO: u32 = 5;
pub const NBD_EPERM: u32 = 1;
pub const NBD_EINVAL: u32 = 22;
pub const NBD_ENOENT: u32 = 2;
pub const NBD_ENOSPC: u32 = 28;

/// The maximum request advertised by this server and accepted on the wire.
/// Keeping this bound identical to `build_export_info` prevents a peer from
/// turning the u32 request length into an unbounded allocation.
pub const NBD_MAX_BLOCK_SIZE: u32 = 1_048_576;
pub const NBD_MAX_OPTION_SIZE: u32 = 1_048_576;

// ─── Command Types ───

pub const NBD_CMD_READ: u16 = 0;
pub const NBD_CMD_WRITE: u16 = 1;
pub const NBD_CMD_DISC: u16 = 2;
pub const NBD_CMD_TRIM: u16 = 3;
pub const NBD_CMD_FLUSH: u16 = 4;

// ─── Transmission Flags ───

pub const NBD_FLAG_HAS_FLAGS: u16 = 1 << 0;
pub const NBD_FLAG_READ_ONLY: u16 = 1 << 1;
pub const NBD_FLAG_SEND_FLUSH: u16 = 1 << 2;
pub const NBD_FLAG_SEND_FUA: u16 = 1 << 3;
pub const NBD_FLAG_ROTATIONAL: u16 = 1 << 4;
pub const NBD_FLAG_SEND_TRIM: u16 = 1 << 5;

/// Global handshake flag: the server accepts the fixed newstyle option
/// negotiation sequence.  This is distinct from transmission flags even
/// though both use the wire-level `u16` representation.
pub const NBD_FLAG_FIXED_NEWSTYLE: u16 = 1 << 0;
pub const NBD_FLAG_NO_ZEROES: u16 = 1 << 1;

/// Request flag carried in the transmission header for forced-unit-access
/// writes.  FUA requires the write to be stable before its reply is sent.
pub const NBD_CMD_FLAG_FUA: u16 = 1 << 0;

/// 新式握手起始帧：NBDMAGIC + IHAVEOPT + global flags + reserved。
///
/// The fixed-newstyle greeting is 142 bytes on the wire.  The reserved
/// bytes are not optional unless `NBD_FLAG_NO_ZEROES` was negotiated; we do
/// not advertise that flag, so keep the complete frame for compatibility
/// with standard NBD clients.
pub async fn send_handshake_greeting<S: AsyncWrite + Unpin>(stream: &mut S) -> NbdResult<()> {
    let mut buf = BytesMut::with_capacity(8 + 8 + 2 + 124);
    buf.extend_from_slice(&NBD_MAGIC.to_be_bytes());
    buf.extend_from_slice(&IHAVEOPT.to_be_bytes());
    buf.extend_from_slice(&NBD_FLAG_FIXED_NEWSTYLE.to_be_bytes());
    buf.resize(8 + 8 + 2 + 124, 0);
    stream.write_all(&buf).await?;
    Ok(())
}

/// 选项请求
#[derive(Debug)]
pub struct NbdOption {
    pub option: u32,
    pub data: Bytes,
}

/// 读取一个选项请求
pub async fn read_option<S: AsyncRead + Unpin>(stream: &mut S) -> NbdResult<Option<NbdOption>> {
    // header: magic(8) + option(4) + length(4) = 16 bytes
    let mut header = [0u8; 16];
    if stream.read_exact(&mut header).await.is_err() {
        return Ok(None);
    }

    let magic = u64::from_be_bytes([
        header[0], header[1], header[2], header[3], header[4], header[5], header[6], header[7],
    ]);
    if magic != NBD_OPT_MAGIC {
        return Ok(None);
    }

    let option = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    let length = u32::from_be_bytes([header[12], header[13], header[14], header[15]]);

    if length > NBD_MAX_OPTION_SIZE {
        return Err(crate::error::NbdError::Protocol("option payload too large"));
    }

    let mut data = BytesMut::with_capacity(length as usize);
    data.resize(length as usize, 0);
    if length > 0 {
        stream.read_exact(&mut data).await?;
    }

    Ok(Some(NbdOption {
        option,
        data: data.freeze(),
    }))
}

/// 发送选项回复
pub async fn send_option_reply<S: AsyncWrite + Unpin>(
    stream: &mut S,
    option: u32,
    reply_type: u32,
    data: &[u8],
) -> NbdResult<()> {
    let mut buf = BytesMut::with_capacity(16 + data.len());
    buf.extend_from_slice(&NBD_REP_MAGIC.to_be_bytes()); // 8 bytes
    buf.extend_from_slice(&option.to_be_bytes()); // 4 bytes
    buf.extend_from_slice(&reply_type.to_be_bytes()); // 4 bytes
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes()); // 4 bytes
    buf.extend_from_slice(data);
    stream.write_all(&buf).await?;
    Ok(())
}

/// 发送 export info 帧（NBD_INFO_EXPORT 回复部分）
pub fn build_export_info(export_size: u64, flags: u16, block_size: u32) -> Vec<u8> {
    let mut data = Vec::with_capacity(23);
    data.extend_from_slice(&NBD_INFO_EXPORT.to_be_bytes()); // info type (2 bytes)
    data.extend_from_slice(&export_size.to_be_bytes()); // export size (8 bytes)
    data.extend_from_slice(&flags.to_be_bytes()); // transmission flags (2 bytes)
    data.push(0); // extra_flags = 0
    data.extend_from_slice(&block_size.to_be_bytes()); // minimum_block_size (4 bytes)
    data.extend_from_slice(&block_size.to_be_bytes()); // preferred_block_size (4 bytes)
    data.extend_from_slice(&NBD_MAX_BLOCK_SIZE.to_be_bytes()); // maximum_block_size (4 bytes)
    data
}

/// 读取数据传输请求（28 字节头部）
#[derive(Debug, Clone)]
pub struct NbdRequest {
    pub flags: u16,
    pub r#type: u16,
    pub handle: u64,
    pub offset: u64,
    pub len: u32,
}

impl fmt::Display for NbdRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let type_str = match self.r#type {
            NBD_CMD_READ => "READ",
            NBD_CMD_WRITE => "WRITE",
            NBD_CMD_DISC => "DISC",
            NBD_CMD_TRIM => "TRIM",
            NBD_CMD_FLUSH => "FLUSH",
            t => return write!(f, "UNKNOWN({})", t),
        };
        write!(
            f,
            "{} handle={:#x} offset={} len={}",
            type_str, self.handle, self.offset, self.len
        )
    }
}

pub async fn read_request<S: AsyncRead + Unpin>(stream: &mut S) -> NbdResult<Option<NbdRequest>> {
    let mut header = [0u8; 28];
    if stream.read_exact(&mut header).await.is_err() {
        return Ok(None);
    }

    let magic = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    if magic != NBD_REQUEST_MAGIC {
        tracing::warn!("bad request magic: {:#x}", magic);
        return Ok(None);
    }

    let flags = u16::from_be_bytes([header[4], header[5]]);
    let r#type = u16::from_be_bytes([header[6], header[7]]);
    let handle = u64::from_be_bytes([
        header[8], header[9], header[10], header[11], header[12], header[13], header[14],
        header[15],
    ]);
    let offset = u64::from_be_bytes([
        header[16], header[17], header[18], header[19], header[20], header[21], header[22],
        header[23],
    ]);
    let len = u32::from_be_bytes([header[24], header[25], header[26], header[27]]);

    if r#type == NBD_CMD_WRITE && len > 0 {
        // write data follows header, handled by transfer layer
    }

    Ok(Some(NbdRequest {
        flags,
        r#type,
        handle,
        offset,
        len,
    }))
}

/// 发送数据传输回复
pub async fn send_response<S: AsyncWrite + Unpin>(
    stream: &mut S,
    handle: u64,
    error: u32,
) -> NbdResult<()> {
    let mut buf = BytesMut::with_capacity(16);
    buf.extend_from_slice(&NBD_RESPONSE_MAGIC.to_be_bytes()); // 4 bytes
    buf.extend_from_slice(&error.to_be_bytes()); // 4 bytes
    buf.extend_from_slice(&handle.to_be_bytes()); // 8 bytes
    stream.write_all(&buf).await?;
    Ok(())
}

/// 发送数据传输回复 + 数据（用于 READ）
pub async fn send_response_with_data<S: AsyncWrite + Unpin>(
    stream: &mut S,
    handle: u64,
    error: u32,
    data: &[u8],
) -> NbdResult<()> {
    send_response(stream, handle, error).await?;
    if !data.is_empty() {
        stream.write_all(data).await?;
    }
    Ok(())
}

/// 读取 write 请求的数据体
pub async fn read_write_data<S: AsyncRead + Unpin>(stream: &mut S, len: u32) -> NbdResult<Bytes> {
    if len > NBD_MAX_BLOCK_SIZE {
        return Err(crate::error::NbdError::Protocol(
            "request payload too large",
        ));
    }
    let mut buf = BytesMut::with_capacity(len as usize);
    buf.resize(len as usize, 0);
    if len > 0 {
        stream.read_exact(&mut buf).await?;
    }
    Ok(buf.freeze())
}

/// Consume an oversized write payload without allocating it, so the next NBD
/// request remains frame-aligned on the same connection.
pub async fn discard_write_data<S: AsyncRead + Unpin>(
    stream: &mut S,
    mut len: u32,
) -> NbdResult<()> {
    let mut buf = [0u8; 64 * 1024];
    while len != 0 {
        let chunk = (len as usize).min(buf.len());
        stream.read_exact(&mut buf[..chunk]).await?;
        len -= chunk as u32;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_export_info_size() {
        let info = build_export_info(1 << 30, NBD_FLAG_HAS_FLAGS | NBD_FLAG_SEND_FLUSH, 4096);
        // info type(2) + export_size(8) + flags(2) + extra(1) + min_bs(4) + pref_bs(4) + max_bs(4) = 25
        assert_eq!(info.len(), 25);
    }

    #[test]
    fn test_build_export_info_values() {
        let info = build_export_info(42, NBD_FLAG_SEND_TRIM, 4096);
        let export_size = u64::from_be_bytes([
            info[2], info[3], info[4], info[5], info[6], info[7], info[8], info[9],
        ]);
        assert_eq!(export_size, 42);
        let flags = u16::from_be_bytes([info[10], info[11]]);
        assert_eq!(flags, NBD_FLAG_SEND_TRIM);
    }

    #[test]
    fn test_request_serde_manual() {
        // 模拟一个 READ 请求的序列化
        let mut header = Vec::with_capacity(28);
        header.extend_from_slice(&NBD_REQUEST_MAGIC.to_be_bytes());
        header.extend_from_slice(&NBD_CMD_FLAG_FUA.to_be_bytes()); // flags
        header.extend_from_slice(&NBD_CMD_READ.to_be_bytes()); // type
        header.extend_from_slice(&0xABCDu64.to_be_bytes()); // handle
        header.extend_from_slice(&8192u64.to_be_bytes()); // offset
        header.extend_from_slice(&4096u32.to_be_bytes()); // len

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (mut a, mut b) = tokio::io::duplex(1024);

        rt.block_on(async {
            use tokio::io::AsyncWriteExt;
            a.write_all(&header).await.unwrap();
        });

        let req = rt
            .block_on(async { read_request(&mut b).await.unwrap() })
            .unwrap();

        assert_eq!(req.r#type, NBD_CMD_READ);
        assert_eq!(req.flags, NBD_CMD_FLAG_FUA);
        assert_eq!(req.handle, 0xABCD);
        assert_eq!(req.offset, 8192);
        assert_eq!(req.len, 4096);
    }

    #[test]
    fn test_response_format() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (mut a, mut b) = tokio::io::duplex(1024);

        rt.block_on(async {
            send_response(&mut b, 0x1234, 0).await.unwrap();
        });

        let resp = rt.block_on(async {
            let mut buf = [0u8; 16];
            a.read_exact(&mut buf).await.unwrap();
            buf
        });

        let magic = u32::from_be_bytes([resp[0], resp[1], resp[2], resp[3]]);
        let error = u32::from_be_bytes([resp[4], resp[5], resp[6], resp[7]]);
        let handle = u64::from_be_bytes([
            resp[8], resp[9], resp[10], resp[11], resp[12], resp[13], resp[14], resp[15],
        ]);

        assert_eq!(magic, NBD_RESPONSE_MAGIC);
        assert_eq!(error, 0);
        assert_eq!(handle, 0x1234);
    }

    #[test]
    fn test_response_with_data() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (mut a, mut b) = tokio::io::duplex(2048);

        let payload = b"HELLO NBD";
        rt.block_on(async {
            send_response_with_data(&mut b, 1, 0, payload)
                .await
                .unwrap();
        });

        let mut resp_buf = vec![0u8; 16 + payload.len()];
        rt.block_on(async {
            a.read_exact(&mut resp_buf).await.unwrap();
        });

        let error = u32::from_be_bytes([resp_buf[4], resp_buf[5], resp_buf[6], resp_buf[7]]);
        assert_eq!(error, 0);

        let data = &resp_buf[16..];
        assert_eq!(data, payload);
    }

    #[test]
    fn test_handshake_greeting() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (mut a, mut b) = tokio::io::duplex(1024);

        rt.block_on(async {
            send_handshake_greeting(&mut b).await.unwrap();
        });

        let mut buf = [0u8; 142];
        rt.block_on(async {
            a.read_exact(&mut buf).await.unwrap();
        });

        let magic = u64::from_be_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ]);
        let ihaveopt = u64::from_be_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]);

        assert_eq!(magic, NBD_MAGIC);
        assert_eq!(ihaveopt, IHAVEOPT);
        assert_eq!(
            u16::from_be_bytes([buf[16], buf[17]]),
            NBD_FLAG_FIXED_NEWSTYLE
        );
        assert!(buf[18..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn test_nbd_request_display() {
        let req = NbdRequest {
            flags: 0,
            r#type: NBD_CMD_READ,
            handle: 0x1,
            offset: 0,
            len: 4096,
        };
        let s = format!("{}", req);
        assert!(s.contains("READ"));
        assert!(s.contains("handle=0x1"));
    }

    #[test]
    fn test_option_reply_format() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (mut a, mut b) = tokio::io::duplex(1024);

        rt.block_on(async {
            send_option_reply(&mut b, NBD_OPT_LIST, NBD_REP_ACK, &[])
                .await
                .unwrap();
        });

        let mut buf = [0u8; 20];
        rt.block_on(async {
            a.read_exact(&mut buf).await.unwrap();
        });

        let rep_magic = u64::from_be_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ]);
        let option = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let rep_type = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);

        assert_eq!(rep_magic, NBD_REP_MAGIC);
        assert_eq!(option, NBD_OPT_LIST);
        assert_eq!(rep_type, NBD_REP_ACK);
    }

    #[test]
    fn test_invalid_request_magic() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (mut a, mut b) = tokio::io::duplex(1024);

        rt.block_on(async {
            use tokio::io::AsyncWriteExt;
            a.write_all(&[0u8; 28]).await.unwrap();
        });

        let result = rt.block_on(async { read_request(&mut b).await });

        // 错误的 magic 应该返回 None
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn test_write_payload_limit_is_rejected_before_allocation() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (_a, mut b) = tokio::io::duplex(1024);
        let result = rt.block_on(async { read_write_data(&mut b, NBD_MAX_BLOCK_SIZE + 1).await });
        assert!(matches!(
            result,
            Err(crate::error::NbdError::Protocol(
                "request payload too large"
            ))
        ));
    }
}
