use crate::recovery::RecoveryState;
use crate::types::StorageError;

/// Pass: 标记允许进入 RW（对应 bcachefs `set_may_go_rw` pass）。
///
/// # 保险模式
///
/// 设置 state.may_go_rw = true 标记 RW 过渡点。
/// recovery 完成后若 may_go_rw 未设置则报错（防止状态机卡在只读模式）。
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    // may_go_rw 保险：标记 RW 过渡完成
    // bcachefs 对齐：bch2_set_may_go_rw() → set_bit(BCH_FS_may_go_rw, &c->flags)
    state.may_go_rw = true;
    Ok(())
}
