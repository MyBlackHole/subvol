# 项目目标与范围

本项目的近期目标是完成一个独立、可验证的 Rust 存储引擎核心，采用
bcachefs 风格的 btree、transaction 和 journal 设计。该核心作为 Rust
存储引擎使用，暂不追求完整 bcachefs 文件系统兼容。

- 本地 `/home/black/Documents/bcachefs-tools` 仅作为 bcachefs 语义、布局和
  边界行为的对照依据，不作为运行时依赖。
- 近期只保证单一数据格式版本，不实现旧格式迁移和多版本兼容。
- 交付重点是 btree 操作正确性、事务一致性、journal 持久化与恢复，以及
  崩溃/故障注入和属性测试验证。
- 不在近期范围内实现完整 VFS、inode、目录、xattr 或其它文件系统兼容层。

# 编码约束（bcachefs 语义对齐）

所有提交必须严格遵守以下 14 条约束，不可违抗。

| # | 约束 |
|---|------|
| 1 | **必须对比本地 bcachefs 代码** — 以 `/home/black/Documents/bcachefs-tools` 源码为唯一对照基准 |
| 2 | **必须使用本地 bcachefs 代码为唯一依据** — 禁止参考其他版本或外部文档，一切以本地实际代码为准 |
| 3 | **必须与本地 bcachefs 源码逻辑一致** — 控制流、循环次数、错误处理分支需完全照搬，不可改变执行顺序 |
| 4 | **必须与本地 bcachefs 源码 API 一致** — 函数签名、参数类型、修饰符（const/static/inline）不可擅自改动 |
| 5 | **允许重构（仅限形式调整）** — 可拆分长函数或重命名局部变量，但行为与调用顺序不得改变 |
| 6 | **不允许简化 bcachefs 原始逻辑** — 禁止删减任何边界检查、重试机制、降级路径或释放操作 |
| 7 | **必须完整保留 bcachefs 所有细节** — 结构体布局、宏常量、锁操作、偏移量等需原样保留，不可裁剪 |
| 8 | **不允许创建 bcachefs 没有的函数** — 禁止定义 bcachefs 源码中不存在的任何函数，避免引入未经上游验证的逻辑分支 |
| 9 | **单元测试超时必须一分钟内** — 防止出现死锁或性能瓶颈，确保测试效率 |
| 10 | **每次修改前必须对照 bcachefs 源码确认** — 任何代码修改（新增、重构、修复）开始前，必须先读取本地 bcachefs 对应源码并确认对齐，不得凭记忆或推测直接编码 |
| 11 | **对齐 bcachefs 无需兼容旧数据格式** — subvol 数据格式不保证向前兼容，凡与 bcachefs 对齐的代码直接照搬当前版本逻辑，无需考虑迁移路径或旧格式兼容 |
| 12 | **不允许存在自有逻辑** — 任何控制流、算法、错误处理、重试机制、降级路径等行为逻辑，均须在 bcachefs 源码中有对应实现，不得引入 bcachefs 不存在的逻辑分支 |
| 13 | **不允许存在自有结构体** — 禁止定义 bcachefs 源码中不存在的 struct、enum、union 或 typedef，所有数据结构必须在 bcachefs 中有直接对应 |
| 14 | **btree id 不必须对齐 bcachefs fs 层** — subvol 可定义自己的 btree id 编号方案和类型集合，无需与 bcachefs `BCH_BTREE_IDS()` 保持一致的编号顺序、fs 层专用 type（如 inodes/dirents/xattrs）或 trigger 关系。此条豁免约束 3/12/13 中与 btree id 相关的部分 |

# 工作流程

默认直接进行实现、测试和提交；只有用户明确要求时才使用 PDCA 流程。
本项目不因 PDCA 流程而阻塞正常开发。

如用户明确要求 PDCA，再使用 `$PDCA_HOME` 下对应流程文件，并遵循
Plan → Do → Check → Act → Archive 阶段门禁。

# 独立 Rust 实现边界

上面的 bcachefs 对齐条款用于校验语义、持久化不变量和错误处理；它们不
要求复制 bcachefs 的 C API，也不允许把 bcachefs 源码链接或 vendoring 到
本项目中。Rust API、模块边界和内部辅助结构可按本项目需要设计，但必须能
在本地 bcachefs 源码中找到对应的语义依据，并通过可重复测试验证。

- 运行时不得依赖 bcachefs-tools 或其构建产物。
- 用户态 RCU 可使用现成的 Rust `urcu`/`urcu2` 开源库；不得另行实现一套
  与之重复的 RCU 机制。
- 单一格式版本、btree/transaction/journal 核心和恢复路径优先；完整文件
  系统兼容层不属于当前交付门槛。
