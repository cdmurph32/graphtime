use wasmtime::error::Context;
use clap::Parser;
use std::sync::Arc;
use wasi_frame_buffer_wasmtime::WasiFrameBufferView;
use wasi_graphics_context_wasmtime::WasiGraphicsContextView;
use wasi_surface_wasmtime::{Surface, SurfaceDesc, WasiSurfaceView};
use wasi_webgpu_wasmtime::WasiWebGpuView;
use wasmtime::{
    component::{Component, Linker},
    Config, Engine, Store,
};

use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiView};
use wasmtime_wasi_io::IoView;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    file: String,
    /// Preopened directory mappings (guest_path::host_path or just path)
    #[arg(long = "dir", value_name = "DIR")]
    dirs: Vec<String>,
    /// Arguments to pass to the wasm component
    #[arg(last = true)]
    wasm_args: Vec<String>,
}

struct HostState {
    pub table: ResourceTable,
    pub ctx: WasiCtx,
    pub instance: Arc<wasi_webgpu_wasmtime::reexports::wgpu_core::global::Global>,
    pub main_thread_proxy: wasi_surface_wasmtime::WasiWinitEventLoopProxy,
}

impl HostState {
    fn new(
        main_thread_proxy: wasi_surface_wasmtime::WasiWinitEventLoopProxy,
        dirs: &[String],
        wasm_args: &[String],
    ) -> Self {
        Self {
            table: ResourceTable::new(),
            ctx: {
                let mut builder = WasiCtxBuilder::new();
                builder.inherit_stdio().inherit_env();
                for dir_spec in dirs {
                    let (guest, host) = if let Some((g, h)) = dir_spec.split_once("::") {
                        (g.to_string(), std::path::PathBuf::from(h))
                    } else {
                        (dir_spec.clone(), std::path::PathBuf::from(dir_spec))
                    };
                    let host_dir = wasmtime_wasi::DirPerms::all();
                    let host_file = wasmtime_wasi::FilePerms::all();
                    builder.preopened_dir(host, guest, host_dir, host_file).unwrap();
                }
                if !wasm_args.is_empty() {
                    builder.args(wasm_args);
                }
                builder.build()
            },
            instance: Arc::new(wasi_webgpu_wasmtime::reexports::wgpu_core::global::Global::new(
                "webgpu",
                wasi_webgpu_wasmtime::reexports::wgpu_types::InstanceDescriptor {
                    backends: wasi_webgpu_wasmtime::reexports::wgpu_types::Backends::all(),
                    flags: wasi_webgpu_wasmtime::reexports::wgpu_types::InstanceFlags::from_build_config(),
                    memory_budget_thresholds: wasi_webgpu_wasmtime::reexports::wgpu_types::MemoryBudgetThresholds::default(),
                    backend_options: wasi_webgpu_wasmtime::reexports::wgpu_types::BackendOptions::default(),
                    display: None,
                },
                None,
            )),
            main_thread_proxy,
        }
    }
}

impl IoView for HostState {
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        wasmtime_wasi::WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl WasiGraphicsContextView for HostState {}
impl WasiFrameBufferView for HostState {}

struct UiThreadSpawner(wasi_surface_wasmtime::WasiWinitEventLoopProxy);

impl wasi_webgpu_wasmtime::MainThreadSpawner for UiThreadSpawner {
    async fn spawn<F, T>(&self, f: F) -> T
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.0.spawn(f).await
    }
}

impl WasiWebGpuView for HostState {
    fn instance(&self) -> Arc<wasi_webgpu_wasmtime::reexports::wgpu_core::global::Global> {
        Arc::clone(&self.instance)
    }

    fn ui_thread_spawner(&self) -> Box<impl wasi_webgpu_wasmtime::MainThreadSpawner + 'static> {
        Box::new(UiThreadSpawner(self.main_thread_proxy.clone()))
    }
}

impl WasiSurfaceView for HostState {
    fn create_canvas(&self, desc: SurfaceDesc) -> Surface {
        pollster::block_on(self.main_thread_proxy.create_window(desc))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .init();

    let args = Args::parse();

    let mut config = Config::default();
    config.wasm_component_model(true);
    config.async_support(true);
    let engine = Engine::new(&config)?;
    let mut linker: Linker<HostState> = Linker::new(&engine);

    wasi_webgpu_wasmtime::add_to_linker(&mut linker)?;
    wasi_frame_buffer_wasmtime::add_to_linker(&mut linker)?;
    wasi_graphics_context_wasmtime::add_to_linker(&mut linker)?;
    wasi_surface_wasmtime::add_only_surface_to_linker(&mut linker)?;
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;

    let (main_thread_loop, main_thread_proxy) =
        wasi_surface_wasmtime::create_wasi_winit_event_loop();
    // Prepend the wasm file name as argv[0] so the component sees a proper argc/argv
    let mut full_args = vec![args.file.clone()];
    full_args.extend_from_slice(&args.wasm_args);
    let host_state = HostState::new(main_thread_proxy, &args.dirs, &full_args);

    let mut store = Store::new(&engine, host_state);

    let component =
        Component::from_file(&engine, &args.file).context("Component file not found")?;

    let command =
        wasmtime_wasi::p2::bindings::Command::instantiate_async(&mut store, &component, &linker)
            .await
            .unwrap();

    std::thread::spawn(move || {
        let result = pollster::block_on(command.wasi_cli_run().call_run(&mut store));
        let exit_code = match result.context("failed to invoke `run` function") {
            Ok(Ok(())) => 0,
            Ok(Err(())) => 1,
            Err(e) => {
                eprintln!("{e:?}");
                1
            }
        };
        std::process::exit(exit_code);
    });

    main_thread_loop.run();

    Ok(())
}
