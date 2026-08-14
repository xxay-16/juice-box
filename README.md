# Juicebox JDK 25 优化版

> Rust/wgpu 新架构正在 `rust-wgpu-rearchitecture` 分支推进。它已经提供 raw observed
> 动态热图、NONE/KR/VC/VC_SQRT、Observed/Expected/OE、Assembly 重排/debris/分组编辑
> 和无需 JDK 的 Windows portable 单 EXE。构建：
> `.\tools\build-rust-portable.ps1`；使用与功能边界见
> [`docs/RUST-PORTABLE.md`](docs/RUST-PORTABLE.md)。Pearson、Control、旧 session 和
> 完整 Java UI 尚未迁移，科研工作仍以 JDK 25 主版本为完整功能基准。

这是一个基于 [Aiden Lab Juicebox](https://github.com/aidenlab/Juicebox) 的桌面端优化版本，主要面向大体积 `.hic` 数据浏览和 Genome Assembly 编辑场景。

本分支已迁移到 JDK 25，并针对文件读取、Assembly 坐标映射、热图瓦片生成、拖动交互和后台缩略图计算进行了优化。

> 本项目不是 Aiden Lab 的官方发行版。如需官方版本、使用手册或科研引用信息，请访问上游项目和官方文档。

## 主要改动

- 使用 JDK 25 编译和运行。
- Block 读取默认支持 16 路并发。
- 复用解压器、压缩数据缓冲区和解压缓冲区，减少重复分配。
- 使用持久化 `FileChannel` 和位置读取，降低随机访问开销。
- 优化 Block 二进制解析，减少临时对象和流包装。
- 优化 Assembly Block 选择算法，避免 scaffold 笛卡尔积带来的大量重复计算。
- 缓存 Assembly 原始 Block，避免编辑后反复读取和解压同一数据。
- 热图瓦片在后台异步生成，完成一块立即显示一块。
- 可见瓦片优先于后台预加载瓦片。
- 预加载可见区域周围两圈瓦片，改善短距离平移体验。
- 热图瓦片缓存容量提升到 128。
- 高频拖动事件合并到约 60 FPS，降低 Swing 事件队列压力。
- 缩略图异步生成；Assembly 编辑期间暂停缩略图计算，避免与主视图争抢资源。
- 加载遮罩背景透明，加载期间不会额外将界面染灰。

## 系统要求

### 必需环境

- 64 位 Windows、Linux 或 macOS。
- JDK 25。JRE 版本必须与编译目标兼容。
- 从源码编译时需要 Apache Ant 1.10.15 或更高版本。

### 建议硬件

- 16 GB 或更多内存。
- SSD 或 NVMe 固态硬盘。
- 多核 CPU。

大型 `.hic` 文件和复杂 Assembly 工程可能需要 32 GB 或更多内存。

## 获取源码

```bash
git clone https://github.com/xxay-16/juice-box.git
cd juice-box
```

## 从源码编译

确认当前使用的是 JDK 25：

```bash
java -version
javac -version
ant -version
```

在项目根目录，也就是包含 `build.xml` 的目录中执行：

```bash
ant clean all
```

构建成功后，桌面端 JAR 位于：

```text
out/artifacts/Juicebox_jar/Juicebox.jar
```

只需要生成 Juicebox 桌面端 JAR 时也可以执行：

```bash
ant clean "artifact.juicebox:jar"
```

该目标的直接输出位于：

```text
__artifacts_temp/Juicebox_jar/Juicebox.jar
```

`__artifacts_temp` 是临时构建目录；正式构建或发布建议使用 `ant clean all`。

## 启动程序

### 通用配置

适用于一般数据浏览：

```bash
java -Xms2g -Xmx8g \
  -Djuicebox.blockReadThreads=16 \
  -jar out/artifacts/Juicebox_jar/Juicebox.jar
```

Windows PowerShell：

```powershell
java -Xms2g -Xmx8g `
  -Djuicebox.blockReadThreads=16 `
  -jar .\out\artifacts\Juicebox_jar\Juicebox.jar
```

### 大内存工作站配置

下面的配置适用于约 64 GB 内存的机器和大型 Assembly 工程：

```powershell
java -Xms4g -Xmx42g `
  -XX:+UseZGC `
  -XX:ZUncommitDelay=60 `
  -XX:ReservedCodeCacheSize=768m `
  -Djuicebox.blockReadThreads=16 `
  -Djava.awt.headless=false `
  -jar .\out\artifacts\Juicebox_jar\Juicebox.jar
```

不要把 `-Xmx` 设置为接近全部物理内存。操作系统、显存映射、线程栈、直接缓冲区和 JVM 自身仍需要本机内存。

## 使用方法

1. 启动 Juicebox。
2. 通过 `File` 菜单打开本地 `.hic` 文件或远程 URL。
3. 使用顶部染色体、显示模式、Normalization、Resolution 和 Color Range 控件调整视图。
4. Genome Assembly 工作流可从 `Assembly` 菜单进入。
5. 导入 `.assembly` 或 modified assembly 后，可选择 scaffold、移动位置、插入到其他位置或旋转方向。

Assembly Tools 的完整使用方法请参考：

- [Juicebox Assembly Tools](https://aidenlab.org/assembly/)
- [Juicebox Wiki](https://github.com/aidenlab/Juicebox/wiki)

## 性能参数

### Block 读取并发

默认值为 16：

```text
-Djuicebox.blockReadThreads=16
```

机械硬盘或低核心数 CPU 可以尝试 4–8；高速 NVMe 和高核心数 CPU 可以使用 16。继续提高不一定更快，过高的并发可能增加磁盘争用和内存压力。

### JVM 堆内存

- `-Xms`：JVM 初始堆大小。
- `-Xmx`：JVM 最大堆大小。
- 普通使用可以从 `-Xmx8g` 开始。
- 大型 Assembly 工程可以根据物理内存提高到 16–42 GB。

### 瓦片缓存和预加载

当前版本缓存最多 128 个热图瓦片，并预加载可见区域周围两圈。可见瓦片使用高优先级，预加载任务使用低优先级；当预加载瓦片进入视野时，其任务会提升为可见优先级。

## 日志与故障排查

### 启动后没有界面

确认使用的是 JDK 25，并检查：

```bash
java -version
```

如果通过远程终端运行 Linux，还需要可用的图形桌面或 X11/Wayland 环境。

### 内存不足

常见日志：

```text
OutOfMemoryError
There is insufficient memory for the Java Runtime Environment to continue
```

第一种通常需要适当提高 `-Xmx`；第二种通常意味着初始堆或最大堆设置过大，本机已经没有足够的原生内存。

### 导入 Assembly 后瓦片较慢

首次访问区域仍需要读取、解压、解析和重映射 Block。当前版本会缓存已经读取的 Block，并预加载周围区域，因此相同区域和短距离移动通常会在后续访问时更快。性能也会受到 `.hic` 文件所在磁盘、分辨率、可见区域大小和 scaffold 数量影响。

### Color Range 修改后短暂刷新

修改颜色范围会使旧瓦片失效，并按新颜色重新生成。这是预期行为。过期任务会从等待队列中清理，新瓦片会逐块显示。

## IntelliJ IDEA

1. 使用 IntelliJ IDEA 打开项目根目录。
2. 将 Project SDK 和 Language Level 设置为 JDK 25。
3. 主程序类设置为 `juicebox.MainWindow`。
4. 命令行工具主类为 `juicebox.tools.HiCTools`。
5. 确保资源文件包含 `.properties`、`.xml`、`.png`、`.txt`、`.sizes` 和 `.cu`。

推荐的 GUI VM Options：

```text
-Xms2g
-Xmx8g
-Djuicebox.blockReadThreads=16
-Djava.awt.headless=false
```

## 项目来源与许可

本项目基于 Aiden Lab 的 Juicebox 源码进行修改：

- 上游仓库：[aidenlab/Juicebox](https://github.com/aidenlab/Juicebox)
- 官方网站：[aidenlab.org/juicebox](https://aidenlab.org/juicebox/)
- Assembly Tools：[aidenlab.org/assembly](https://aidenlab.org/assembly/)

Juicebox 最初由 Jim Robinson、Neva C. Durand 和 Erez Lieberman Aiden 等开发者创建，并由 Aiden Lab 及社区持续维护。完整贡献者信息请参阅上游仓库。

本项目沿用原项目的 MIT License，详见 [LICENSE](LICENSE)。

## 科研引用

如果本软件用于科研工作，请按照 Aiden Lab 官方文档和上游 Juicebox 项目提供的信息引用对应论文，并根据实际使用的 `.hic` 数据和 Assembly 工作流补充相关数据来源。
