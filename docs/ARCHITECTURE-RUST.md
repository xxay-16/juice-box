# Juicebox Rust / wgpu 渐进重构

## 决策状态

本分支用于验证 Rust 原生核心与 wgpu 交互渲染是否值得替代当前 Java/Swing 数据与热图管线。现有 JDK 25 程序继续作为功能、文件兼容和科学计算结果的基准。在验收门槛通过前，不删除旧实现，也不提前绑定最终 UI 框架。

## 目标边界

```text
UI adapter
  -> Viewport / Commands
  -> Tile Planner
  -> hic-core / Assembly Transform
  -> CPU Reference Rasterizer
  -> R32F Intensity Tiles
  -> wgpu Texture Cache / Shader / Compositor
```

- `hic-core`：无 UI 的 `.hic` Header、Footer、Matrix、Block 和 normalization 读取。
- `assembly-core`：版本化文档、稳定 scaffold ID、命令、撤销重做和坐标变换。
- `heatmap-core`：Viewport、TileKey、任务 generation、强度 Tile 和缓存契约。
- `heatmap-cpu`：科学结果参考、无 GPU 回退和导出。
- `heatmap-wgpu`：R32F 纹理上传、camera transform、色阶 shader 和最终合成。
- `desktop-app`：最终 UI 适配层；在 GPU 原型通过前不选择 Qt、Slint 或其他框架。

依赖必须单向。格式、Assembly 与查询模块不得依赖窗口、控件或 GPU API。

## 交互原则

- 已加载区域拖动只改变 camera transform。
- 连续缩放先复用已有纹理，跨 LOD 后后台补充精细 Tile。
- min/max、色板和透明度只改变 Shader 参数。Normalization、MatrixType、Observed/Control、Pearson 等数据语义变化仍会生成新 Tile。
- 所有后台请求携带 viewport generation；过期结果不得进入栅格化、GPU 上传或屏幕展示。
- 原始 Block、Assembly 变换结果、CPU 强度 Tile 和 GPU Texture 使用独立、按字节预算的缓存。
- Assembly 编辑按真实坐标变换范围失效；不假设移动操作永远只影响局部。

## 当前垂直切片

当前代码已经包含：

- Rust workspace；
- `.hic` v8/v9+ Header、Master Index、normalization 和 expected-value 读取；
- 框架无关的 `TileKey`、`IntensityTile` 与 `Viewport`；
- `R32Float` wgpu 纹理；
- GPU camera 平移、缩放与 shader 色阶映射；
- `.hic` v8/v9+ 的 Matrix metadata、Block index、zlib 解压和原始 contact records 读取；
- 标准 `.assembly` scaffold/layout 文本的无 UI 解析、placement 坐标查询、反转/移动与 Undo/Redo 基础；
- 标准 `.assembly` 的坐标映射、反转/移动和 Undo/Redo；
- 使用真实 `.hic` 的 `1_1` BP contacts 构建 CPU 参考强度 Tile、导出 PNG，并上传到 GPU；
- 逐分辨率比较 Java/Rust 的 Block 数、record 数、stored counts 总和与逐 record 指纹。
- 基因组坐标 viewport、自动 LOD、generation 取消、256 MiB 原始 Block LRU、两圈预取；
- 1.5 倍 overscan 纹理，覆盖区内拖动只更新 shader，不重新读取 `.hic`；
- 最多 16 路并发位置读取和解压，Windows 上复用持久文件句柄；
- Assembly source/current 双向坐标索引，顺序和方向真实接入 Block 查询与栅格化；
- scaffold 选择、翻转、移动、Undo/Redo 和 modified assembly 保存；
- 无 JDK、静态 CRT 的 Windows portable 单 EXE。
- Observed / dense Expected / O/E / Pearson，以及直接 Control / Control-OE / Control-Pearson MatrixType；observed 与 control normalization 和缓存彼此独立；
- Java 默认菜单中的 VS、Ratio/RatioV2、O/E-VS、Pearson-VS、OEV2/OECTRLV2/OEVSV2、
  LOG/LOGC/LOGEOVS；其中 V2 模式保留 O/E raw scientific values，仅由 shader 使用红蓝
  log-ratio 色阶，Log 模式按 Java float 加法与 double `Math.log` 精度生成 R32F 值；
- advanced expected/pseudocount 第一族：OEP1/OEP1V2、OECTRLP1/OECTRLP1V2、
  OEVSP1/OEVSP1V2，以及独立 LOGEO/LOGCEO；它们已进入 JDK 25 production raw-pixel
  Gate，但在完整 advanced 菜单完成前不混入标准 `M` 键循环；
- visible Block 并行解压后逐块栅格化和上传，进入视口的新区域无需等待全部 Block；
- 真实 v8 数据的 raw/normalized Block、normalization vector、expected vector 和 O/E
  逐 record Java/Rust 指纹 Gate。

真实 `genome.hic` v8 的 Reader Gate 已通过，CPU raw-observed、normalization、
Observed/Expected/OE、动态 GPU viewport 和基础 Assembly 编辑垂直切片也已跑通。
Control 双数据源现已建立独立 reader、Block/normalization/expected/Pearson cache，并接入
Control、Control/ExpectedC 和 Control Pearson。标准 VS/Ratio/O/E/Pearson/Log 比较视图已实现；difference
及 Java advanced 菜单仍未实现，因此这仍不是完整迁移：旧 session、高级 Assembly 多选/phase 工具和
跨设备验收尚未完成，不能据此替换 Java 主程序。GPU 初始化失败时已自动尝试软件/CPU
适配器，并提供 `JUICEBOX_FORCE_CPU=1` 验收开关。

## 构建与运行

```powershell
cargo test --workspace
cargo run -p hic-core --bin hic-info -- ..\data\genome.hic
cargo run -p hic-core --bin hic-matrix-info -- ..\data\genome.hic 1_1
cargo run -p assembly-core --bin assembly-info -- ..\data\genome.assembly
cargo run -p heatmap-cpu --bin hic-render-png -- ..\data\genome.hic milestone-artifacts\genome-500kb.png 1_1 500000
cargo run -p heatmap-wgpu -- ..\data\genome.hic 1_1 ..\data\genome.assembly [control.hic]
```

最后一个命令会打开 GPU 原型。左键拖动、滚轮缩放；`+/-` 调色，`A` 恢复
自动色阶，`R` 重置视图。右键选择 scaffold，再右键目标 scaffold 即移动到目标
前，Ctrl+Z/Y 撤销重做，Ctrl+S 保存 modified assembly。`N` 切换 normalization，
不传 control 文件时，`M` 切换 Observed / Expected / O/E / OEV2 / Pearson / LOG。传入第四个位置参数
`control.hic` 后，会循环 Java 默认的 18 个标准模式，包括 Control、VS、Ratio/RatioV2、
O/E/OEV2 三组 observed/control/VS、Pearson 三组和 LOG/LOGC/LOGEOVS；单数据源视图中
`N` 切换当前数据源的 normalization，比较视图中 `N` 切 observed、`Shift+N` 切 control，
标题同时显示两侧状态。

Windows portable 构建：

```powershell
.\tools\build-rust-portable.ps1
```

## 回滚

该实现位于独立分支和 `rust/` 目录，不改变 Java 运行路径。任何阶段未达到验收门槛，都可以继续发布 `jdk25-migration` 分支，或仅保留 Rust Reader/工具而放弃 UI 迁移。
