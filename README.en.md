# tern-netcdf

NetCDF field visualization and node-graph processing plugin for Tern. Computation runs in an embedded Rust binary, with visual controls and layouts rendered via the Tern Surface Protocol (TSP).

[English](README.en.md) | [中文](README.md)

![tern-netcdf Node Graph & Blending Preview](assets/preview.png)

```mermaid
flowchart LR
    L["window.luau"] -->|"Open file, spawn pane"| B["bin/tern-netcdf"]
    B -->|"TSP Surface"| T["Tern"]
    B -->|"Graph & Slices"| W["Worker Thread"]
    W --> N["netCDF-C Static Lib"]
    W --> R["Rayon Kernels & PNG"]
    W -->|"Source & Composite Images"| B
```

## Features

- **Visual Node Graph**: Build processing pipelines without code. Connect nodes by selecting output (`Output →`) and input ports (`A ←`, `B ←`).
- **Operators**: Includes `Gaussian blur`, `Mean convolution`, `Difference (A − B)`, `Addition (A + B)`, `Scale & offset`, `Filter · Range [min, max]`, `Median filter`, `2D FFT · Spectrum`, `2D IFFT · Inverse`, `Gradient`, `Vector (U + V)`, `Magnitude`, and `t+n · Time shift`.
- **Multi-Variable Layer Blending**: Layer multiple variables and derived fields in a compositing stack. Each layer independently configures its visibility (`Hide` / `Show`), removal (`Remove`), colormap, opacity, and z-order.
- **Probe Grid & Time Series**: 12×12 click probe lattice over the field. Clicking any cell writes the coordinate index and retrieves the complete 1D time series across available frames.
- **Python Export**: One-click generation of standalone Python scripts based on `xarray`, `numpy`, `scipy.ndimage`, and `matplotlib`. Reproduces the exact graph topology, slices, colormaps, opacity blend, and vector arrows offline.

## Installation

Pre-built binaries statically link NetCDF-C, HDF5, and zlib; no external C libraries are required at runtime.

```sh
# Build package from source (requires Rust, C/C++ compiler, CMake)
scripts/package.sh                    # macOS / Linux
scripts/package.ps1                   # Windows
# Output: dist/tern-netcdf-<target>/ and .tar.gz / .zip

tern plugin install <directory or git URL>
tern plugin list                      # Shows tern-netcdf ready
```

## Usage

In Tern, open the command palette (`Ctrl+Shift+P` / `Cmd+Shift+P`) and execute **"NetCDF: Open data file"**, then enter a `.nc`, `.nc4`, `.cdf`, or `.mat` path. The plugin also handles file opens from the file manager, terminal commands (`tern open <file.mat>`), and Finder drag-and-drop.

### Workspace Layout

- **Source Variables (Left Rail)**: Lists variables in the dataset. Each variable card shows the variable name, wrapped dimension tags (e.g. `time: 365`, `latitude: 121`), and units. Click `+ Node` to add the variable to the graph.
- **Dual Viewports (Center)**: Left pane displays the raw source field; right pane displays the pipeline composite.
- **Layers Stack (Right Rail)**: Manage active layers. Toggle visibility, switch colormaps, adjust opacity sliders, and reorder layers (`↑` / `↓`).
- **Operator Library (Bottom Left)**: Quick-add operation nodes and access export buttons (Python, PNG, NetCDF).
- **Processing Graph (Bottom Right)**: Interactive node workspace showing connected upstream dependencies, numerical parameters (e.g. $\sigma$, scale factors), and layer controls.

### Temporal differences

The `t+n` node shifts its input by n time frames, with n=1 by default. Use − / + to select positive, zero, or negative offsets. Connect the shifted field to Difference input A and the original field to input B to compute `A(t+n) − A(t)`. Offsets count dataset frames. Frames outside the time range contain missing values. Time must be a frame dimension. Depth and other slice indices remain fixed. Scripts use `time_shift(field("uo"), 1)`, and Python exports preserve these semantics.

## Python Export

Selecting `Python` in the Export section produces a standalone script (`.py`). The script loads the source dataset using `xarray`, reproduces identical boundary mirror conditions and missing value handling with `scipy.ndimage`, computes non-uniform coordinate gradients, and renders composite figures using `matplotlib`.

## File Structure

| Path | Purpose |
| --- | --- |
| `plugin.toml` | Manifest: ID `tern-netcdf`, window entry |
| `window.luau` | Command palette, file route handlers, process launcher |
| `native/src/model.rs` | Node graph, layers, project, and field data types |
| `native/src/kernels.rs` | Rayon parallel kernels (filter, convolve, gradient, vectors) |
| `native/src/netcdf_io.rs` | NetCDF C reader, slice decoders, grid export |
| `native/src/render.rs` | Colormaps, alpha blending, quiver arrows, and PNG encoder |
| `native/src/python.rs` | Standalone Python export generator |
| `native/src/runtime.rs` | Background engine worker, debounce, and preview versions |
| `native/src/ui.rs` | TSP node interface, events, and state machine |
| `native/src/main.rs` | Entry point for CLI and interactive surface |
| `native/ui.css` | Surface layout and visual styles |
| `scripts/package.sh` | Unix build and packaging script |
| `scripts/package.ps1` | Windows PowerShell build and packaging script |
| `data/sample_ocean.nc` | Sample 3D ocean velocity field (365×121×121) |

## Supported Platforms

- macOS arm64 (`aarch64-apple-darwin`)
- macOS x86_64 (`x86_64-apple-darwin`)
- Linux x86_64 (`x86_64-unknown-linux-gnu`)
- Windows x86_64 (`x86_64-pc-windows-msvc`)
