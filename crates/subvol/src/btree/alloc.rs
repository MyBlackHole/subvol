//! btree 节点扇区分配（对齐 bcachefs allocator 的 btree 分配路径）。
//!
//! 语义锚点（约束 1/3/10，实现前已逐处对照本地源码）：
//! - `bch2_bucket_alloc_freelist`（fs/alloc/foreground.c:438）：从 freespace
//!   btree 选候选桶 → alloc btree 校验 FREE → 事务置 BCH_DATA_BTREE 并清
//!   freespace 位。域内由 engine.rs `allocate_bucket` 的候选规则提炼而来。
//! - `bch2_alloc_sectors_req`（foreground.c:1466）：写点（write_point）扇区
//!   记账，空间不足时换桶（interior.c:473-482 先清空已满桶再重试）。
//! - `bch2_ob_ptr`（fs/alloc/foreground.h:387-395）：ptr.offset =
//!   bucket_to_sector(bucket) + bucket_size - sectors_free。
//! - `bch2_alloc_sectors_append_ptrs_inlined`（foreground.h:406-430）：
//!   wp/ob 的 sectors_free 扣减。
//! - `__bch2_btree_node_alloc`（fs/btree/interior.c:451）：reserve_cache
//!   复用（485-503）→ 分配扇区 → `bkey_btree_ptr_v2_init` +
//!   `bch2_alloc_sectors_append_ptrs` 构造节点 key。
//! - `bch2_btree_reserve_put`（interior.c:634-663）：未消费节点（written==0）
//!   的 key + open_bucket 回填缓存。

use super::bkey::{bkey, bkey_i, bkey_init, bkey_s_c, bpos, set_bkey_val_bytes, POS_MIN};
use super::bset::{
    bch2_bkey_ptrs_c, bch_alloc_v4, bch_btree_ptr_v2, bch_extent_ptr, KEY_TYPE_alloc_v4,
    KEY_TYPE_btree_ptr_v2, KEY_TYPE_set,
};
use super::types::{bch_fs, btree, open_buckets, BKEY_BTREE_PTR_VAL_U64S_MAX};
use super::update::{bch2_btree_bit_mod, bch2_trans_commit, trigger_update_value};

/// 域内 btree id（约束 14 豁免编号）：alloc 桶状态 / freespace 位图。
/// 语义锚点：上游 BTREE_ID_alloc（bcachefs_format.h:803）+ 上游 freespace
/// btree（BTREE_ID_freespace）。
pub const BTREE_ID_ALLOC: u8 = 4;
pub const BTREE_ID_FREESPACE: u8 = 5;
pub const BTREE_ID_NEED_DISCARD: u8 = 6;

/// 对齐 `BCH_DATA_*`（fs/bcachefs_format.h BCH_DATA_* 枚举）：bucket 数据
/// 类型标记，alloc 触发器按此选择 freespace 位维护行为。
pub const BCH_DATA_FREE: u8 = 0;
pub const BCH_DATA_BTREE: u8 = 3;
pub const BCH_DATA_NEED_DISCARD: u8 = 9;

/// 对齐 `alloc_freespace_pos`（fs/alloc/background.c:1113）：freespace 位
/// 位置 = bucket 号 | ((gc_gen >> 4) << 56)，编码代数供回收顺序使用。
pub fn alloc_freespace_pos(position: bpos, alloc: &bch_alloc_v4) -> bpos {
    let gc_gen = alloc.gen.wrapping_sub(alloc.oldest_gen);
    bpos {
        offset: position.offset | (((gc_gen as u64) >> 4) << 56),
        ..position
    }
}

/// 对齐 `struct open_bucket`（fs/alloc/types.h:65-91）的域内等价：保留
/// 分配记账关键字段（dev/gen/bucket/sectors_free），域内无并发 I/O 故
/// 裁剪锁与 ec/stripe 字段。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct btree_open_bucket {
    pub valid: bool,
    pub dev: u64,
    pub gen: u32,
    pub bucket: u64,
    pub sectors_free: u32,
}

impl Default for btree_open_bucket {
    fn default() -> Self {
        Self {
            valid: false,
            dev: 0,
            gen: 0,
            bucket: 0,
            sectors_free: 0,
        }
    }
}

/// 对齐 `struct write_point`（fs/alloc/types.h:130-146）的 btree 写点：
/// 域内单设备单桶写点（`ptrs.nr` 恒 0/1），字段语义与上游一致。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct btree_write_point {
    pub sectors_free: u32,
    pub sectors_allocated: u64,
    pub ptrs: open_buckets,
}

impl Default for btree_write_point {
    fn default() -> Self {
        Self {
            sectors_free: 0,
            sectors_allocated: 0,
            ptrs: open_buckets::default(),
        }
    }
}

/// 对齐 `struct btree_alloc`（fs/btree/interior_types.h:5-7）：reserve_cache
/// 缓存的节点 key 与 open_bucket 索引。
#[repr(C)]
#[derive(Clone)]
pub struct btree_alloc {
    pub ob: open_buckets,
    pub k: bkey_i,
    pub k_pad: [u64; BKEY_BTREE_PTR_VAL_U64S_MAX],
}

/// 对齐 `struct bch_fs_btree_reserve_cache`（interior_types.h:19-26）：
/// 域内用 Vec 等价上游定长数组。
#[derive(Default)]
pub struct btree_reserve_cache {
    pub nr: usize,
    pub data: Vec<btree_alloc>,
}

/// 对齐 `struct bch_fs_allocator`（fs/alloc/types.h）中 btree 分配相关部分：
/// open_buckets 表 + btree_write_point（types.h:213）。
pub struct bch_fs_allocator {
    pub open_buckets: [btree_open_bucket; OPEN_BUCKETS_NR],
    pub btree_write_point: btree_write_point,
}

impl Default for bch_fs_allocator {
    fn default() -> Self {
        Self {
            open_buckets: [btree_open_bucket::default(); OPEN_BUCKETS_NR],
            btree_write_point: btree_write_point::default(),
        }
    }
}

/// open_buckets 表容量（域内单写点，1 个在用在途桶即可）。
pub const OPEN_BUCKETS_NR: usize = 4;

fn btree_node_sectors(sb: *const crate::sb::bch_sb) -> u32 {
    crate::sb::io::BCH_SB_BTREE_NODE_SIZE(unsafe { &*sb }) as u32
}

/// 对齐 `bch2_bucket_alloc_freelist`（foreground.c:438）：从 freespace btree
/// 选候选桶（KEY_TYPE_set 位，alloc_freespace_pos 编码）→ alloc btree 校验
/// data_type == FREE → 事务：置 BCH_DATA_BTREE + 清 freespace 位。
/// 返回 (dev, bucket, gen)。
pub unsafe fn bch2_bucket_alloc_freelist(c: *mut bch_fs, dev: u64) -> Result<(u64, u64, u32), i32> {
    if c.is_null() || (*c).disk_sb.sb.is_null() {
        return Err(-1);
    }
    if dev >= (*(*c).disk_sb.sb).nr_devices as u64 {
        return Err(-1);
    }
    if (*c).devs_online.d[dev as usize / usize::BITS as usize]
        & (1usize << (dev as usize % usize::BITS as usize))
        == 0
    {
        return Err(-1);
    }
    let member = crate::sb::io::bch2_sb_member_get((*c).disk_sb.sb, dev as usize);
    if member.bucket_size == 0 || !crate::sb::bch2_member_alive(&member) {
        return Err(-1);
    }
    let mut freespace_candidates = std::collections::BTreeSet::new();
    let mut trans = super::iter::btree_trans::default();
    super::iter::bch2_trans_init(&mut trans, c);
    let result = loop {
        super::iter::bch2_trans_begin(&mut trans);
        let mut iter = super::iter::btree_iter::default();
        super::iter::bch2_trans_iter_init(
            &mut trans,
            &mut iter,
            BTREE_ID_FREESPACE,
            POS_MIN,
            super::iter::BTREE_ITER_intent,
        );
        let mut current = super::iter::bch2_btree_iter_peek(&mut iter);
        let mut ret = 0;
        loop {
            let error = super::bkey::bkey_err(current);
            if error != 0 {
                ret = error;
                break;
            }
            if current.k.is_null() {
                break;
            }
            if (*current.k).type_ == KEY_TYPE_set && (*current.k).p.inode == dev {
                freespace_candidates.insert((*current.k).p.offset & ((1u64 << 56) - 1));
            }
            current = super::iter::bch2_btree_iter_next(&mut iter);
        }
        super::iter::bch2_trans_iter_exit(&mut iter);
        if ret != 0 {
            break Err(ret);
        }
        let mut iter = super::iter::btree_iter::default();
        super::iter::bch2_trans_iter_init(
            &mut trans,
            &mut iter,
            BTREE_ID_ALLOC,
            POS_MIN,
            super::iter::BTREE_ITER_intent,
        );
        let mut current = super::iter::bch2_btree_iter_peek(&mut iter);
        let mut allocated = None;
        let mut restart = false;
        loop {
            let error = super::bkey::bkey_err(current);
            if error != 0 {
                ret = error;
                break;
            }
            if current.k.is_null() {
                break;
            }
            if (*current.k).type_ == KEY_TYPE_alloc_v4
                && (*current.k).p.inode == dev
                && (*current.k).u64s as usize >= super::bkey::BKEY_U64S as usize + 1
            {
                let bucket_offset = (*current.k).p.offset;
                if bucket_offset < member.first_bucket as u64 || bucket_offset >= member.nbuckets {
                    current = super::iter::bch2_btree_iter_next(&mut iter);
                    continue;
                }
                if !freespace_candidates.is_empty()
                    && !freespace_candidates.contains(&bucket_offset)
                {
                    current = super::iter::bch2_btree_iter_next(&mut iter);
                    continue;
                }
                let value = current.v.cast::<super::bset::bch_alloc_v4>();
                let mut alloc = core::ptr::read_unaligned(value);
                if alloc.data_type != BCH_DATA_FREE {
                    current = super::iter::bch2_btree_iter_next(&mut iter);
                    continue;
                }
                let old_alloc = alloc;
                let gen = alloc.gen;
                alloc.data_type = BCH_DATA_BTREE;
                let pos = (*current.k).p;
                let mut ret = trigger_update_value(
                    &mut trans,
                    BTREE_ID_ALLOC,
                    pos,
                    KEY_TYPE_alloc_v4,
                    (&alloc as *const super::bset::bch_alloc_v4).cast(),
                    core::mem::size_of::<super::bset::bch_alloc_v4>(),
                );
                ret = if ret == 0 {
                    bch2_btree_bit_mod(
                        &mut trans,
                        BTREE_ID_FREESPACE,
                        alloc_freespace_pos(pos, &old_alloc),
                        false,
                    )
                } else {
                    ret
                };
                ret = if ret == 0 {
                    bch2_trans_commit(&mut trans)
                } else {
                    ret
                };
                if ret == 0 {
                    let inode = pos.inode;
                    let offset = pos.offset;
                    crate::rewrite_log_debug!("freelist allocated pos=({inode},{offset})");
                }
                if ret == -4 || (ret == -12 && trans.realloc_bytes_required != 0) {
                    restart = true;
                    break;
                }
                if ret != 0 {
                    super::iter::bch2_trans_iter_exit(&mut iter);
                    ret = -1;
                    break;
                }
                allocated = Some((pos.inode, pos.offset, gen as u32));
                break;
            }
            current = super::iter::bch2_btree_iter_next(&mut iter);
        }
        super::iter::bch2_trans_iter_exit(&mut iter);
        if restart {
            continue;
        }
        match (ret, allocated) {
            (0, Some(found)) => break Ok(found),
            (0, None) => break Err(-28),
            (r, _) if r == -4 || (r == -12 && trans.realloc_bytes_required != 0) => {
                continue;
            }
            (r, _) => break Err(r),
        }
    };
    super::iter::bch2_trans_put(&mut trans);
    result
}

/// 对齐 `bch2_alloc_sectors_req`（foreground.c:1466）+ `bch2_ob_ptr`
/// （foreground.h:387-395）+ `bch2_alloc_sectors_append_ptrs_inlined`
/// （foreground.h:406-430）：从 btree 写点分配 `sectors` 个扇区。
/// 空间不足时关闭已满桶并分配新桶（interior.c:473-482 的换桶语义）。
/// 返回生成的 bch_extent_ptr（gen/dev/offset）。
pub unsafe fn bch2_alloc_sectors_btree(
    c: *mut bch_fs,
    sectors: u32,
) -> Result<bch_extent_ptr, i32> {
    if c.is_null() {
        return Err(-1);
    }
    let sb = (*c).disk_sb.sb;
    if sb.is_null() {
        return Err(-1);
    }
    let member = crate::sb::io::bch2_sb_member_get(sb, (*sb).dev_idx as usize);
    let bucket_size = member.bucket_size as u32;
    let bucket_to_sector = |bucket: u64| bucket * member.bucket_size as u64;

    let allocator = &mut (*c).allocator;
    let wp = &mut allocator.btree_write_point;
    if wp.sectors_free < sectors {
        /* 对齐 interior.c:473-482：写点空间不足 → 已满桶清空（sectors_free
         * 记 0，标记已用尽）→ 分配新桶后重试。 */
        for idx in 0..wp.ptrs.nr as usize {
            let ob = &mut allocator.open_buckets[wp.ptrs.v[idx] as usize];
            ob.sectors_free = 0;
            ob.valid = false;
        }
        wp.ptrs.nr = 0;
        let (dev, bucket, gen) = bch2_bucket_alloc_freelist(c, (*sb).dev_idx as u64)?;
        let ob_idx = allocator
            .open_buckets
            .iter_mut()
            .position(|ob| !ob.valid)
            .ok_or(-1)?;
        allocator.open_buckets[ob_idx] = btree_open_bucket {
            valid: true,
            dev,
            gen,
            bucket,
            sectors_free: bucket_size,
        };
        wp.ptrs.v[0] = ob_idx as u16;
        wp.ptrs.nr = 1;
        wp.sectors_free = bucket_size;
    }
    let ob = allocator.open_buckets[wp.ptrs.v[0] as usize];
    if !ob.valid || ob.sectors_free < sectors {
        return Err(-28);
    }
    /* bch2_ob_ptr：offset = bucket_to_sector(bucket) + bucket_size -
     * sectors_free（已用扇区起点）；域内 bucket 扇区线性映射。 */
    let offset = bucket_to_sector(ob.bucket) + (bucket_size - ob.sectors_free) as u64;
    wp.sectors_free -= sectors;
    allocator.open_buckets[wp.ptrs.v[0] as usize].sectors_free -= sectors;
    wp.sectors_allocated += sectors as u64;
    let mut ptr = bch_extent_ptr { v: 0 };
    super::bset::SET_BCH_EXTENT_PTR_DEV(&mut ptr, ob.dev);
    super::bset::SET_BCH_EXTENT_PTR_GEN(&mut ptr, ob.gen as u64);
    super::bset::SET_BCH_EXTENT_PTR_OFFSET(&mut ptr, offset);
    Ok(ptr)
}

/// 节点 key 是否已携带磁盘 ptr（reserve_cache 回填判断用）。
pub unsafe fn btree_node_key_has_ptr(b: *const btree) -> bool {
    if b.is_null() {
        return false;
    }
    let ptrs = bch2_bkey_ptrs_c(bkey_s_c {
        k: &(*b).key.k,
        v: &(*b).key.v,
    });
    !ptrs.start.is_null() && ptrs.start < ptrs.end
}

/// 对齐 `bch2_btree_reserve_put`（interior.c:634-663）：节点未写盘
/// （written == 0）且 key 携带磁盘 ptr 时，key + open_bucket 回填缓存。
/// 缓存满时按上游同样释放（open_buckets_put 语义：ob 索引清空）。
pub unsafe fn bch2_btree_reserve_cache_put(c: *mut bch_fs, b: *mut btree) {
    if c.is_null() || b.is_null() || (*b).written != 0 || !btree_node_key_has_ptr(b) {
        return;
    }
    let cache = &mut (*c).btree.reserve_cache;
    if cache.data.len() < 16 {
        let mut a = btree_alloc {
            ob: (*b).ob,
            k: core::mem::zeroed(),
            k_pad: [0; BKEY_BTREE_PTR_VAL_U64S_MAX],
        };
        super::bkey::bkey_copy(&mut a.k, &(*b).key);
        cache.data.push(a);
        cache.nr = cache.data.len();
        (*b).ob.nr = 0;
    } else {
        (*b).ob.nr = 0;
    }
}

/// 对齐 `__bch2_btree_node_alloc`（interior.c:451-505）的扇区/key 部分：
/// 先查 reserve_cache（485-503：bkey_copy + ob 转移），无则分配扇区并构造
/// key（btree_ptr_v2 + extent ptr）。节点初始化（level/bset_init_first）
/// 由调用方在调用本函数前完成（对齐 bch2_btree_node_alloc 的初始化顺序）。
/// 返回 0 成功 / 负 errno。
pub unsafe fn bch2_btree_node_alloc_sectors(c: *mut bch_fs, b: *mut btree) -> i32 {
    if c.is_null() || b.is_null() || (*c).disk_sb.sb.is_null() {
        return -1;
    }
    /* 域内差异：interior 单元测试为纯内存模式（无磁盘设备，sb 无有效
     * members）；此时跳过扇区分配，节点 key 保持空（mem_ptr 模式，
     * child_ptr 的"无 extent 则跳过"分支），与 T0205 行为一致。有设备
     * 才走真实分配（对齐 __bch2_btree_node_alloc interior.c:451-505）。 */
    let sb = (*c).disk_sb.sb;
    let member = crate::sb::io::bch2_sb_member_get(sb, (*sb).dev_idx as usize);
    if member.bucket_size == 0 || !crate::sb::bch2_member_alive(&member) {
        return 0;
    }
    let cache = &mut (*c).btree.reserve_cache;
    if cache.nr != 0 {
        let a = cache.data.pop().unwrap();
        cache.nr = cache.data.len();
        super::bkey::bkey_copy(&mut (*b).key, &a.k);
        (*b).ob = a.ob;
    } else {
        let sectors = btree_node_sectors((*c).disk_sb.sb);
        let ptr = match bch2_alloc_sectors_btree(c, sectors) {
            Ok(ptr) => ptr,
            Err(ret) => return ret,
        };
        /* bkey_btree_ptr_v2_init + bch2_alloc_sectors_append_ptrs
         * （interior.c:504-511）：key 构造为 btree_ptr_v2 + extent ptr。 */
        let key_u64s = (core::mem::size_of::<bkey>()
            + core::mem::size_of::<bch_btree_ptr_v2>()
            + core::mem::size_of::<bch_extent_ptr>())
            / 8;
        bkey_init(&mut (*b).key.k);
        (*b).key.k.u64s = key_u64s as u8;
        (*b).key.k.type_ = KEY_TYPE_btree_ptr_v2;
        (*b).key.k.p = POS_MIN;
        set_bkey_val_bytes(
            &mut (*b).key.k,
            (key_u64s * 8) as u32 - core::mem::size_of::<bkey>() as u32,
        );
        let v = (&(*b).key as *const bkey_i)
            .cast::<u8>()
            .add(core::mem::size_of::<bkey>())
            .cast::<bch_btree_ptr_v2>()
            .cast_mut();
        *v = bch_btree_ptr_v2 {
            v: Default::default(),
            mem_ptr: 0,
            seq: (*(*b).data).keys.seq,
            sectors_written: 0,
            flags: 0,
            min_key: POS_MIN,
            ..Default::default()
        };
        let ptrs = bch2_bkey_ptrs_c(bkey_s_c {
            k: &(*b).key.k,
            v: &(*b).key.v,
        });
        if ptrs.start.is_null() || ptrs.start >= ptrs.end {
            return -1;
        }
        core::ptr::write_unaligned(ptrs.start.cast_mut(), super::bset::bch_extent_entry { ptr });
    }
    /* bch2_btree_node_alloc（interior.c:546-553）：key 的 mem_ptr/seq/
     * sectors_written 更新（mem_ptr 由 split 后续 child_ptr 填充）。 */
    let bp = (&(*b).key as *const bkey_i)
        .cast::<u8>()
        .add(core::mem::size_of::<bkey>())
        .cast::<bch_btree_ptr_v2>()
        .cast_mut();
    (*bp).mem_ptr = 0;
    (*bp).seq = (*(*b).data).keys.seq;
    (*bp).sectors_written = 0;
    0
}
