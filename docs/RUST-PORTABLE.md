# Juicebox Rust Portable

双击 `JuiceboxRust.exe`，选择本地 `.hic` 文件即可启动。不需要安装，也不需要 JDK。
如果 `.hic` 旁边存在同名 `.assembly`，程序会自动加载；也可以按 `Ctrl+O` 打开其他
Assembly。
也可以从命令行直接打开 Java 保存的 session：

```text
JuiceboxRust.exe <JuiceboxStatesForExport.xml> [SelectedPath]
```

当前会恢复单 observed、最多一个 control、染色体、BP resolution、origin/scale、已支持
MatrixType、normalization 和颜色范围。XML 包含多个 state 时可用第二参数选择；省略时加载
第一个并写入日志。Session 模式不会自动套用 `.hic` 旁边的同名 Assembly，因为 Java XML
没有保存 Assembly 路径。observed/control 的 matrix key 会在各自 `.hic` 中按染色体名称
独立解析，不要求两个文件使用相同的染色体索引顺序。multi-map、FRAG、
tracks/annotations/loops 尚未恢复。

## 操作

- 左键拖动：平移；滚轮：以鼠标位置为中心缩放。
- `+` / `-`：调整颜色上限；`A`：恢复自动颜色范围；`R`：重置视图。
- `N`：循环当前数据源的 NONE / KR / VC / VC_SQRT；`M`：无 control 时循环
  Observed / Expected / O/E / OEV2 / Pearson / LOG。通过命令行提供 control 后，循环
  Java 默认菜单的 18 个标准模式：基础 observed/control、VS、RATIO/RATIOV2、
  O/E/OEV2 的 observed/control/VS、Pearson 的 observed/control/VS，以及 LOG/LOGC/LOGEOVS；observed/control
  normalization 与缓存彼此独立。
- 右键第一次：选择 scaffold；右键第二次点另一个 scaffold：把选中项移动到目标项前，
  原位置后的 scaffold 会自动顺次补上。`I`：翻转选中 scaffold。
- `D`：在选中 scaffold 内设置 debris 起点；移动鼠标后再次按 `D`，把两点之间
  的区间拆出为独立 debris superscaffold。
- `B`：在选中 scaffold 后拆分 superscaffold；`J`：把选中 superscaffold 与下一组
  合并。
- `Ctrl+Z` / `Ctrl+Y`：撤销 / 重做 Assembly 编辑。
- `Ctrl+S`：保存为原 Assembly 旁边的 `.modified.assembly`，不会覆盖原文件。

日志位置：`%LOCALAPPDATA%\JuiceboxRust\juicebox-rust.log`。

GPU 初始化失败时，程序会自动尝试 Windows 软件/CPU 适配器。开发或验收时可设置
`JUICEBOX_FORCE_CPU=1` 强制验证该路径。

## 当前科学功能范围

当前 Rust 版已经支持 v8/v9+ Header/Matrix/Block、normalization vector、expected
vector 解码，以及 NONE/KR/VC/VC_SQRT 下的 Observed、Expected、O/E 和 Pearson。真实 v8
样本的 raw/normalized records、normalization vectors、expected vectors 和 O/E records
均已逐条与 JDK 25 Java Reader 对照。动态 LOD、Assembly 顺序/方向映射和当前编辑工具
也已接入；当前可执行顺序移动、单 scaffold 翻转、debris 提取、superscaffold 拆分/
合并、撤销/重做和 modified assembly 保存。Control 双数据源的基础 Control、
Control/ExpectedC、Control Pearson、VS、RATIO/RATIOV2、OEVS、PearsonVS、
OEV2/OECTRLV2/OEVSV2、LOG/LOGC/LOGEOVS 与 NORM2 三模式已接入，并通过 Java production renderer
有序像素 Gate；NORM2 三模式可通过 `M` 键循环选择，NONE 会自动切换为 KR。Java legacy
session 的基础视图状态已可恢复；其余未接入菜单的高级模式、session tracks/annotations
和全部 Java UI 功能
仍由 JDK 25 主版本提供；未实现模式不能用于科研结论。

开发期双数据源启动格式：

```text
JuiceboxRust.exe <observed.hic> <matrix-key> <assembly> <control.hic>
```

启动时会校验 observed/control 的矩阵染色体名称、长度和共同 BP 分辨率；不兼容会直接
给出错误，不会混合渲染。
比较视图中 `N` 切换 observed normalization，`Shift+N` 切换 control normalization；
窗口标题同时显示 `obs ... / ctrl ...`，避免两侧归一化状态不透明。
