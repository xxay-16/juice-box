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
- `session-core` 独立解析 Java `SavedMaps/STATE` 21 字段 XML，支持 ISO-8859-1、
  XML entity/CDATA、多 state `SelectedPath` 选择，并将 session 状态转换为无 UI launch config；
  observed/control 会分别在各自 `.hic` 染色体字典中按名称解析 matrix key 和 X/Y 轴方向，
  不要求两个文件具有相同的染色体数值索引顺序；
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
- advanced transform/subtraction 第二族：EXPLOGEO、EXPLOGCEO、OCMEVS 和 DIFF；
  OME/CME 作为 OCMEVS 的内部 observed/control source type 使用，因为当前 Java
  `HeatmapRenderer.render` 没有可作为权威基准的独立 OME/CME renderer 分支；
- advanced ratio baseline 第三族：RATIOP1/RATIOP1V2 使用带 pseudocount 的 zoom average，
  RATIO0/RATIO0V2 使用 expected distance-zero baseline，RATIO0P1/RATIO0P1V2 同时使用
  distance-zero baseline 与 pseudocount；六种模式保持 paired-contact-only 与 Java float 运算顺序；
- advanced paired O/E comparison 第四族：OERATIO/OERATIOV2、OERATIOP1/OERATIOP1V2、
  OERATIOMINUS/OERATIOMINUSP1；同染色体按 contact 对角距离读取两套 expected，跨染色体
  按 Java 行为回退到各自 zoom average（非正值使用 1），ratio 与 signed difference 共用配对栅格器；
- advanced log comparison 第五族：LOGVS 在上下三角独立使用两套稀疏 contact；
  LOGRATIO/LOGRATIOV2 与 LOGEORATIO/LOGEORATIOV2 仅使用 paired contacts，并分别复现
  Java float log-operands 与 double Math.log 中间精度；V2 继续只切换 log-ratio 色阶；
- normalization-squared 第六族：NORM2/NORM2CTRL/NORM2OBSVSCTRL 在当前 1024² viewport
  内按实际 KR/VC/VC_SQRT normalization vector 计算 `1/(nvX*nvY*distance^4)`，Assembly
  模式先将 display bin 映射回 source bin。数值保持 Java double 公式再转 float，但避免
  Java 为整条染色体分配 `double[][]` 的 O(n²) 内存问题；三种模式已进入主程序 `M` 键
  循环，若当前 normalization 为 NONE 会先切到 KR；
- visible Block 并行解压后逐块栅格化和上传，进入视口的新区域无需等待全部 Block；
- 真实 v8 数据的 raw/normalized Block、normalization vector、expected vector 和 O/E
  逐 record Java/Rust 指纹 Gate。

真实 `genome.hic` v8 的 Reader Gate 已通过，CPU raw-observed、normalization、
Observed/Expected/OE、动态 GPU viewport 和基础 Assembly 编辑垂直切片也已跑通。
Control 双数据源现已建立独立 reader、Block/normalization/expected/Pearson cache，并接入
Control、Control/ExpectedC 和 Control Pearson。标准 VS/Ratio/O/E/Pearson/Log 比较视图以及 DIFF、
EXPLOGEO/EXPLOGCEO/OCMEVS、RATIOP1/RATIO0/RATIO0P1、OERATIO、LOG comparison 与 NORM2 家族已实现。JDK 25 production raw-pixel Gate 现覆盖 43 个模式，
每模式比对 36 个有序像素。Java legacy session 现可恢复单 observed、最多一个 control、
染色体轴（含 X/Y 转置）、独立轴边界、BP resolution lock、origin/scale、已支持的 MatrixType、
normalization 与颜色范围；滚轮首次缩放后解除 saved resolution lock 并回到自动 LOD。
多 map summation、FRAG、track/annotation/loop 资源和多 state 图形选择器尚未实现。其余 Java advanced 菜单仍未实现，因此这仍不是完整迁移：高级 Assembly 多选/phase 工具和
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
cargo run -p heatmap-wgpu -- tools\fixtures\legacy-session-real-data.xml [SelectedPath]
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
