use std::{path::PathBuf, sync::{Arc, atomic::AtomicU64}, time::Instant};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tern_netcdf::{graph, kernels::Cancellation, model::{Project, Slice}, netcdf_io::Dataset, render, runtime::{Engine, Output}};

#[derive(Parser)]
#[command(version, about = "NetCDF fields with a composable processing pipeline inside Tern")]
struct Arguments { #[command(subcommand)] command: Command }

#[derive(Subcommand)]
enum Command {
    /// Open the interactive Tern surface.
    View { path: Option<String>, #[arg(long)] project: Option<String> },
    /// Inspect native NetCDF metadata.
    Inspect { path: String },
    /// Evaluate a field graph and optionally write its preview image.
    Render {
        path: String,
        #[arg(long)] script: Option<PathBuf>,
        #[arg(long)] variable: Option<String>,
        #[arg(long, default_value = "{}")] slice: String,
        #[arg(long)] point: Option<String>,
        #[arg(long)] out: Option<String>,
    },
}

fn main() -> Result<()> {
    match Arguments::parse().command {
        Command::View { path, project } => tern_netcdf::ui::run(path, project),
        Command::Inspect { path } => {
            let dataset = Dataset::open(&path)?;
            println!("{}", serde_json::to_string_pretty(dataset.info())?);
            Ok(())
        },
        Command::Render { path, script, variable, slice, point, out } => {
            let started = Instant::now();
            let script = match script {
                Some(path) => std::fs::read_to_string(&path).with_context(|| format!("Read {}", path.display()))?,
                None => variable.map(|v| graph::default_script(&v)).unwrap_or_default(),
            };
            let value: serde_json::Value = serde_json::from_str(&slice).context("Parse --slice JSON")?;
            let slice = if value.get("x").is_some() || value.get("y").is_some() || value.get("indices").is_some() {
                serde_json::from_value::<Slice>(value)?
            } else { Slice { indices: serde_json::from_value(value)?, ..Slice::default() } };
            let point = match point {
                Some(text) => {
                    let (x, y) = text.split_once(',').context("--point takes x,y")?;
                    Some([x.trim().parse()?, y.trim().parse()?])
                },
                None => None,
            };
            let project = Project { path, script, slice, point, ..Project::default() };
            let cancel = Cancellation::new(Arc::new(AtomicU64::new(1)), 1);
            let mut engine = Engine::default();
            let output = engine.compute(&project, None, &cancel)?;
            let Output::Field { rendered, slice, series, .. } = output else { anyhow::bail!("render requires a NetCDF file"); };
            if let Some(path) = &out {
                if path.to_ascii_lowercase().ends_with(".nc") || path.to_ascii_lowercase().ends_with(".nc4") {
                    tern_netcdf::netcdf_io::write_grid(path, &rendered.grid)?;
                } else { render::write_png(path, &rendered)?; }
            }
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "width": rendered.grid.width, "height": rendered.grid.height,
                "image_width": rendered.width, "image_height": rendered.height,
                "min": rendered.min, "max": rendered.max, "valid": rendered.valid,
                "units": rendered.units, "slice": slice,
                "series": series.as_ref().map(|series| serde_json::json!({
                    "name": series.name, "frames": series.values.len(),
                    "first": series.values.iter().take(5).collect::<Vec<_>>(),
                })),
                "png_bytes": rendered.png.len(), "output": out,
                "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
            }))?);
            Ok(())
        },
    }
}
