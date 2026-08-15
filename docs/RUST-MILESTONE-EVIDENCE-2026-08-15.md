# Rust/wgpu milestone evidence — 2026-08-15

## Goal and acceptance scope

本里程碑目标是把早期整图 GPU 原型推进为可实际使用的动态热图和
基础 Assembly 编辑器，并提供不依赖 JDK/安装器的 Windows portable 构建。完整替代
Java Juicebox 仍要求 Control、旧 session、全部高级 UI
操作和跨设备长期运行验收；这些不在本里程碑的完成声明内。

## Automated gates

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
.\tools\verify-real-data.ps1
.\tools\build-rust-portable.ps1
```

2026-08-15 全部通过。Workspace 包括 Assembly 坐标/编辑、viewport/rasterizer、
CPU palette、Tile Engine 集成和 `.hic` v8 block parser 测试。关键集成测试：

```text
tile_engine::tests::assembly_edits_change_block_selection_and_rasterized_contacts_end_to_end ... ok
```

它覆盖 Assembly reversal/reorder -> source block selection -> contact remap -> R32F Tile
像素，并覆盖 move 后的 undo/redo。

## Java/Rust Reader Gate

真实 `data/genome.hic`, matrix `1_1`：

| BP | Blocks | Records | Fingerprint |
|---:|---:|---:|---|
| 2,500,000 | 1 | 30,813 | `8cf15c0360668dfd` |
| 1,000,000 | 1 | 189,935 | `a488cb878404dd0b` |
| 500,000 | 3 | 739,954 | `9d22ba2c2c9d4d26` |
| 250,000 | 6 | 2,604,632 | `d6e8621b62321270` |
| 100,000 | 28 | 8,931,746 | `267c7dbeca50c014` |

五档 Block 数、record 数、counts 和逐 record 指纹与 JDK 25 Java Reader 一致。真实
Assembly 为 161 scaffolds / 32 superscaffolds / 617,772,809 bp，所有 scaffold 恰好
放置一次。

此外，NONE/KR/VC/VC_SQRT × 5 档 BP 的 normalization vectors、expected vectors 和
O/E records 均与 Java 对照。O/E Gate 比较每条 record 的 binX、binY 与 Java float
ratio 指纹；Expected Gate 比较长度、finite、sum、chromosome factor、尾值语义和
double-bit fingerprint。

## Real GUI scenarios

使用 `genome.hic` 与 `genome.assembly` 的 Release EXE 实际执行：

1. 长方形 1000x820 窗口中热图保持居中正方形。
2. 小幅拖动只改变 shader transform，日志没有新 `tile displayed`。
3. 越出 1.5x overscan 后，Block 完成即流式填充；250 kb 完整 6/6 blocks 通常约
   120-160 ms，缓存命中场景无需等待磁盘读取。
4. 滚轮缩放切到 100 kb LOD；串行基线为 448.9 ms，16 路并发位置读取后为
   211.7 ms，画面持续更新。
5. 右键选中 scaffold 3，`I` 翻转后 generation 1、cache 6/6；Ctrl+Z 恢复为
   generation 2；Ctrl+Y 重做为 generation 3。
6. Ctrl+S 输出 `edit-test.modified.assembly`；重新解析仍为 161/32，长度一致。
7. 无参数启动出现 `.hic` 文件选择框；取消会安静退出，不显示致命错误。
8. `M` 切换 Expected：250 kb dense matrix 约 20.3 ms；再次切换 O/E：6/6 blocks、
   cache 6/6、约 133.6 ms。
9. `JUICEBOX_FORCE_CPU=1` 使用同一 Release EXE 启动真实数据，日志确认
   `Microsoft Basic Render Driver / Dx12 / device_type=Cpu`，窗口正常响应。
10. 真实 `genome.assembly` 对 scaffold 8 提取 debris，生成 163 scaffolds /
    33 superscaffolds，总长度仍为 617,772,809 bp；重绘、撤销、重做、保存和重新解析
    均通过。随后对 superscaffold 5 执行拆分与合并，generation 4/5 均完整显示。
11. 最新 portable EXE 依次按 `M` 进入 Expected、O/E 和 Pearson；Pearson 在 NVIDIA
    GeForce RTX 5070 Ti / Vulkan 上以 250 kb 完整显示红蓝相关热图，generation 3 的
    1024×1024 R32F 纹理上传为 4,194,304 bytes，热缓存场景约 1.45 s。
12. 同一真实 `genome.hic` 作为 observed/control 的自动化 identity Gate 完整比较
    Observed=Control、O/E=Control/ExpectedC、Pearson=Control Pearson 的 1024×1024
    float raw bits；两侧首次可见 Block 均有独立 cache miss。该测试由
    `tools/verify-real-data.ps1` 自动执行并输出 `Control identity match`。

对应开发期日志保存在本地未提交目录 `__artifacts_temp/`，正式运行日志写入：

```text
%LOCALAPPDATA%\JuiceboxRust\juicebox-rust.log
```

## Portable artifact

```text
dist/JuiceboxRust-portable/JuiceboxRust.exe
size: 6,468,096 bytes
ProductName: Juicebox Rust
FileDescription: Juicebox Rust Hi-C Viewer
```

PE dependency inspection contains only Windows system DLLs. Static CRT removed the prior
`VCRUNTIME140.dll` dependency; no Java/JDK DLL or executable is required. The EXE uses the
Windows GUI subsystem, embeds the Juicebox icon, provides file dialogs and logs fatal errors.

## Known remaining gates

- Observed-vs-Control、ratio/difference 和其他比较型 MatrixType 语义。
- 更高级 Assembly 多选、phase 与 Java UI 完整交互 parity。
- Intel/AMD/NVIDIA and 125/150/200% DPI matrix.
- 30-minute memory/performance soak.
- v9+ real corpus and old session migration.
