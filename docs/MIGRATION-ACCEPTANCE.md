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

1. Reader Gate：真实 `.hic` Header、Footer、Matrix 和 Block 结果对照通过。
2. CPU Gate：核心矩阵类型的参考 Tile 对照通过。
3. GPU Gate：拖动、缩放、调色、显存预算和任务取消通过。
4. Assembly Gate：编辑、版本化失效和 Undo/Redo 对照通过。
5. UI Gate：通过上述门槛后再决定 Qt/QML、Slint、egui 或其他 UI。
6. Release Gate：兼容、稳定性、portable 发布和长时间运行全部通过后，才考虑替换 Java 主程序。
