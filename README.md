# tern-netcdf

Tern 里的 NetCDF 场查看与组合式处理插件。计算在随插件分发的 Rust 程序中进行，界面通过 Tern Surface Protocol 显示。

[English](README.en.md) | [中文](README.md)

![NetCDF 节点图与多变量混合界面](assets/preview.png)

```mermaid
flowchart LR
    L["window.luau"] -->|"打开文件, 启动窗格"| B["bin/tern-netcdf"]
    B -->|"TSP 界面"| T["Tern"]
    B -->|"脚本与切片"| W["计算线程"]
    W --> N["netCDF-C 静态库"]
    W --> R["Rayon 算子与 PNG"]
    W -->|"前后两幅图"| B
```

## 安装

每个平台一个目录，内含已编译的 `bin/tern-netcdf`。NetCDF、HDF5 与 zlib 已静态编入，运行时不需安装这些库。

```sh
# 源码构建打包（构建机需要 Rust、C/C++ 与 CMake）
scripts/package.sh                    # macOS / Linux
scripts/package.ps1                   # Windows
# 产物: dist/tern-netcdf-<target>/ 与同名 .tar.gz 或 .zip

tern plugin install <解包目录或 Git 地址>
tern plugin list                      # 显示 tern-netcdf ready
```

下载发行包时，选择与本机匹配的目标三元组，解包后用 `tern plugin install <目录>` 安装。

## 使用

命令面板执行 "NetCDF: Open data file"，输入 `.nc`、`.nc4` 或 `.cdf` 路径。插件注册了这三类扩展名的打开路由：在文件面板、Finder 拖放、`tern open` 或命令面板打开时，都会启动原生查看器，不再落到十六进制编辑器。`NetCDF: New workspace` 建立空项目，`NetCDF: Resume last project` 恢复上一个项目。

界面按可视化数据流组织：

1. **输入变量列表（左侧）**：列出数据集变量。每个变量卡片独立展示名称、维度标签（如 `time: 365`、`latitude: 121`）和单位，标签自适应换行，点击 `+ Node` 直接将该变量作为源节点添加到处理节点图中。
2. **双场对比区（中间）**：左图显示原始场，右图显示经节点图处理与图层合成后的复合场。两图均覆盖 12×12 点击探针网格与时间序列点选条。
3. **多图层混合栈（右侧）**：支持将任意节点加入图层。每个图层支持独立配置色标（Viridis、Thermal / Inferno、Diverging / RdBu_r、Gray）、透明度（0%、25%、50%、75%、100% 及 ±5% 微调）、显隐切换以及上下图层移序。多变量在像素级按 Alpha 混合呈现。
4. **操作算子库（底部左侧）**：提供 `Gaussian blur`、`Mean convolution`、`Difference (A − B)`、`Scale & offset`、`Gradient`、`Vector (U + V)` 和 `Magnitude` 算子节点，以及导出面板。
5. **节点图画布（底部右侧）**：免代码的流式节点工作台。每个节点具备清晰的输入端口（`A ←`、`B ←`）与输出端口（`Output →`），支持参数微调步进（如高斯 $\sigma$、缩放因数与偏置）与连线环路自动检测。

### Python 代码导出

在导出面板选择 `Python`，将当前节点图、图层配色与切片范围导出为完全独立的 Python 脚本（依赖 `xarray`、`scipy`、`matplotlib` 与 `netCDF4`）。导出的脚本无缝对齐原生计算语义，可离线批量复现并作为论文插图生成脚本。

图面覆盖 12×12 的透明格网：点一格就把该点的网格索引写进 x / y，并读出这一点的整条时间序列。序列显示为 64 格字符条，点任意一格跳到对应帧；播放、上一帧、下一帧与坐标输入仍然可用。协议不提供鼠标坐标，格网把点击换算成网格索引。图像按可用高度铺满画布，渲染期间保留上一帧，完成后直接替换。梯度输出保持"变量单位 / 度"，没有换算成球面距离。

## 数据边界

- NetCDF 读取支持经典格式与 NetCDF-4、分组变量和一维坐标；`_FillValue` 与 `missing_value` 处理为缺测，`valid_min` 等属性不参与裁剪。
- 图像最长边 1536 像素；更大的网格降采样显示，导出 NetCDF 保留完整精度。
- 导出 PNG 与 NetCDF 要求目标文件不存在。项目 JSON 记录路径、脚本、变量、切片与色标。
- TSP 不提供鼠标坐标、滚轮与拖拽事件；点选、缩放和平移使用格网、控件与键盘。

## 文件

| 路径 | 作用 |
| --- | --- |
| `plugin.toml` | 清单：id `tern-netcdf`，窗口入口 |
| `window.luau` | 命令面板、文件打开路由、原生程序启动 |
| `native/src/model.rs` | 项目、场、处理图与结果数据类型 |
| `native/src/graph.rs` | Rhai 场表达式、节点求值和数组缓存 |
| `native/src/kernels.rs` | Rayon 滤波、卷积、差分、梯度与向量模长 |
| `native/src/netcdf_io.rs` | NetCDF 元数据、平面读取、坐标解码和导出 |
| `native/src/render.rs` | 色标、图层合成、箭头与 PNG 编码 |
| `native/src/python.rs` | 独立 Python 可视化代码生成引擎 |
| `native/src/runtime.rs` | 持久句柄、预览版本、导出队列和前后视图 |
| `native/src/ui.rs` | TSP 界面、编辑器状态和用户操作 |
| `native/src/main.rs` | 交互界面与命令行入口 |
| `native/ui.css` | 屏幕界面的布局与控件样式 |
| `scripts/package.sh` / `package.ps1` | 本平台打包脚本 |
| `data/sample_ocean.nc` | 样例场（365×121×121） |

## 平台范围

本仓库验证的平台为 macOS arm64。Linux 与 Windows 由对应构建脚本在各自平台产出；跨平台构建需要该目标的 Rust 工具链与 C/C++ 构建环境。
