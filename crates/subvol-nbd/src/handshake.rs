use std::collections::HashMap;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{NbdError, NbdResult};
use crate::export::NbdExport;
use crate::protocol::*;

/// 执行新版 NBD 握手并返回选中的导出
///
/// 流程：
/// 1. 发送 NBDMAGIC + IHAVEOPT
/// 2. 循环处理客户端选项，直到 NBD_OPT_GO
/// 3. 返回选中的导出 + 是否继续
pub async fn negotiate<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    exports: &HashMap<String, NbdExport>,
) -> NbdResult<NbdExport> {
    send_handshake_greeting(stream).await?;

    loop {
        let Some(opt) = read_option(stream).await? else {
            return Err(NbdError::Disconnected);
        };

        match opt.option {
            NBD_OPT_LIST => {
                // 回复所有导出名称
                for export in exports.values() {
                    let name_bytes = export.name.as_bytes();
                    let mut payload = Vec::with_capacity(2 + name_bytes.len());
                    payload.extend_from_slice(&(name_bytes.len() as u32).to_be_bytes());
                    payload.extend_from_slice(name_bytes);
                    send_option_reply(stream, NBD_OPT_LIST, NBD_REP_SERVER, &payload).await?;
                }
                send_option_reply(stream, NBD_OPT_LIST, NBD_REP_ACK, &[]).await?;
            }

            NBD_OPT_INFO | NBD_OPT_GO => {
                // payload: export name length (4) + name
                let name_len = if opt.data.len() >= 4 {
                    u32::from_be_bytes([opt.data[0], opt.data[1], opt.data[2], opt.data[3]])
                        as usize
                } else {
                    0
                };
                let name = if opt.data.len() >= 4 + name_len {
                    String::from_utf8_lossy(&opt.data[4..4 + name_len]).to_string()
                } else {
                    String::new()
                };

                let Some(export) = exports.get(&name) else {
                    let name_bytes = name.as_bytes();
                    let mut err_payload = Vec::with_capacity(4 + name_bytes.len());
                    err_payload.extend_from_slice(&(name_bytes.len() as u32).to_be_bytes());
                    err_payload.extend_from_slice(name_bytes);
                    send_option_reply(stream, opt.option, NBD_REP_ERR_UNKNOWN, &err_payload)
                        .await?;
                    if opt.option == NBD_OPT_GO {
                        return Err(NbdError::UnknownExport(name));
                    }
                    continue;
                };

                // 发送 INFO_EXPORT
                let flags = if export.is_read_only() {
                    (export.flags | NBD_FLAG_READ_ONLY) & !(NBD_FLAG_SEND_FUA | NBD_FLAG_SEND_TRIM)
                } else {
                    export.flags & !NBD_FLAG_READ_ONLY
                };
                let info_data = build_export_info(export.size(), flags, export.vol.block_size());
                send_option_reply(stream, opt.option, NBD_REP_INFO, &info_data).await?;

                // 发送 ACK
                send_option_reply(stream, opt.option, NBD_REP_ACK, &[]).await?;

                if opt.option == NBD_OPT_GO {
                    // Fixed-newstyle negotiation is followed by a 32-bit
                    // client-flags word before transmission requests begin.
                    // Consume it here so the first request header remains
                    // frame-aligned for real NBD clients.
                    let mut client_flags = [0u8; 4];
                    stream.read_exact(&mut client_flags).await?;
                    return Ok(export.clone_inner());
                }
            }

            NBD_OPT_EXPORT_NAME => {
                // 旧式导出，简单支持
                let name = String::from_utf8_lossy(&opt.data).to_string();
                let Some(export) = exports.get(&name) else {
                    return Err(NbdError::UnknownExport(name));
                };

                // Oldstyle transmission reply: export_size(8) + flags(2) +
                // 124 bytes reserved.  The reserved area is part of the
                // wire frame; omitting it leaves legacy clients misaligned
                // when they start sending transmission requests.
                let mut reply = Vec::with_capacity(8 + 2 + 124);
                reply.extend_from_slice(&export.size().to_be_bytes());
                let flags = if export.is_read_only() {
                    (export.flags | NBD_FLAG_READ_ONLY) & !(NBD_FLAG_SEND_FUA | NBD_FLAG_SEND_TRIM)
                } else {
                    export.flags & !NBD_FLAG_READ_ONLY
                };
                reply.extend_from_slice(&flags.to_be_bytes());
                reply.resize(8 + 2 + 124, 0);
                stream.write_all(&reply).await?;

                return Ok(export.clone_inner());
            }

            NBD_OPT_ABORT => {
                return Err(NbdError::Disconnected);
            }

            _ => {
                // 不支持的选项
                send_option_reply(stream, opt.option, NBD_REP_ERR_UNSUP, &[]).await?;
            }
        }
    }
}

impl NbdExport {
    pub(crate) fn clone_inner(&self) -> Self {
        NbdExport {
            name: self.name.clone(),
            flags: self.flags,
            vol: self.vol.clone(),
            subvol_id: self.subvol_id,
            capacity: self.capacity,
        }
    }
}
