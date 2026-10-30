use std::{collections::VecDeque, fs, path::Path, sync::{Arc, Condvar, Mutex, atomic::{AtomicU64, Ordering}, mpsc::{self, Receiver}}, thread::{self, JoinHandle}, time::{Instant, SystemTime}};
use anyhow::{Result, Context, ensure};
use crate::{dataset::Dataset, graph::{self, Evaluator}, kernels::Cancellation, model::*, netcdf_io, render};

pub enum Output {
    Field {
        info: DatasetInfo, slice: Slice, source_slice: Slice, source_variable: String,
        results: Vec<ResultVariable>, source: Arc<Rendered>, rendered: Arc<Rendered>,
        series: Option<Series>,
    },
    Exported(String),
}

pub struct Completion {
    pub generation: u64,
    pub result: Result<Output>,
    pub elapsed_ms: f64,
}

pub struct Engine {
    path: String,
    stamp: Option<(u64, SystemTime)>,
    dataset: Option<Dataset>,
    evaluator: Evaluator,
    script: String,
    scene: Option<Scene>,
    source_cache: Option<(String, Arc<Rendered>)>,
}

pub fn is_field(path: &str) -> bool {
    Path::new(path).extension().and_then(|e| e.to_str()).is_some_and(|e| ["nc", "nc4", "cdf", "mat"].contains(&e.to_ascii_lowercase().as_str()))
}

impl Default for Engine {
    fn default() -> Self { Self { path: String::new(), stamp: None, dataset: None, evaluator: Evaluator::default(), script: String::new(), scene: None, source_cache: None } }
}

impl Engine {
    pub fn compute(&mut self, project: &Project, destination: Option<&str>, cancel: &Cancellation) -> Result<Output> {
        ensure!(project.version == 1, "Unsupported project version {}", project.version);
        ensure!(!project.path.trim().is_empty(), "Enter a data file path");
        cancel.check()?;
        let stamp = fs::metadata(&project.path).ok().and_then(|m| Some((m.len(), m.modified().ok()?)));
        ensure!(is_field(&project.path), "{} is not a supported data file (.nc, .nc4, .cdf, .mat)", project.path);
        if self.path != project.path || self.stamp != stamp {
            let dataset = Dataset::open(&project.path).with_context(|| format!("Open {}", project.path))?;
            self.dataset = Some(dataset);
            self.path = project.path.clone();
            self.stamp = stamp;
            self.evaluator.clear();
            self.script.clear();
            self.scene = None;
            self.source_cache = None;
        }
        cancel.check()?;
        let Some(data) = &self.dataset else { anyhow::bail!("NetCDF dataset is not open") };
        {
            let variable = if project.source_variable.is_empty() {
                data.info().variables.first().context("File contains no numeric field with two spatial dimensions")?
            } else {
                data.info().variables.iter().find(|v| v.name == project.source_variable)
                    .with_context(|| format!("Unknown source variable {}", project.source_variable))?
            };
            let slice = if project.slice.x.is_empty() || project.slice.y.is_empty() {
                let mut slice = data.default_slice(&variable.name)?;
                slice.indices.extend(project.slice.indices.clone());
                slice
            } else { project.slice.clone() };
            let script = if let Some(graph) = &project.graph { serde_json::to_string(graph)? }
                else if project.script.trim().is_empty() { graph::default_script(&variable.name) } else { project.script.clone() };
            if self.script != script || self.scene.is_none() {
                let scene = match &project.graph { Some(graph) => graph.compile()?, None => graph::compile(&script)? };
                self.scene = Some(scene);
                self.script = script;
            }
            let original_scene = self.scene.as_ref().unwrap();
            let results = original_scene.outputs.iter().map(|output| ResultVariable {
                name: output.name.clone(), vector: matches!(output.field, FieldNode::Vector(_)),
            }).collect();
            let mut scene = original_scene.clone();
            if !project.result_variable.is_empty() {
                let output = original_scene.outputs.iter().find(|output| output.name == project.result_variable)
                    .with_context(|| format!("Unknown result variable {}", project.result_variable))?;
                let colormap = scene.layers[0].colormap;
                scene.layers.clear();
                scene.arrows.clear();
                let field = match &output.field {
                    FieldNode::Scalar(field) => field.clone(),
                    FieldNode::Vector(field) => {
                        scene.arrows.push(Arrows { field: field.clone(), stride: 8, scale: 1.0 });
                        Arc::new(ScalarNode::Magnitude { input: field.clone() })
                    },
                };
                scene.layers.push(Layer { field, colormap, opacity: 1.0, range: None });
            }
            if let Some(range) = project.result_range { scene.layers[0].range = Some(range); }
            let (layers, arrows) = self.evaluator.evaluate(&scene, data, &slice, cancel)?;
            cancel.check()?;
            if let Some(path) = destination {
                let ext = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
                if ["nc", "nc4", "cdf"].contains(&ext.as_str()) {
                    netcdf_io::write_grid(path, &layers[0].1)?;
                } else {
                    ensure!(ext == "png", "Field export requires .png or .nc");
                    let rendered = render::render(&layers, &arrows)?;
                    cancel.check()?;
                    render::write_png(path, &rendered)?;
                }
                return Ok(Output::Exported(path.to_owned()));
            }
            let rendered = Arc::new(render::render(&layers, &arrows)?);
            cancel.check()?;
            let source_slice = project.source_slice.as_ref().unwrap_or(&slice).clone();
            let source_key = serde_json::to_string(&(&variable.name, &source_slice, project.source_colormap, project.source_range))?;
            let source = if let Some((key, image)) = &self.source_cache {
                if key == &source_key { Some(image.clone()) } else { None }
            } else { None };
            let source = match source {
                Some(image) => image,
                None => {
                    let node = Arc::new(ScalarNode::Source { variable: variable.name.clone() });
                    let grid = self.evaluator.scalar(&node, data, &source_slice, cancel)?;
                    let layer = Layer { field: node, colormap: project.source_colormap, opacity: 1.0, range: project.source_range };
                    let image = Arc::new(render::render(&[(layer, grid)], &[])?);
                    self.source_cache = Some((source_key, image.clone()));
                    image
                },
            };
            let series = match (project.point, data.info().variables.iter().find(|v| v.name == variable.name)) {
                (Some([x, y]), Some(field)) => {
                    let frame = field.dimensions.iter()
                        .find(|dim| dim.name != slice.x && dim.name != slice.y && dim.len > 1)
                        .map(|dim| dim.name.clone());
                    match frame {
                        Some(name) => {
                            let values = data.series(&variable.name, &slice, &name, x, y)?;
                            Some(Series { name, values })
                        },
                        None => None,
                    }
                },
                _ => None,
            };
            cancel.check()?;
            Ok(Output::Field { info: data.info().clone(), slice, source_slice, source_variable: variable.name.clone(), results, source, rendered, series })
        }
    }
}

struct Job { generation: u64, project: Project, destination: Option<String> }
#[derive(Default)]
struct Queue { preview: Option<Job>, exports: VecDeque<Job>, stopped: bool }

pub struct Worker {
    queue: Arc<(Mutex<Queue>, Condvar)>,
    epoch: Arc<AtomicU64>,
    next: AtomicU64,
    results: Receiver<Completion>,
    thread: Option<JoinHandle<()>>,
}

impl Default for Worker { fn default() -> Self { Self::new() } }

impl Worker {
    pub fn new() -> Self {
        let queue = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let epoch = Arc::new(AtomicU64::new(0));
        let (sender, results) = mpsc::channel();
        let shared_queue = queue.clone();
        let shared_epoch = epoch.clone();
        let thread = thread::spawn(move || {
            let mut engine = Engine::default();
            loop {
                let job = {
                    let (lock, ready) = &*shared_queue;
                    let mut queue = lock.lock().unwrap();
                    while !queue.stopped && queue.preview.is_none() && queue.exports.is_empty() {
                        queue = ready.wait(queue).unwrap();
                    }
                    if queue.stopped { break; }
                    queue.exports.pop_front().or_else(|| queue.preview.take()).unwrap()
                };
                let started = Instant::now();
                let export = job.destination.is_some();
                let epoch = if export { Arc::new(AtomicU64::new(job.generation)) } else { shared_epoch.clone() };
                let cancel = Cancellation::new(epoch, job.generation);
                let result = engine.compute(&job.project, job.destination.as_deref(), &cancel);
                if !export && shared_epoch.load(Ordering::Relaxed) != job.generation { continue; }
                if sender.send(Completion { generation: job.generation, result, elapsed_ms: started.elapsed().as_secs_f64() * 1000.0 }).is_err() { break; }
            }
        });
        Self { queue, epoch, next: AtomicU64::new(0), results, thread: Some(thread) }
    }
    pub fn submit(&self, project: Project) -> u64 {
        let generation = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.epoch.store(generation, Ordering::Relaxed);
        let (lock, ready) = &*self.queue;
        lock.lock().unwrap().preview = Some(Job { generation, project, destination: None });
        ready.notify_one();
        generation
    }
    pub fn export(&self, project: Project, destination: String) -> u64 {
        let generation = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (lock, ready) = &*self.queue;
        lock.lock().unwrap().exports.push_back(Job { generation, project, destination: Some(destination) });
        ready.notify_one();
        generation
    }
    pub fn try_recv(&self) -> Option<Completion> { self.results.try_recv().ok() }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.epoch.store(0, Ordering::Relaxed);
        let (lock, ready) = &*self.queue;
        lock.lock().unwrap().stopped = true;
        ready.notify_one();
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}
