# Rust 重构验收与 Go/No-Go

## 兼容语料

至少覆盖普通和大型 `.hic`、v8 与 v9+、Observed/Control、modified assembly、旧 session，以及当前 Java 能打开的 normalization 和 MatrixType 组合。样本文件只作为本地测试数据，不默认提交仓库。

## 正确性门槛

- Header、Master Index、Matrix metadata、Block index 与 Java Reader 输出一致。
- Contact record 的 binX、binY、counts 与 Java 结果一致。
- CPU 参考热图逐像素一致，或满足为每种 MatrixType 明确规定的浮点误差。
- NaN、Infinity、缺失值、对称矩阵、Observed/Control 缺边行为一致。
- Assembly 移动、插入、旋转、拆分与撤销重做结果一致。
- `.hic` 与 `.assembly` 保持兼容；旧 session 通过独立 schema 迁移器处理。

任一科学数值结果无法解释地不一致，均为 No-Go。

## 性能门槛

- 已缓存区域平移 p95 帧时间低于 16.7 ms。
- 拖动期间不读取 `.hic`、不解压、不重新栅格化。
- 色阶调整只更新 uniform/shader，不创建新强度 Tile。
- 过期 generation 不能覆盖当前 viewport。
- 可见中心 Tile 优先；预取根据方向和速度调整。
- CPU 内存与显存按字节预算限制，连续交互 30 分钟无持续增长。

## 稳定性与发布门槛

- Intel、AMD、NVIDIA Windows 设备至少各测试一种。
- 125%、150%、200% DPI 下输入与绘制坐标一致。
- Surface 丢失、窗口休眠/恢复和 GPU 初始化失败有恢复或 CPU 回退。
- 干净 Windows 环境无需 JDK 即可运行 portable 构建。
- CPU 后端可用于自动测试、无 GPU 回退以及图片/报告导出。

## 阶段门

1. Reader Gate：真实 `.hic` Header、Footer、Matrix、raw/normalized Block、normalization
   vector、expected vector 和 O/E record 结果对照通过。**`genome.hic` v8 的 5 档
   BP × NONE/KR/VC/VC_SQRT 已通过。**
2. CPU Gate：raw observed 的 R32F Tile 与 PNG 参考导出已建立；Observed、dense Expected、
   O/E 和 Pearson 已有单测与 Java/Rust Gate。Pearson 对真实数据的 NONE/KR/VC/VC_SQRT
   × 2.5 Mb/1 Mb 整张矩阵逐 float 位指纹一致，并已接入 GUI。Control 双 reader、
   独立 cache/normalization、Control/ExpectedC 与 Control Pearson 已通过同文件 identity
   垂直切片；首批 VS、RATIO/RATIOV2、OEVS、PEARSONVS 已通过同文件不变量 Gate、
   非对称 JDK 25 production renderer 有序像素网格 Gate（每模式 36 格 raw float bits，覆盖
   坐标、漏格和最终覆盖），以及 Java `pre` 动态生成的不同 v9 observed/control 双 reader
   Gate。该 production Gate 现覆盖 29 个模式：原 11 个标准模式，OEP1/OEP1V2、
   OECTRLP1/OECTRLP1V2、OEVSP1/OEVSP1V2、LOGEO、LOGCEO，以及 EXPLOGEO、
   EXPLOGCEO、OCMEVS、DIFF、RATIOP1/RATIOP1V2、RATIO0/RATIO0V2、
   RATIO0P1/RATIO0P1V2。OME/CME 目前只作为 OCMEVS 的内部 source type，
   因为当前 Java `HeatmapRenderer.render` 没有独立 renderer 分支；其余 Java advanced
   MatrixType 仍未通过。2026-08-16 使用 JDK 25 和真实 `genome.hic` 的完整 Gate 已输出
   `Production pixel-grid match: ... (29 modes, 36 ordered cells per mode)` 与
   `Real-data verification passed.`。
3. GPU Gate：真实数据拖动、缩放、调色、256 MiB CPU Block 预算、generation 取消、
   overscan 覆盖区零读取和动态 LOD已通过单机验证；显存预算、长时间运行和跨 GPU
   验收仍待完成。
4. Assembly Gate：顺序/方向映射、scaffold 翻转/移动、debris 提取、superscaffold
   拆分/合并、版本化失效、Undo/Redo 和 modified assembly 保存回读已通过真实数据；
   高级多选、phase 与全部 Java 操作对照仍待完成。
5. UI Gate：通过上述门槛后再决定 Qt/QML、Slint、egui 或其他 UI。
6. Release Gate：Windows 静态 CRT 单 EXE、图标、文件选择、本地日志和自动 CPU/软件
   适配器回退已通过；兼容、跨设备和长时间运行全部通过后，才考虑替换 Java 主程序。

## 本地真实数据回归

真实测试数据不提交仓库。将 `.hic` 与 `.assembly` 放到仓库同级的 `data/`
目录后，在 PowerShell 中运行：

```powershell
.\tools\verify-real-data.ps1
```

该检查会运行 Rust workspace 测试，并以 `1_1` 矩阵的每个 BP 分辨率比对
Rust 与当前 Java Reader 的 Block/contact 指纹、NONE/KR/VC/VC_SQRT normalization
vectors、expected vectors，以及每条 O/E record 的 float 指纹；随后解析 assembly。
它为当前 v8 输入提供可重复证据，但不替代 v9+ 真实语料、Control、跨 GPU、
长时间运行或高级多选/phase Assembly 编辑验收。
