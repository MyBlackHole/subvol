# bcachefs 设备模型与多设备行为

> 仅基于本地 `bcachefs-tools` 源码。
> 重点：成员查找、成员状态、目标掩码、副本选择和设备引用规则。

## 1. 成员设备与元数据查找

流程图源文件：[`./mmd/dev-01.mmd`](./mmd/dev-01.mmd)


## 2. Superblock 与成员状态

流程图源文件：[`./mmd/dev-02.mmd`](./mmd/dev-02.mmd)


## 3. 设备 I/O 引用状态机

流程图源文件：[`./mmd/dev-03.mmd`](./mmd/dev-03.mmd)


流程图源文件：[`./mmd/dev-04.mmd`](./mmd/dev-04.mmd)


## 4. 副本放置与选择

流程图源文件：[`./mmd/dev-05.mmd`](./mmd/dev-05.mmd)


流程图源文件：[`./mmd/dev-06.mmd`](./mmd/dev-06.mmd)


## 5. 参考锚点

- 超级块成员：`fs/sb/members.c`
- 设备引用：`fs/sb/members.h`
- 读设备选择：`fs/data/extents.c`、`fs/data/read.c`
- 写设备选择：`fs/alloc/disk_groups.h`、`fs/data/write.c`
- B树节点扫描：`fs/btree/node_scan.c`
- 恢复阶段：`fs/init/passes.c`、`fs/init/recovery.c`
