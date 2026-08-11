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
- `.hic` v8/v9+ Header 与 Master Index 基础读取；
- 框架无关的 `TileKey`、`IntensityTile` 与 `Viewport`；
- `R32Float` wgpu 纹理；
- GPU camera 平移、缩放与 shader 色阶映射；
- 使用真实 `.hic` 元数据初始化窗口标题。

当前 GPU 画面仍使用确定性的演示强度数据，不能视为真实 `.hic` 矩阵迁移完成。下一阶段必须实现 Matrix metadata、Block index、zlib 解压和 contact records，并与 Java 输出对照。

## 构建与运行

```powershell
cargo test --workspace
cargo run -p hic-core --bin hic-info -- data/inter.hic
cargo run -p heatmap-wgpu -- data/inter.hic
```

GPU 原型中按住左键拖动，滚轮缩放。

## 回滚

该实现位于独立分支和 `rust/` 目录，不改变 Java 运行路径。任何阶段未达到验收门槛，都可以继续发布 `jdk25-migration` 分支，或仅保留 Rust Reader/工具而放弃 UI 迁移。
